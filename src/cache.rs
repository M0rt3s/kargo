//! A refresh-ahead cache for values fetched from the game server.
//!
//! Many concurrent callers share one copy. When the copy goes stale it is still
//! served while exactly one background refresh runs (single flight); failed
//! refreshes back off exponentially, so a struggling server is not hammered.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::errors::{Error, Result};

/// First retry delay after a failed refresh.
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Longest retry delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

type Fetch<T> = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<T>> + Send>> + Send + Sync>;

struct State<T> {
    value: Option<(Instant, Arc<T>)>,
    refreshing: bool,
    failures: u32,
    retry_at: Option<Instant>,
    last_error: String,
}

/// Cached value with a time-to-live, refreshed in the background once stale.
pub(crate) struct SwrCache<T> {
    ttl: Duration,
    base_backoff: Duration,
    max_backoff: Duration,
    fetch: Fetch<T>,
    cold: tokio::sync::Mutex<()>,
    state: Arc<Mutex<State<T>>>,
}

impl<T: Send + Sync + 'static> SwrCache<T> {
    /// A cache that obtains values with `fetch` and keeps them for `ttl`.
    pub(crate) fn new<F, Fut>(ttl: Duration, fetch: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        Self {
            ttl,
            base_backoff: BASE_BACKOFF,
            max_backoff: MAX_BACKOFF,
            fetch: Arc::new(move || Box::pin(fetch())),
            cold: tokio::sync::Mutex::new(()),
            state: Arc::new(Mutex::new(State {
                value: None,
                refreshing: false,
                failures: 0,
                retry_at: None,
                last_error: String::new(),
            })),
        }
    }

    /// Override the retry delays (tests).
    #[cfg(test)]
    fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.base_backoff = base;
        self.max_backoff = max;
        self
    }

    /// The current value: fresh, or stale while a refresh runs. Only the very first
    /// call (no value yet) waits for the server.
    pub(crate) async fn get(&self) -> Result<Arc<T>> {
        if let Some(value) = self.cached() {
            return Ok(value);
        }
        let _first = self.cold.lock().await;
        if let Some(value) = self.cached() {
            return Ok(value);
        }
        if let Some(wait) = self.backoff_remaining() {
            return Err(self.backing_off(wait));
        }
        let outcome = (self.fetch)().await;
        Self::record(&self.state, outcome, self.base_backoff, self.max_backoff)
    }

    /// A value if there is one worth returning right now, starting a refresh when it is stale.
    fn cached(&self) -> Option<Arc<T>> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (at, value) = state.value.as_ref()?;
        let value = Arc::clone(value);
        if at.elapsed() < self.ttl {
            return Some(value);
        }
        let due = state.retry_at.is_none_or(|t| Instant::now() >= t);
        if !state.refreshing && due {
            state.refreshing = true;
            let fetch = Arc::clone(&self.fetch);
            let shared = Arc::clone(&self.state);
            let (base, max) = (self.base_backoff, self.max_backoff);
            tokio::spawn(async move {
                let outcome = fetch().await;
                let _ = Self::record(&shared, outcome, base, max);
            });
        }
        Some(value)
    }

    fn backoff_remaining(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retry_at?.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
    }

    fn backing_off(&self, wait: Duration) -> Error {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Error::Unavailable(format!("{} (retrying in {} s)", state.last_error, wait.as_secs().max(1)))
    }

    fn record(shared: &Mutex<State<T>>, outcome: Result<T>, base: Duration, max: Duration) -> Result<Arc<T>> {
        let mut state = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.refreshing = false;
        match outcome {
            Ok(value) => {
                let value = Arc::new(value);
                state.value = Some((Instant::now(), Arc::clone(&value)));
                state.failures = 0;
                state.retry_at = None;
                Ok(value)
            }
            Err(err) => {
                state.failures = state.failures.saturating_add(1);
                state.retry_at = Some(Instant::now() + backoff(state.failures, base, max));
                state.last_error = err.to_string();
                Err(err)
            }
        }
    }
}

/// Delay after `failures` consecutive failures: `base`, doubling, capped at `max`.
pub(crate) fn backoff(failures: u32, base: Duration, max: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    base.saturating_mul(1 << doublings).min(max)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn counting(
        ttl: Duration,
        delay: Duration,
        fail: Arc<std::sync::atomic::AtomicBool>,
    ) -> (SwrCache<usize>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let cache = SwrCache::new(ttl, move || {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            let fail = Arc::clone(&fail);
            async move {
                tokio::time::sleep(delay).await;
                if fail.load(Ordering::SeqCst) { Err(Error::Unavailable("down".into())) } else { Ok(n) }
            }
        });
        (cache, calls)
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let (base, cap) = (Duration::from_secs(1), Duration::from_secs(60));
        let secs: Vec<u64> = (1..=9).map(|f| backoff(f, base, cap).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(backoff(u32::MAX, base, cap), cap);
    }

    #[tokio::test]
    async fn fresh_values_are_reused_and_concurrent_cold_callers_share_one_fetch() {
        let (cache, calls) = counting(Duration::from_secs(60), Duration::from_millis(50), Arc::default());
        let cache = Arc::new(cache);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                tokio::spawn(async move { cache.get().await.map(|v| *v) })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), 1);
        }
        assert_eq!(*cache.get().await.unwrap(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_values_are_served_while_one_refresh_runs() {
        let (cache, calls) = counting(Duration::from_millis(100), Duration::from_millis(150), Arc::default());
        assert_eq!(*cache.get().await.unwrap(), 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let started = Instant::now();
        for _ in 0..5 {
            assert_eq!(*cache.get().await.unwrap(), 1, "stale value must be served");
        }
        assert!(started.elapsed() < Duration::from_millis(100), "must not wait for the refresh");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*cache.get().await.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one refresh");
    }

    #[tokio::test]
    async fn failed_refreshes_back_off_and_keep_serving_the_stale_value() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cache, calls) = counting(Duration::from_millis(50), Duration::ZERO, Arc::clone(&fail));
        let cache = cache.with_backoff(Duration::from_millis(400), Duration::from_secs(1));
        assert_eq!(*cache.get().await.unwrap(), 1);
        fail.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        for _ in 0..20 {
            assert_eq!(*cache.get().await.unwrap(), 1);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one failed refresh, then back-off");
    }

    #[tokio::test]
    async fn a_cold_failure_is_remembered_during_back_off() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (cache, calls) = counting(Duration::from_secs(60), Duration::ZERO, fail);
        assert!(cache.get().await.is_err());
        let err = cache.get().await.unwrap_err();
        assert!(err.to_string().contains("down") && err.to_string().contains("retrying"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

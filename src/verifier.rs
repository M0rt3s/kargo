//! Checking a bearer token against the game API before any MCP request is served.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

use crate::api::Client;
use crate::errors::{Error, Result};
use crate::token_hash;

/// Path of the endpoint that says whether a token is valid (and costs no game allowance).
pub(crate) const WHOAMI_PATH: &str = "/api/agent/whoami";
/// How long a "valid" verdict is trusted.
pub(crate) const POSITIVE_TTL: Duration = Duration::from_secs(60);
/// How long an "invalid" verdict is trusted.
pub(crate) const NEGATIVE_TTL: Duration = Duration::from_secs(5);
/// Verdicts kept before expired ones are swept, and the hard cap before entries are evicted.
const SWEEP_AT: usize = 1024;
const HARD_CAP: usize = 16_384;

/// One question being asked of the game API; everyone asking about the same token shares it.
type Flight = Arc<OnceCell<std::result::Result<bool, String>>>;

/// A question in flight and how many checks are waiting on it (changed only under the map lock).
struct Pending {
    flight: Flight,
    waiters: usize,
}

/// Asks the game API whether a token is acceptable, remembering the answer briefly.
///
/// Tokens are only ever kept as keyed hashes (random per process). Concurrent checks of one
/// token share a single request.
pub(crate) struct TokenVerifier {
    client: Client,
    positive: Duration,
    negative: Duration,
    sweep_at: usize,
    hard_cap: usize,
    verdicts: Mutex<HashMap<u64, (Instant, bool)>>,
    flights: Mutex<HashMap<u64, Pending>>,
}

impl TokenVerifier {
    /// A verifier that asks through `client` (which must not carry a token).
    pub(crate) fn new(client: Client) -> Self {
        Self::with_ttls(client, POSITIVE_TTL, NEGATIVE_TTL)
    }

    pub(crate) fn with_ttls(client: Client, positive: Duration, negative: Duration) -> Self {
        Self {
            client,
            positive,
            negative,
            sweep_at: SWEEP_AT,
            hard_cap: HARD_CAP,
            verdicts: Mutex::default(),
            flights: Mutex::default(),
        }
    }

    /// `Ok(true)` for a valid token, `Ok(false)` for a missing/unknown/expired one, and an
    /// error when the game API could not give an answer (nothing is remembered then).
    pub(crate) async fn check(&self, token: &str) -> Result<bool> {
        let key = token_hash::hash(Some(token));
        if let Some(verdict) = self.recall(key) {
            return Ok(verdict);
        }
        let flight = {
            let mut flights = lock(&self.flights);
            let pending =
                flights.entry(key).or_insert_with(|| Pending { flight: Flight::default(), waiters: 0 });
            pending.waiters += 1;
            Arc::clone(&pending.flight)
        };
        let _registered = Registered { flights: &self.flights, key, flight: Arc::clone(&flight) };
        let outcome = flight.get_or_init(|| self.ask(key, token)).await.clone();
        outcome.map_err(Error::Unavailable)
    }

    /// Forget what is known about `token`, for example after the game API refused it at use.
    pub(crate) fn evict(&self, token: &str) {
        lock(&self.verdicts).remove(&token_hash::hash(Some(token)));
    }

    async fn ask(&self, key: u64, token: &str) -> std::result::Result<bool, String> {
        let verdict = match self.client.with_token(Some(token.to_owned())).get(WHOAMI_PATH, &[]).await {
            Ok(_) => true,
            Err(Error::Auth(_)) => false,
            Err(other) => return Err(other.to_string()),
        };
        self.remember(key, verdict);
        Ok(verdict)
    }

    fn recall(&self, key: u64) -> Option<bool> {
        let (at, verdict) = *lock(&self.verdicts).get(&key)?;
        let ttl = if verdict { self.positive } else { self.negative };
        (at.elapsed() < ttl).then_some(verdict)
    }

    fn remember(&self, key: u64, verdict: bool) {
        let mut verdicts = lock(&self.verdicts);
        if verdicts.len() >= self.sweep_at {
            let (positive, negative) = (self.positive, self.negative);
            verdicts.retain(|_, (at, ok)| at.elapsed() < if *ok { positive } else { negative });
        }
        if verdicts.len() >= self.hard_cap && !verdicts.contains_key(&key) {
            // Rejections are the cheap thing to lose (and what a flood of junk tokens produces).
            verdicts.retain(|_, (_, ok)| *ok);
            if verdicts.len() >= self.hard_cap
                && let Some(oldest) = verdicts.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| *k)
            {
                verdicts.remove(&oldest);
            }
        }
        verdicts.insert(key, (Instant::now(), verdict));
    }
}

/// Takes a finished (or abandoned) question off the books, even if the check is cancelled.
struct Registered<'a> {
    flights: &'a Mutex<HashMap<u64, Pending>>,
    key: u64,
    flight: Flight,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let mut flights = lock(self.flights);
        let Some(pending) = flights.get_mut(&self.key).filter(|p| Arc::ptr_eq(&p.flight, &self.flight))
        else {
            return;
        };
        pending.waiters -= 1;
        // Kept only while someone else still waits for an answer that has not arrived.
        if pending.waiters == 0 || self.flight.initialized() {
            flights.remove(&self.key);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::routing::get;

    use super::*;

    async fn verifier(
        positive: Duration,
        negative: Duration,
    ) -> (TokenVerifier, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        slow_verifier(positive, negative, Duration::ZERO).await
    }

    async fn slow_verifier(
        positive: Duration,
        negative: Duration,
        delay: Duration,
    ) -> (TokenVerifier, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let hits = crate::testutil::counter();
        let seen = std::sync::Arc::clone(&hits);
        let app = Router::new().route(
            WHOAMI_PATH,
            get(move |headers: HeaderMap| {
                seen.fetch_add(1, Ordering::SeqCst);
                let good = headers.get(header::AUTHORIZATION).is_some_and(|v| v == "Bearer good");
                async move {
                    tokio::time::sleep(delay).await;
                    if good {
                        (StatusCode::OK, "{\"username\":\"u\",\"company\":true}")
                    } else {
                        (StatusCode::UNAUTHORIZED, "no")
                    }
                }
            }),
        );
        let url = crate::testutil::serve(app).await;
        let client = Client::new(&url, None, Duration::from_secs(1)).unwrap();
        (TokenVerifier::with_ttls(client, positive, negative), hits)
    }

    #[tokio::test]
    async fn verdicts_are_remembered_per_token() {
        let (verifier, hits) = verifier(Duration::from_secs(60), Duration::from_secs(60)).await;
        assert!(verifier.check("good").await.unwrap());
        assert!(verifier.check("good").await.unwrap());
        assert!(!verifier.check("bad").await.unwrap());
        assert!(!verifier.check("bad").await.unwrap());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn positive_and_negative_verdicts_expire_on_their_own_clocks() {
        let (verifier, hits) = verifier(Duration::from_secs(60), Duration::from_millis(100)).await;
        assert!(verifier.check("good").await.unwrap());
        assert!(!verifier.check("bad").await.unwrap());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(verifier.check("good").await.unwrap(), "positive verdict outlives the negative TTL");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(!verifier.check("bad").await.unwrap());
        assert_eq!(hits.load(Ordering::SeqCst), 3, "negative verdict expired and was re-asked");
    }

    #[tokio::test]
    async fn concurrent_checks_of_one_token_share_one_request() {
        let (verifier, hits) =
            slow_verifier(Duration::from_secs(60), Duration::from_secs(60), Duration::from_millis(300)).await;
        let verifier = std::sync::Arc::new(verifier);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let verifier = std::sync::Arc::clone(&verifier);
                tokio::spawn(async move { verifier.check("good").await.unwrap() })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap());
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one question for eight callers");
        assert!(verifier.flights.lock().unwrap().is_empty(), "finished flights are forgotten");
    }

    #[tokio::test]
    async fn cancelled_checks_leave_no_flights_behind() {
        let (verifier, _) =
            slow_verifier(Duration::from_secs(60), Duration::from_secs(60), Duration::from_secs(5)).await;
        let verifier = std::sync::Arc::new(verifier);
        let tasks: Vec<_> = (0..50)
            .map(|i| {
                let verifier = std::sync::Arc::clone(&verifier);
                tokio::spawn(async move { verifier.check(&format!("token-{i}")).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(verifier.flights.lock().unwrap().len(), 50, "all checks are in flight");
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        assert!(verifier.flights.lock().unwrap().is_empty(), "cancelled checks must not leak entries");
    }

    #[tokio::test]
    async fn evicting_a_token_makes_the_next_check_ask_again() {
        let (verifier, hits) = verifier(Duration::from_secs(60), Duration::from_secs(60)).await;
        assert!(verifier.check("good").await.unwrap());
        verifier.evict("good");
        assert!(verifier.check("good").await.unwrap());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn at_the_hard_cap_rejections_go_first_then_the_oldest_and_never_everything() {
        let (mut verifier, _) = verifier(Duration::from_secs(60), Duration::from_secs(60)).await;
        verifier.hard_cap = 4;
        verifier.sweep_at = 100;
        let known = |v: &TokenVerifier, key: u64| v.recall(key).is_some();
        let pause = || std::thread::sleep(Duration::from_millis(2));
        verifier.remember(1, true);
        pause();
        verifier.remember(2, false);
        verifier.remember(3, false);
        verifier.remember(4, true);
        verifier.remember(5, true);
        assert!(known(&verifier, 1) && known(&verifier, 4) && known(&verifier, 5), "positives survive");
        assert!(!known(&verifier, 2) && !known(&verifier, 3), "rejections are evicted first");
        pause();
        verifier.remember(6, true);
        pause();
        verifier.remember(7, true);
        assert!(!known(&verifier, 1), "with only positives left, the oldest goes");
        assert!(known(&verifier, 4) && known(&verifier, 5) && known(&verifier, 6) && known(&verifier, 7));
        assert_eq!(verifier.verdicts.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn an_unreachable_game_api_is_an_error_and_is_not_remembered() {
        let client = Client::new("http://127.0.0.1:1", None, Duration::from_secs(1)).unwrap();
        let verifier = TokenVerifier::new(client);
        assert!(matches!(verifier.check("good").await, Err(Error::Unavailable(_))));
        assert!(verifier.verdicts.lock().unwrap().is_empty());
    }
}

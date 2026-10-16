//! At most one command in flight per token, with a bounded queue behind it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;

use crate::errors::{Error, describe_duration};
use crate::token_hash;

/// Callers allowed to wait behind the one command in flight.
pub(crate) const DEFAULT_QUEUE: usize = 8;
/// Longest a caller waits for its turn before the call is refused as busy.
pub(crate) const DEFAULT_QUEUE_WAIT: Duration = Duration::from_secs(90);

#[derive(Default)]
struct Slot {
    turn: Arc<Mutex<()>>,
    waiting: AtomicUsize,
}

/// Serialises commands that share a token.
///
/// Callers holding the same token queue up rather than racing each other
/// against the server's pacing; past `max_queue` waiters the call is refused as
/// busy; so is a caller whose turn does not come within `max_wait`, and nothing has been sent
/// by then. Tokens are only ever kept as keyed hashes.
pub(crate) struct ActGate {
    slots: StdMutex<HashMap<u64, Arc<Slot>>>,
    max_queue: usize,
    max_wait: Duration,
}

impl Default for ActGate {
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE, DEFAULT_QUEUE_WAIT)
    }
}

impl ActGate {
    /// A gate that lets `max_queue` callers wait per token, each for at most `max_wait`.
    pub(crate) fn new(max_queue: usize, max_wait: Duration) -> Self {
        Self { slots: StdMutex::default(), max_queue, max_wait }
    }

    /// Wait for this token's turn, giving up at `deadline` (or after `max_wait`, whichever is
    /// first); the returned guard releases the turn on drop.
    pub(crate) async fn enter(
        &self,
        token: Option<&str>,
        deadline: Instant,
    ) -> Result<OwnedMutexGuard<()>, Error> {
        // The caller is counted as waiting before the map is unlocked, so a concurrent sweep
        // can never drop the slot it is about to wait on.
        let queued = {
            let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // A slot nobody else holds is idle: forget it so the map cannot grow without bound.
            slots.retain(|_, slot| {
                Arc::strong_count(&slot.turn) > 1 || slot.waiting.load(Ordering::SeqCst) > 0
            });
            let slot = Arc::clone(slots.entry(token_hash::hash(token)).or_default());
            if slot.waiting.fetch_add(1, Ordering::SeqCst) >= self.max_queue {
                slot.waiting.fetch_sub(1, Ordering::SeqCst);
                return Err(Error::Busy(format!(
                    "{} calls are already queued for this token; wait for them to finish",
                    self.max_queue
                )));
            }
            // Counts this caller as queued for exactly as long as it waits, even if cancelled.
            Queued(slot)
        };
        let limit = self.max_wait.min(deadline.saturating_duration_since(Instant::now()));
        let turn = Arc::clone(&queued.0.turn);
        tokio::time::timeout(limit, turn.lock_owned()).await.map_err(|_| {
            Error::Busy(format!(
                "waited {} for the previous call on this token to finish; nothing was sent",
                describe_duration(limit)
            ))
        })
    }
}

struct Queued(Arc<Slot>);

impl Drop for Queued {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// Fail a test that would otherwise hang.
    async fn bounded<T>(work: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), work).await.expect("the test hung")
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[tokio::test]
    async fn same_token_waits_different_token_does_not() {
        let gate = Arc::new(ActGate::default());
        let first = bounded(gate.enter(Some("a"), soon())).await.unwrap();
        let other = tokio::time::timeout(Duration::from_millis(200), gate.enter(Some("b"), soon())).await;
        assert!(other.is_ok(), "a different token must not block");
        let blocked = tokio::time::timeout(Duration::from_millis(100), gate.enter(Some("a"), soon())).await;
        assert!(blocked.is_err(), "the same token must wait");
        drop(first);
        let free = tokio::time::timeout(Duration::from_millis(200), gate.enter(Some("a"), soon())).await;
        assert!(free.is_ok());
    }

    #[tokio::test]
    async fn a_turn_that_does_not_come_in_time_is_busy_and_the_queue_slot_is_released() {
        let gate = ActGate::new(1, Duration::from_millis(100));
        let holder = bounded(gate.enter(Some("t"), soon())).await.unwrap();
        let refused = bounded(gate.enter(Some("t"), soon())).await.unwrap_err();
        assert!(matches!(refused, Error::Busy(_)), "{refused:?}");
        let again = bounded(gate.enter(Some("t"), soon())).await.unwrap_err();
        assert!(matches!(again, Error::Busy(ref m) if m.contains("waited")), "slot was released: {again:?}");
        drop(holder);
        assert!(bounded(gate.enter(Some("t"), soon())).await.is_ok());
    }

    #[tokio::test]
    async fn the_callers_deadline_cuts_the_wait_short_and_the_message_names_it_in_milliseconds() {
        let gate = ActGate::default();
        let _holder = bounded(gate.enter(Some("t"), soon())).await.unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(250);
        let refused = bounded(gate.enter(Some("t"), deadline)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "the 90 s default must not apply");
        assert!(matches!(refused, Error::Busy(ref m) if m.contains("ms")), "{refused:?}");
    }

    #[tokio::test]
    async fn idle_slots_are_forgotten() {
        let gate = ActGate::default();
        drop(bounded(gate.enter(Some("a"), soon())).await.unwrap());
        drop(bounded(gate.enter(Some("b"), soon())).await.unwrap());
        let _current = bounded(gate.enter(Some("c"), soon())).await.unwrap();
        assert_eq!(gate.slots.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_queue_behind_a_token_is_bounded() {
        let gate = Arc::new(ActGate::new(2, DEFAULT_QUEUE_WAIT));
        let holder = bounded(gate.enter(Some("t"), soon())).await.unwrap();
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move { gate.enter(Some("t"), soon()).await.map(drop) })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = bounded(gate.enter(Some("t"), soon())).await.unwrap_err();
        assert!(matches!(refused, Error::Busy(_)), "{refused:?}");
        drop(holder);
        for waiter in waiters {
            bounded(waiter).await.unwrap().unwrap();
        }
        assert!(bounded(gate.enter(Some("t"), soon())).await.is_ok(), "the queue drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_callers_on_one_token_never_hold_it_together() {
        let gate = Arc::new(ActGate::new(64, DEFAULT_QUEUE_WAIT));
        let held = Arc::new(AtomicBool::new(false));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (gate, held) = (Arc::clone(&gate), Arc::clone(&held));
                tokio::spawn(async move {
                    for _ in 0..1500 {
                        // Idle gaps let the sweep see a slot nobody holds, which is where it raced.
                        tokio::time::sleep(Duration::from_micros(1)).await;
                        let turn = gate.enter(Some("t"), soon()).await.unwrap();
                        assert!(!held.swap(true, Ordering::SeqCst), "two holders at once");
                        tokio::task::yield_now().await;
                        held.store(false, Ordering::SeqCst);
                        drop(turn);
                    }
                })
            })
            .collect();
        for task in tasks {
            bounded(task).await.unwrap();
        }
    }
}

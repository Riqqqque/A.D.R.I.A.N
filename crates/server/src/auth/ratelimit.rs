//! Small sliding-window rate limiter for sensitive routes, and the per-user
//! gate that keeps assistant turns from overlapping.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct RateLimiter {
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the action is allowed under `max` hits per `window`.
    pub fn check(&self, key: &str, max: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut map = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let list = map.entry(key.to_string()).or_default();
        list.retain(|t| now.duration_since(*t) < window);
        if list.len() >= max {
            return false;
        }
        list.push(now);
        true
    }
}

/// One assistant turn at a time per user. A turn checks the budget before it
/// records what it spent, so overlapping turns would all pass the same check.
#[derive(Default)]
pub struct TurnGates {
    gates: Mutex<HashMap<i64, Arc<Semaphore>>>,
}

impl TurnGates {
    /// Wait up to `wait` for this user's earlier turn to finish. The turn
    /// holds the permit until it ends.
    pub async fn enter(&self, user_id: i64, wait: Duration) -> Option<OwnedSemaphorePermit> {
        let gate = self
            .gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(user_id)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone();
        tokio::time::timeout(wait, gate.acquire_owned())
            .await
            .ok()?
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_max_per_window_and_isolates_keys() {
        let rl = RateLimiter::new();
        let w = Duration::from_secs(60);
        for _ in 0..3 {
            assert!(rl.check("ip:1", 3, w));
        }
        assert!(!rl.check("ip:1", 3, w)); // 4th hit blocked
        assert!(rl.check("ip:2", 3, w)); // other key unaffected
    }

    #[tokio::test]
    async fn turns_queue_per_user_and_give_up_after_the_wait() {
        let gates = TurnGates::default();
        let wait = Duration::from_millis(20);
        let first = gates.enter(1, wait).await.unwrap();
        assert!(gates.enter(1, wait).await.is_none());
        assert!(gates.enter(2, wait).await.is_some());
        drop(first);
        assert!(gates.enter(1, wait).await.is_some());
    }
}

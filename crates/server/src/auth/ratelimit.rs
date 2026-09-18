//! Small sliding-window rate limiter for sensitive unauthenticated routes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
}

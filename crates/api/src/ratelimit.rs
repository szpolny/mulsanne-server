use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// In-memory sliding-window limiter keyed by IP. Per process, so limits
/// reset on restart and aren't shared between instances.
pub struct Limiter {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl Limiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, hits: Mutex::new(HashMap::new()) }
    }

    /// Records a hit and returns whether it's within the limit.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        // Drop idle entries so the map doesn't grow without bound.
        if hits.len() > 10_000 {
            hits.retain(|_, v| v.last().is_some_and(|t| now.duration_since(*t) < self.window));
        }
        let entry = hits.entry(ip).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() >= self.max {
            return false;
        }
        entry.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_within_window() {
        let limiter = Limiter::new(2, Duration::from_secs(60));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let t = Instant::now();
        assert!(limiter.check_at(a, t));
        assert!(limiter.check_at(a, t));
        assert!(!limiter.check_at(a, t));
        assert!(limiter.check_at(b, t));
        assert!(limiter.check_at(a, t + Duration::from_secs(61)));
    }
}

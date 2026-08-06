// SPDX-License-Identifier: GPL-3.0-or-later
//! Failure-streak bookkeeping shared by the plugin runtime recovery paths:
//! how many consecutive failures an episode has seen and when the last one
//! happened, plus the escalating wait schedule used before respawning a
//! plugin VM worker.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct FailureStreak {
    failures: u32,
    last_failure: Instant,
}

impl FailureStreak {
    pub fn first(now: Instant) -> Self {
        Self {
            failures: 1,
            last_failure: now,
        }
    }

    pub fn record(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.last_failure = now;
    }

    pub fn elapsed(&self, now: Instant, wait: Duration) -> bool {
        now.saturating_duration_since(self.last_failure) >= wait
    }
}

/// Whether enough time has passed since the streak's last failure to try
/// spawning a fresh worker: 1s, 5s, 30s, then 120s capped.
pub fn respawn_due(streak: &FailureStreak, now: Instant) -> bool {
    let wait = match streak.failures {
        0 => return true,
        1 => Duration::from_secs(1),
        2 => Duration::from_secs(5),
        3 => Duration::from_secs(30),
        _ => Duration::from_secs(120),
    };
    streak.elapsed(now, wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respawn_backoff_widens_then_caps() {
        let start = Instant::now();
        let mut streak = FailureStreak::first(start);
        assert!(!respawn_due(&streak, start));
        assert!(respawn_due(&streak, start + Duration::from_secs(1)));
        streak.record(start);
        assert!(!respawn_due(&streak, start + Duration::from_secs(4)));
        assert!(respawn_due(&streak, start + Duration::from_secs(5)));
        streak.record(start);
        assert!(!respawn_due(&streak, start + Duration::from_secs(29)));
        assert!(respawn_due(&streak, start + Duration::from_secs(30)));
        for _ in 0..10 {
            streak.record(start);
        }
        assert!(!respawn_due(&streak, start + Duration::from_secs(119)));
        assert!(respawn_due(&streak, start + Duration::from_secs(120)));
    }

    #[test]
    fn a_recorded_failure_restarts_the_wait_window() {
        let start = Instant::now();
        let mut streak = FailureStreak::first(start);
        assert!(streak.elapsed(start + Duration::from_secs(1), Duration::from_secs(1)));
        streak.record(start + Duration::from_secs(1));
        assert!(!streak.elapsed(start + Duration::from_secs(1), Duration::from_secs(1)));
        assert!(streak.elapsed(start + Duration::from_secs(2), Duration::from_secs(1)));
    }
}

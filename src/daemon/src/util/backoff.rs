// SPDX-License-Identifier: GPL-3.0-or-later
//! Failure-streak bookkeeping shared by the plugin runtime recovery paths:
//! how many consecutive failures an episode has seen and when the last one
//! happened, plus the escalating wait schedule used before respawning a
//! plugin VM worker. Pollers ask [`respawn_due`] on their own tick;
//! [`RetryQueue`] owns the waiting for callers that just want an action
//! retried.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
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

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

pub fn respawn_wait(streak: &FailureStreak) -> Duration {
    match streak.failures {
        0 => Duration::ZERO,
        1 => Duration::from_secs(1),
        2 => Duration::from_secs(5),
        3 => Duration::from_secs(30),
        _ => Duration::from_secs(120),
    }
}

pub fn respawn_due(streak: &FailureStreak, now: Instant) -> bool {
    streak.elapsed(now, respawn_wait(streak))
}

/// What a retried action wants once it has run.
pub enum Retry {
    /// Wait out the next step of the backoff and run again, budget permitting.
    Again,
    /// End the episode. The streak survives, so the next failure resumes the
    /// backoff where this one left off; [`RetryQueue::clear`] is what forgives.
    Stop,
}

/// Retry scheduling per key: one attempt in flight at a time, waits taken from
/// the key's [`FailureStreak`], and a bounded attempt budget.
#[derive(Default)]
pub struct RetryQueue {
    keys: Mutex<HashMap<String, RetryState>>,
}

struct RetryState {
    streak: FailureStreak,
    running: Option<tokio::task::AbortHandle>,
}

impl RetryQueue {
    /// End the episode: drop a queued attempt and forget the streak.
    pub fn clear(&self, key: &str) {
        if let Some(running) = lock(self).remove(key).and_then(|state| state.running) {
            running.abort();
        }
    }

    pub fn clear_all(&self) {
        for (_, state) in lock(self).drain() {
            if let Some(running) = state.running {
                running.abort();
            }
        }
    }
}

/// Queue `action` for `key`, run once its backoff has elapsed and repeated
/// while it answers [`Retry::Again`] and the budget holds. Returns whether an
/// attempt was queued: a key already waiting on one keeps it, so a burst of
/// failures stays a single episode.
pub fn schedule<F, Fut>(queue: &Arc<RetryQueue>, key: &str, max_attempts: u32, action: F) -> bool
where
    F: Fn(u32) -> Fut + Send + 'static,
    Fut: Future<Output = Retry> + Send + 'static,
{
    let mut keys = lock(queue);
    if keys.get(key).is_some_and(|state| state.running.is_some()) {
        return false;
    }
    let Some((attempt, wait)) = note_failure(&mut keys, key, max_attempts) else {
        return false;
    };
    let running = tokio::spawn(retry_loop(
        Arc::clone(queue),
        key.to_owned(),
        max_attempts,
        action,
        attempt,
        wait,
    ));
    if let Some(state) = keys.get_mut(key) {
        state.running = Some(running.abort_handle());
    }
    true
}

async fn retry_loop<F, Fut>(
    queue: Arc<RetryQueue>,
    key: String,
    max_attempts: u32,
    action: F,
    mut attempt: u32,
    mut wait: Duration,
) where
    F: Fn(u32) -> Fut + Send,
    Fut: Future<Output = Retry> + Send,
{
    loop {
        tokio::time::sleep(wait).await;
        if matches!(action(attempt).await, Retry::Stop) {
            break;
        }
        let Some(next) = note_failure(&mut lock(&queue), &key, max_attempts) else {
            break;
        };
        (attempt, wait) = next;
    }
    if let Some(state) = lock(&queue).get_mut(&key) {
        state.running = None;
    }
}

fn lock(queue: &RetryQueue) -> MutexGuard<'_, HashMap<String, RetryState>> {
    queue.keys.lock().unwrap_or_else(|poisoned| {
        log::warn!("recovered poisoned retry queue");
        poisoned.into_inner()
    })
}

fn note_failure(
    keys: &mut HashMap<String, RetryState>,
    key: &str,
    max_attempts: u32,
) -> Option<(u32, Duration)> {
    let now = Instant::now();
    let state = keys
        .entry(key.to_owned())
        .and_modify(|state| state.streak.record(now))
        .or_insert_with(|| RetryState {
            streak: FailureStreak::first(now),
            running: None,
        });
    (state.streak.failures() <= max_attempts)
        .then(|| (state.streak.failures(), respawn_wait(&state.streak)))
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

    type BoxFuture = std::pin::Pin<Box<dyn Future<Output = Retry> + Send>>;

    /// An action that records the attempt numbers it was handed.
    fn recorder(seen: &Arc<Mutex<Vec<u32>>>, answer: fn() -> Retry) -> impl Fn(u32) -> BoxFuture {
        let seen = Arc::clone(seen);
        move |attempt| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                seen.lock().unwrap().push(attempt);
                answer()
            })
        }
    }

    /// Long enough for any queued attempt to have run under a paused clock.
    async fn drain() {
        tokio::time::sleep(Duration::from_secs(300)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_action_that_keeps_asking_stops_at_the_attempt_budget() {
        let queue = Arc::new(RetryQueue::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        assert!(schedule(
            &queue,
            "plug",
            3,
            recorder(&seen, || Retry::Again)
        ));

        drain().await;

        assert_eq!(*seen.lock().unwrap(), [1, 2, 3]);
        assert!(
            !schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)),
            "a spent budget refuses further attempts until the key is cleared"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_episode_resumes_where_it_left_off_until_cleared() {
        let queue = Arc::new(RetryQueue::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        assert!(schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)));
        drain().await;
        assert!(schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)));
        drain().await;
        assert_eq!(*seen.lock().unwrap(), [1, 2], "the streak survives a stop");

        queue.clear("plug");
        assert!(schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)));
        drain().await;
        assert_eq!(*seen.lock().unwrap(), [1, 2, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn clearing_a_key_cancels_the_attempt_it_was_waiting_on() {
        let queue = Arc::new(RetryQueue::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        assert!(schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)));
        assert!(
            !schedule(&queue, "plug", 3, recorder(&seen, || Retry::Stop)),
            "a burst of failures is one episode, not one attempt each"
        );

        queue.clear("plug");
        drain().await;

        assert!(seen.lock().unwrap().is_empty());
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

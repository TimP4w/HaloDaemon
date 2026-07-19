// SPDX-License-Identifier: GPL-3.0-or-later
//! Idle-shutdown: a killed (not cleanly quit) tray never sends
//! `DaemonCommand::Shutdown`, so the daemon watches its own client count
//! instead and shuts itself down once empty for
//! [`halod_shared::lifecycle::IDLE_SHUTDOWN_GRACE`]. `--headless` opts out.

use std::sync::Arc;
use std::time::{Duration, Instant};

use halod_shared::lifecycle::IDLE_SHUTDOWN_GRACE;

use crate::application::ipc::router::request_shutdown;
use crate::application::state::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn idle_shutdown_due(empty_since: Option<Instant>, now: Instant, grace: Duration) -> bool {
    empty_since.is_some_and(|since| now.duration_since(since) >= grace)
}

pub fn spawn_idle_watcher(app: Arc<AppState>, headless: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if headless {
            return;
        }
        let mut empty_since: Option<Instant> = Some(Instant::now());
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let connected = !app.clients.lock().await.is_empty();
            if connected {
                empty_since = None;
                continue;
            }
            empty_since.get_or_insert_with(Instant::now);
            if idle_shutdown_due(empty_since, Instant::now(), IDLE_SHUTDOWN_GRACE) {
                log::info!(
                    "no connected client for {:?}; shutting down",
                    IDLE_SHUTDOWN_GRACE
                );
                request_shutdown(&app);
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_due_while_a_client_is_connected() {
        assert!(!idle_shutdown_due(
            None,
            Instant::now(),
            Duration::from_secs(10)
        ));
    }

    #[test]
    fn not_due_before_grace_elapses() {
        let since = Instant::now();
        let now = since + Duration::from_secs(5);
        assert!(!idle_shutdown_due(
            Some(since),
            now,
            Duration::from_secs(10)
        ));
    }

    #[test]
    fn due_once_grace_elapses() {
        let since = Instant::now();
        let now = since + Duration::from_secs(10);
        assert!(idle_shutdown_due(Some(since), now, Duration::from_secs(10)));
    }
}

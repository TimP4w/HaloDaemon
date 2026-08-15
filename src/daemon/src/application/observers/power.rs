// SPDX-License-Identifier: GPL-3.0-or-later
//! Re-applies device state after the host wakes from suspend.
//!
//! A device that stays enumerated across suspend never looks disconnected, so
//! neither hotplug observer revisits it — yet its firmware has usually dropped
//! back to its own defaults. Resume is the only signal that says so.

use std::sync::Arc;
use std::time::Duration;

use crate::application::state::AppState;

/// Buses, wireless links, and firmware all come back on their own schedule.
/// Restores dispatched into that window fail; the shared retry backoff behind
/// `restore_saved_state` covers whatever is still not answering after it.
const SETTLE: Duration = Duration::from_secs(3);

/// How long to wait before resubscribing after the notification stream ends.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);

pub async fn run(app: Arc<AppState>) {
    loop {
        let mut resumes = match crate::infrastructure::platform::power::resume_events().await {
            Ok(Some(resumes)) => resumes,
            Ok(None) => return std::future::pending().await,
            Err(e) => {
                log::warn!("[power] resume notifications unavailable: {e:#}");
                tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                continue;
            }
        };
        while resumes.recv().await.is_some() {
            log::info!("[power] system resumed, re-applying device state");
            tokio::time::sleep(SETTLE).await;
            crate::application::usecases::profiles::lifecycle::load_active_profile(Arc::clone(
                &app,
            ))
            .await;
        }
        log::warn!("[power] resume notifications ended, resubscribing");
        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
    }
}

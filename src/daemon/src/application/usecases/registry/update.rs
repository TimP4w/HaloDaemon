// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use halod_shared::app::latest_release_api_url;
use halod_shared::types::NotificationCode;
use semver::Version;

use crate::application::notifications;
use crate::application::state::AppState;
use crate::domain::events::{Change, ChangeSink as _};
use crate::domain::update::{latest_release, newer};

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_METADATA_BYTES: u64 = 1 << 20;

fn fetch_latest() -> Result<Vec<u8>> {
    let url = halod_plugin_signing::fetch::https_url(&latest_release_api_url())?;
    halod_plugin_signing::fetch::https_get(&url, MAX_METADATA_BYTES)
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}

pub async fn check(app: &Arc<AppState>, fetch: impl FnOnce() -> Result<Vec<u8>> + Send + 'static) {
    if std::mem::replace(&mut app.update.lock().await.checking, true) {
        return;
    }
    app.record_change(Change::Update).await;

    let result = tokio::task::spawn_blocking(fetch)
        .await
        .context("update check task panicked")
        .and_then(|bytes| latest_release(&bytes?))
        .map(|latest| latest.and_then(|latest| newer(&current_version(), latest)));

    let available = {
        let mut status = app.update.lock().await;
        status.checking = false;
        match result {
            Ok(available) => {
                status.failed = false;
                status.available = available.map(|v| v.to_string());
            }
            Err(error) => {
                log::warn!("update check failed: {error:#}");
                status.failed = true;
            }
        }
        status.available.clone()
    };
    app.record_change(Change::Update).await;

    let Some(version) = available else {
        return;
    };
    if app.config.read().await.gui.update_notified == version {
        return;
    }
    app.config.write().await.gui.update_notified = version.clone();
    app.request_config_save();
    notifications::send(app, NotificationCode::UpdateAvailable { version }).await;
}

pub fn check_in_background(app: Arc<AppState>) {
    tokio::spawn(async move { check(&app, fetch_latest).await });
}

pub async fn watcher(app: Arc<AppState>) {
    tokio::time::sleep(FIRST_CHECK_DELAY).await;
    loop {
        if app.config.read().await.gui.check_for_updates {
            check(&app, fetch_latest).await;
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

pub async fn set_update_check(enabled: bool, app: Arc<AppState>) -> Result<()> {
    app.config.write().await.gui.check_for_updates = enabled;
    app.request_config_save();
    app.record_change(Change::Gui).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tmp_config;
    use halod_shared::bus::BusEventPayload;
    use halod_shared::types::UpdateStatus;

    fn release(tag: &str) -> Vec<u8> {
        serde_json::json!({ "tag_name": tag, "assets": [{ "name": "SHA256SUMS.sig" }] })
            .to_string()
            .into_bytes()
    }

    fn update_notifications(app: &AppState) -> Vec<String> {
        app.data_bus
            .replay_events(None)
            .events
            .into_iter()
            .filter_map(|event| match event.payload {
                BusEventPayload::Notification(n) => match n.code {
                    NotificationCode::UpdateAvailable { version } => Some(version),
                    _ => None,
                },
            })
            .collect()
    }

    #[tokio::test]
    async fn newer_release_is_published_and_notified_once() {
        with_tmp_config(|app| async move {
            check(&app, || Ok(release("v99.0.0"))).await;
            check(&app, || Ok(release("v99.0.0"))).await;
            let status = app.update.lock().await.clone();
            assert_eq!(
                status,
                UpdateStatus {
                    checking: false,
                    available: Some("99.0.0".into()),
                    failed: false,
                }
            );
            assert_eq!(update_notifications(&app), vec!["99.0.0".to_string()]);
            assert_eq!(app.config.read().await.gui.update_notified, "99.0.0");
        })
        .await;
    }

    #[tokio::test]
    async fn a_newer_version_notifies_again() {
        with_tmp_config(|app| async move {
            check(&app, || Ok(release("v99.0.0"))).await;
            check(&app, || Ok(release("v99.1.0"))).await;
            assert_eq!(update_notifications(&app), vec!["99.0.0", "99.1.0"]);
        })
        .await;
    }

    #[tokio::test]
    async fn current_or_older_release_is_not_available() {
        with_tmp_config(|app| async move {
            check(&app, || Ok(release("v0.0.1"))).await;
            let status = app.update.lock().await.clone();
            assert_eq!(status, UpdateStatus::default());
            assert!(update_notifications(&app).is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn failed_fetch_sets_failed_and_keeps_the_last_result() {
        with_tmp_config(|app| async move {
            check(&app, || Ok(release("v99.0.0"))).await;
            check(&app, || anyhow::bail!("offline")).await;
            let status = app.update.lock().await.clone();
            assert!(status.failed);
            assert!(!status.checking);
            assert_eq!(status.available.as_deref(), Some("99.0.0"));
            assert_eq!(update_notifications(&app).len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn check_commits_update_topic_transactions() {
        with_tmp_config(|app| async move {
            let before = app.data_bus.state_snapshot(&[]).revision;
            check(&app, || Ok(release("v99.0.0"))).await;
            assert_eq!(app.data_bus.state_snapshot(&[]).revision, before + 2);
        })
        .await;
    }

    #[tokio::test]
    async fn a_check_already_in_flight_is_not_repeated() {
        with_tmp_config(|app| async move {
            app.update.lock().await.checking = true;
            check(&app, || Ok(release("v99.0.0"))).await;
            let status = app.update.lock().await.clone();
            assert!(status.checking);
            assert_eq!(status.available, None);
        })
        .await;
    }

    #[tokio::test]
    async fn set_update_check_persists_the_flag() {
        with_tmp_config(|app| async move {
            set_update_check(false, app.clone()).await.unwrap();
            assert!(!app.config.read().await.gui.check_for_updates);
        })
        .await;
    }
}

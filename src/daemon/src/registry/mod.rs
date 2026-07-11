// SPDX-License-Identifier: GPL-3.0-or-later
pub mod config;
pub mod discovery;
pub mod snapshot;
pub mod state;
pub mod usecases;

use anyhow::Result;
use std::sync::Arc;

use crate::drivers::Device;
use crate::state::AppState;
use halod_shared::types::VisibilityState;

pub use state::{HidTracking, HidTrackingEntry};

/// Look up the device with the given `id` and return an owned clone, holding the
/// `devices` lock only for the lookup. Error if the device is not found or is
/// disabled — every device-action usecase routes through here, so a disabled
/// device rejects all actions. Visibility changes use a raw lookup instead.
pub async fn require_device_owned_id(id: &str, app: &AppState) -> Result<Arc<dyn Device>> {
    let device = app
        .find_device_by_id(id)
        .await
        .ok_or_else(|| anyhow::anyhow!("device not found: {id}"))?;
    if device.active_state() == VisibilityState::Disabled {
        anyhow::bail!("device is disabled: {id}");
    }
    Ok(device)
}

pub async fn seed_known_devices(app: Arc<AppState>) {
    let devices = app.devices.read().await.clone();
    let mut cfg = app.config.write().await;
    for device in &devices {
        config::ensure_record(&mut cfg.known_devices, device.id(), Some(device.as_ref()));
    }
}

pub async fn initialize_app_state(app: Arc<AppState>) {
    ensure_official_repo(&app).await;
    {
        let cfg = app.config.read().await;
        crate::drivers::plugins::load_all_with_repos(
            &crate::config::plugins_dir(),
            &crate::drivers::plugins::repo_plugin_dirs(&cfg.plugin_repos),
        );
        crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
        crate::drivers::plugins::set_granted(&cfg.plugin_permissions);
        crate::drivers::plugins::set_acknowledged(&cfg.plugin_acknowledged);
        crate::drivers::plugins::set_config_values(&cfg.plugin_config);
        crate::drivers::plugins::set_integrations_disabled(&cfg.integrations_disabled);
    }
    crate::drivers::plugins::set_secret_store(app.secret_store.clone());
    notify_ungranted_plugins(&app).await;
    discovery::discover_devices(app.clone()).await;
    seed_known_devices(app.clone()).await;
    usecases::chain::restore_saved_chains(app.clone()).await;
    crate::profiles::usecases::profiles::load_active_profile(app.clone()).await;
}

/// Seed the non-removable official plugin repo record if absent, and clone it
/// if its clone directory is missing. Official plugins still go through the
/// normal consent/permission flow — only the repo *record* is protected from
/// removal (see `usecases::repos::remove_repo`). A clone failure (e.g. no
/// network on first launch) is logged and must never fail boot: the daemon
/// simply has no official plugins until a later successful clone.
pub(crate) async fn ensure_official_repo(app: &Arc<AppState>) {
    ensure_official_repo_from(app, crate::constants::OFFICIAL_PLUGIN_REPO_URL).await;
}

/// [`ensure_official_repo`] parameterized on the URL to clone, so tests can
/// exercise the offline/clone-failure path against a local, non-network URL
/// instead of the real `constants::OFFICIAL_PLUGIN_REPO_URL`.
async fn ensure_official_repo_from(app: &Arc<AppState>, url: &str) {
    use crate::config::PluginRepoRecord;
    use crate::drivers::plugins::repo;

    {
        let mut cfg = app.config.write().await;
        if !cfg
            .plugin_repos
            .iter()
            .any(|r| r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
        {
            cfg.plugin_repos.push(PluginRepoRecord {
                url: url.to_owned(),
                slug: crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
                branch: None,
                locked_sha: String::new(),
                last_sync: None,
            });
            app.request_config_save();
        }
    }

    let dest = crate::config::plugin_repos_dir().join(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG);
    if dest.join(".git").exists() {
        return;
    }
    let url = url.to_owned();
    let dest2 = dest.clone();
    match tokio::task::spawn_blocking(move || repo::clone(&url, &dest2, None)).await {
        Ok(Ok(sha)) => {
            let mut cfg = app.config.write().await;
            if let Some(r) = cfg
                .plugin_repos
                .iter_mut()
                .find(|r| r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
            {
                r.locked_sha = sha;
                r.last_sync = Some(chrono::Utc::now().to_rfc3339());
            }
            app.request_config_save();
        }
        Ok(Err(e)) => {
            log::warn!("official plugin repo clone failed (continuing with none): {e:#}");
        }
        Err(e) => log::warn!("official plugin repo clone task panicked: {e:#}"),
    }
}

/// Toast one notification per auto-discovered plugin that needs a permission
/// grant. A manually-imported plugin is pre-marked notified by the import
/// usecase (the GUI shows a blocking consent modal for that flow instead).
pub(crate) async fn notify_ungranted_plugins(app: &Arc<AppState>) {
    for plugin in crate::drivers::plugins::take_newly_ungranted_plugins() {
        crate::platform::notify::send(
            app,
            halod_shared::types::NotificationCode::PluginNeedsPermission { plugin },
        )
        .await;
    }
}

#[cfg(test)]
mod guard_tests {
    use super::require_device_owned_id;
    use crate::config::Config;
    use crate::drivers::Device;
    use crate::state::AppState;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::Arc;

    #[tokio::test]
    async fn require_device_owned_id_rejects_disabled_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        dev.set_active_state(VisibilityState::Disabled);
        app.devices.write().await.push(dev);

        let result = require_device_owned_id("dev1", &app).await;
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("disabled"),
            "expected disabled error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn require_device_owned_id_allows_visible_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        app.devices.write().await.push(dev);

        assert!(require_device_owned_id("dev1", &app).await.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use async_trait::async_trait;

    struct StubDevice {
        id: &'static str,
    }

    #[async_trait]
    impl Device for StubDevice {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn vendor(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> {
            vec![]
        }
    }

    async fn app_with_devices(ids: &[&'static str]) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let mut devices = app.devices.write().await;
        for id in ids {
            devices.push(Arc::new(StubDevice { id }) as Arc<dyn Device>);
        }
        drop(devices);
        app
    }

    #[tokio::test]
    async fn require_device_owned_id_returns_matching_device() {
        let app = app_with_devices(&["dev_a", "dev_b"]).await;
        let found = require_device_owned_id("dev_b", &app).await.unwrap();
        assert_eq!(found.id(), "dev_b");
    }

    #[tokio::test]
    async fn require_device_owned_id_errors_when_not_found() {
        let app = app_with_devices(&["dev_a"]).await;
        let err = require_device_owned_id("dev_z", &app)
            .await
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("dev_z"));
    }
}

#[cfg(test)]
mod official_repo_tests {
    use super::*;
    use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG as OFFICIAL_SLUG;

    #[tokio::test]
    async fn seeds_the_record_even_when_the_clone_url_is_unreachable() {
        crate::test_support::with_tmp_config(|app| async move {
            // A local nonexistent path fails fast (no network hang) while still
            // exercising the exact "clone failed" path a bad/offline URL hits.
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;

            let cfg = app.config.read().await;
            let record = cfg
                .plugin_repos
                .iter()
                .find(|r| r.slug == OFFICIAL_SLUG)
                .expect("official repo record must be seeded even if the clone fails");
            assert!(
                record.locked_sha.is_empty(),
                "a failed clone must not fabricate a locked_sha"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn is_idempotent_and_does_not_duplicate_the_record() {
        crate::test_support::with_tmp_config(|app| async move {
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;

            let cfg = app.config.read().await;
            let count = cfg
                .plugin_repos
                .iter()
                .filter(|r| r.slug == OFFICIAL_SLUG)
                .count();
            assert_eq!(count, 1, "re-running must not duplicate the seeded record");
        })
        .await;
    }
}

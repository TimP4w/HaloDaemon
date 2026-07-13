// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing device plugins: enable/disable, import, and delete.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use halod_shared::types::Permission;
use serde_json::json;

use crate::ipc::ClientHandle;
use crate::state::AppState;

/// Enable or disable a plugin, persist the choice, and reconcile its devices.
pub async fn set_enabled(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    if enabled {
        // Re-enabling a plugin accepts its current on-disk content as the new
        // baseline (stage it into the index), so a plugin that was quarantined
        // for a local edit — or that the user is intentionally editing — isn't
        // re-disabled on the next startup tamper check. No-op for a pristine or
        // non-repo plugin.
        accept_on_disk_content(&app, &id).await;
    }
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.disabled.retain(|x| x != &id);
        if !enabled {
            cfg.plugins.disabled.push(id.clone());
        }
    }
    app.request_config_save();
    reconcile_plugins(&app, &[id]).await;
    Ok(())
}

/// Stage a repo plugin's working-tree files into its git index, making the
/// tamper-check baseline match what's on disk. Local, best-effort: a failure
/// (or a non-repo plugin) is logged and ignored.
async fn accept_on_disk_content(app: &Arc<AppState>, id: &str) {
    let Some((slug, subpath)) = app.registry.repo_location_for(id) else {
        return;
    };
    let dir = crate::config::plugin_repos_dir().join(&slug);
    if let Err(e) = tokio::task::spawn_blocking(move || {
        crate::drivers::plugins::repo::stage_subtree(&dir, &subpath)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("stage task panicked: {e}")))
    {
        log::warn!("accepting on-disk content for plugin '{id}': {e:#}");
    }
}

/// Replace the set of permissions granted to a plugin and persist the choice.
/// Also records the plugin's current script hash as acknowledged: granting is
/// an explicit consent to run *this* script, so the plugin activates and stays
/// active only until its content changes (trust-on-first-use). The grant and
/// enabled state are committed together before devices are reconciled.
pub async fn set_trust(
    id: String,
    granted: Vec<Permission>,
    enabled: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let hash = app.registry.content_hash_for(&id);
    {
        let mut cfg = app.config.write().await;
        if granted.is_empty() {
            // Revoke: drop the grant and its content pin, back to pristine.
            cfg.plugins.granted.remove(&id);
            cfg.plugins.acknowledged.remove(&id);
        } else {
            cfg.plugins.granted.insert(id.clone(), granted);
            // Pin the grant to the exact script the user is consenting to.
            match hash {
                Some(h) => {
                    cfg.plugins.acknowledged.insert(id.clone(), h);
                }
                None => {
                    cfg.plugins.acknowledged.remove(&id);
                }
            }
        }
        cfg.plugins.disabled.retain(|x| x != &id);
        if !enabled {
            cfg.plugins.disabled.push(id.clone());
        }
    }
    app.request_config_save();
    reconcile_plugins(&app, &[id]).await;
    Ok(())
}

/// Split `values` into the plaintext `cfg.plugins.config` map and the secret
/// store (for manifest-declared `secure` fields), then re-publish the
/// plaintext config to the plugin registry and persist. Shared by
/// [`set_config`] and [`super::integrations::set_integration_config`]. An
/// absent (or empty) secure value leaves the
/// previously stored secret untouched, so the GUI never has to round-trip a
/// secret to keep it.
pub(crate) async fn persist_config_values(
    id: &str,
    values: &std::collections::HashMap<String, String>,
    app: &Arc<AppState>,
) -> Result<()> {
    app.registry.validate_config_values(id, values)?;
    let secure_keys: std::collections::HashSet<String> = app
        .registry
        .secure_config_keys_for(id)
        .into_iter()
        .collect();
    let mut cfg = app.config.write().await;
    let plaintext = cfg.plugins.config.entry(id.to_owned()).or_default();
    for (key, value) in values {
        if secure_keys.contains(key) {
            if !value.is_empty() {
                app.secret_store
                    .set(id, key, value)
                    .with_context(|| format!("storing secret '{key}' for plugin '{id}'"))?;
            }
        } else {
            plaintext.insert(key.clone(), value.clone());
        }
    }
    if plaintext.is_empty() {
        cfg.plugins.config.remove(id);
    }
    app.registry.replace_policy(&cfg.plugins);
    Ok(())
}

/// Fetch a plugin's display-only asset and send it to the client as base64. Not staged — a pure read.
pub async fn get_asset(
    plugin_id: String,
    name: String,
    client: ClientHandle,
    app: Arc<AppState>,
) -> Result<()> {
    let bytes = app.registry.read_asset(&plugin_id, &name)?;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    client.send_json(&json!({
        "type": "plugin_asset",
        "plugin_id": plugin_id,
        "name": name,
        "data_b64": data_b64,
    }));
    Ok(())
}

/// Replace a plugin's user-editable config values and reconcile its devices.
pub async fn set_config(
    id: String,
    values: std::collections::HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    persist_config_values(&id, &values, &app).await?;
    app.request_config_save();
    reconcile_plugins(&app, &[id]).await;
    Ok(())
}

static IMPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tracks plugin ids with an import in flight, so concurrent imports of the same
/// id are rejected rather than racing on the destination directory.
static IMPORTS_IN_FLIGHT: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

struct ImportGuard(String);

impl ImportGuard {
    fn acquire(id: &str) -> Result<Self> {
        let mut guard = IMPORTS_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        let set = guard.get_or_insert_with(std::collections::HashSet::new);
        if !set.insert(id.to_owned()) {
            bail!("an import for plugin '{id}' is already in progress");
        }
        Ok(Self(id.to_owned()))
    }
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = IMPORTS_IN_FLIGHT
            .lock()
            .or_else(|e| Ok::<_, ()>(e.into_inner()))
        {
            if let Some(set) = guard.as_mut() {
                set.remove(&self.0);
            }
        }
    }
}

/// Recursively copy `src` into `dst` (both directories), creating `dst`.
/// Rejects symlinks: `std::fs::copy` dereferences them, so a symlinked entry in
/// an imported package would otherwise copy the *target's* contents (an
/// arbitrary host file the daemon can read) into the plugins dir. A legitimate
/// plugin package has no need for symlinks, so we fail the import outright.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("creating plugin dir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("plugin package contains a symlink: {}", from.display());
        } else if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        } else {
            bail!(
                "plugin package contains a non-regular file: {}",
                from.display()
            );
        }
    }
    Ok(())
}

/// Install a plugin package (a directory containing `plugin.yaml` + its entry
/// script) as a new plugin directory: validated in place first, then copied
/// in and re-parsed from its final location. Staged — see the module docs.
pub async fn import(source_dir: String, app: Arc<AppState>) -> Result<()> {
    let src = Path::new(&source_dir);
    let parsed = crate::drivers::plugins::parse_manifest_from_dir(src)
        .context("plugin package is not a valid manifest")?;
    let id = parsed.plugin_id.clone();

    // Serialize concurrent imports of the same id so two callers can't race on
    // the same destination.
    let _guard = ImportGuard::acquire(&id)?;

    let plugins_dir = crate::config::plugins_dir();
    let dst = plugins_dir.join(&id);
    if dst.exists() {
        bail!("a plugin '{id}' is already installed");
    }

    // Copy into a uniquely-named private temp sibling, validate the completed
    // copy, then atomically rename it into place so a failed import never leaves
    // a partial installed plugin behind.
    std::fs::create_dir_all(&plugins_dir)?;
    let seq = IMPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = plugins_dir.join(format!(".{id}.import.{}.{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let manifest = (|| {
        copy_dir_all(src, &staging)?;
        crate::drivers::plugins::parse_manifest_from_dir(&staging)
            .context("re-parsing imported plugin directory")
    })();
    let manifest = match manifest {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&staging, &dst) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e).with_context(|| format!("installing plugin into {}", dst.display()));
    }
    log::info!(
        "Imported plugin package {} into {}",
        src.display(),
        dst.display()
    );

    // A manual import gets the GUI's blocking consent modal instead of the
    // auto-discovery toast — suppress it before the reload below would
    // otherwise fire one for this exact plugin.
    app.registry.suppress_permission_notice(&manifest.plugin_id);

    reconcile_plugins(&app, &[manifest.plugin_id]).await;
    Ok(())
}

/// Delete a user plugin directory by id. A repo-sourced plugin has no
/// standalone directory to delete on its own — remove its repo instead —
/// so this refuses anything but a `Local` plugin. Staged — see the module docs.
pub async fn delete(id: String, app: Arc<AppState>) -> Result<()> {
    let is_local = app
        .registry
        .list(&*app.secret_store)
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| matches!(p.source, halod_shared::types::PluginSource::Local))
        .unwrap_or(true);
    if !is_local {
        bail!("plugin '{id}' is provided by a repository — remove the repository instead");
    }
    let dir = crate::config::plugins_dir().join(&id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => log::info!("Deleted plugin {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!("Plugin {} already gone", dir.display());
        }
        Err(e) => return Err(e).with_context(|| format!("deleting {}", dir.display())),
    }

    if purge_plugin_state(&id, &app).await {
        app.request_config_save();
    }

    reconcile_plugins(&app, &[id]).await;
    Ok(())
}

/// Purge every id-keyed piece of plugin state — secret, disabled flag, plaintext
/// config, granted permissions, acknowledged content hash, and integration
/// disable — returning whether config changed. Shared by [`delete`] and
/// `repos::remove_repo`, so a reinstall of identical content can't inherit an
/// old grant or acknowledgement.
pub(crate) async fn purge_plugin_state(id: &str, app: &Arc<AppState>) -> bool {
    for key in app.registry.secure_config_keys_for(id) {
        if let Err(e) = app.secret_store.delete(id, &key) {
            log::warn!("deleting secret '{key}' for plugin '{id}': {e:#}");
        }
    }

    let mut cfg = app.config.write().await;
    let before = cfg.plugins.disabled.len();
    cfg.plugins.disabled.retain(|x| x != id);
    let disabled_changed = cfg.plugins.disabled.len() != before;
    let config_changed = cfg.plugins.config.remove(id).is_some();
    let perms_changed = cfg.plugins.granted.remove(id).is_some();
    let ack_changed = cfg.plugins.acknowledged.remove(id).is_some();
    let integ_before = cfg.plugins.integrations_disabled.len();
    cfg.plugins.integrations_disabled.retain(|x| x != id);
    let integ_changed = cfg.plugins.integrations_disabled.len() != integ_before;

    let changed =
        disabled_changed || config_changed || perms_changed || ack_changed || integ_changed;
    if changed {
        app.registry.replace_policy(&cfg.plugins);
    }
    changed
}

/// Re-read the plugins directory and every configured git-repo source, and re-apply the disabled/granted sets. Shared with `repos.rs`.
pub(crate) async fn reload_registry(app: &Arc<AppState>) {
    let cfg = app.config.read().await;
    app.registry.load_all_with_repos(
        &crate::config::plugins_dir(),
        &crate::drivers::plugins::repo_plugin_dirs(&cfg.plugins.repos),
    );
    app.registry.replace_policy(&cfg.plugins);
}

/// Apply a repo-driven registry change immediately.
///
/// Repository installs and updates are explicit user actions, so reconcile
/// their affected devices before returning success.
pub(crate) async fn apply_repo_plugins(app: Arc<AppState>, plugin_ids: Vec<String>) -> Result<()> {
    if plugin_ids.is_empty() {
        crate::ipc::broadcast_state(&app).await;
        return Ok(());
    }
    reconcile_plugins(&app, &plugin_ids).await;
    Ok(())
}

/// Close and unregister every currently-registered device owned by one of
/// `plugins` (plus its `_ctrl_` children); leaves every other device untouched.
async fn teardown_owned_devices(app: &Arc<AppState>, plugins: &[String]) {
    let owned_ids: Vec<String> = {
        let devices = app.devices.read().await;
        devices
            .iter()
            .filter(|d| {
                d.owning_plugin_id()
                    .is_some_and(|pid| plugins.contains(&pid))
            })
            .map(|d| d.id().to_owned())
            .collect()
    };
    // Untrack the FULL torn-down set (parent + children): a plugin controller and
    // its children share one HID key, and `untrack_devices` only prunes a key
    // once every device it tracks is torn down. Passing only the owning parents
    // would leave the key tracked, so the HID rescan skips it and the device
    // never comes back on re-enable.
    let mut torn_down: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &owned_ids {
        let removed = super::registration::unregister_device_and_children(app, id).await;
        torn_down.extend(removed);
    }
    app.hid.untrack_devices(&torn_down).await;
}

/// Scoped teardown + reprobe for `plugins`: only devices owned by one of
/// these plugin ids are closed and re-discovered; every other device is
/// left untouched.
pub(crate) async fn reconcile_plugins(app: &Arc<AppState>, plugins: &[String]) {
    use crate::registry::discovery::PendingRediscovery;
    app.merge_rediscovery(PendingRediscovery::PluginSet(
        plugins.iter().cloned().collect(),
    ))
    .await;
    drain_rediscovery(app).await;
}

pub(crate) async fn reconcile_full(app: &Arc<AppState>) {
    app.merge_rediscovery(crate::registry::discovery::PendingRediscovery::Full)
        .await;
    drain_rediscovery(app).await;
}

async fn drain_rediscovery(app: &Arc<AppState>) {
    use crate::registry::discovery::{DiscoveryScope, PendingRediscovery};
    let _runner = app.rediscovery_runner.lock().await;
    loop {
        match app.take_rediscovery().await {
            PendingRediscovery::Clean => return,
            PendingRediscovery::Full => {
                reload_registry(app).await;
                app.set_discovery_scope(DiscoveryScope::Full).await;
                crate::registry::discovery::discover_devices(Arc::clone(app)).await;
                app.set_discovery_scope(DiscoveryScope::Clean).await;
            }
            PendingRediscovery::PluginSet(plugin_ids) => {
                reconcile_plugin_set(app, &plugin_ids.into_iter().collect::<Vec<_>>()).await;
            }
        }
    }
}

async fn reconcile_plugin_set(app: &Arc<AppState>, plugins: &[String]) {
    use crate::registry::discovery::{DiscoveryFilter, DiscoveryScope};

    // Keep both sides of a manifest change in scope. Deleted/disabled plugins
    // need their old specs so a native driver can reclaim the hardware, while
    // newly imported or updated plugins need their new specs to claim it.
    let mut specs = app.registry.device_specs_for(plugins);

    // 1. Refresh manifests + disabled/granted/config so match_handle reflects
    //    the new state, then add the post-change specs.
    reload_registry(app).await;
    specs.extend(app.registry.device_specs_for(plugins));

    // 2. Teardown: close and unregister every device owned by a changed plugin.
    teardown_owned_devices(app, plugins).await;

    // 3. Scope re-probing to only these plugins' hardware.
    app.set_discovery_scope(DiscoveryScope::PluginSet {
        plugin_ids: plugins.iter().cloned().collect(),
        filter: Arc::new(DiscoveryFilter { specs }),
    })
    .await;

    // 4. Scoped re-probe.
    crate::registry::discovery::discover_devices(Arc::clone(app)).await;

    // 5. Clear the scope, then seed known-device records and restore any
    //    chain layout for the newly registered devices. Each new device's own
    //    profile state was already applied by `register_device` during the
    //    scoped probe, so we deliberately skip the global `load_active_profile`
    //    here — it would clear every device's LCD slot and re-load every
    //    device's state, disturbing the untouched devices this path exists to
    //    leave alone (mirroring the integration scoped path).
    app.set_discovery_scope(DiscoveryScope::Clean).await;
    crate::registry::seed_known_devices(Arc::clone(app)).await;
    crate::registry::usecases::chain::restore_saved_chains(Arc::clone(app)).await;
    crate::ipc::broadcast_state(app).await;
}

/// Sanitize a file name or repo URL into a safe plugin id / directory name (lower-cased `[a-z0-9-]`, path-traversal-proof).
pub(crate) fn sanitize_slug(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut slug = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "plugin".to_owned()
    } else {
        slug.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn import_guard_serializes_same_id() {
        let _a = ImportGuard::acquire("dup-id").unwrap();
        assert!(ImportGuard::acquire("dup-id").is_err());
        drop(_a);
        // Once released, a fresh acquire succeeds.
        assert!(ImportGuard::acquire("dup-id").is_ok());
    }

    #[test]
    fn copy_dir_all_copies_files_and_nested_dirs() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        std::fs::create_dir(src.path().join("assets")).unwrap();
        std::fs::write(src.path().join("assets").join("a.png"), b"png").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let out = dst.path().join("demo");
        copy_dir_all(src.path(), &out).unwrap();

        assert_eq!(
            std::fs::read(out.join("plugin.yaml")).unwrap(),
            b"id: demo\n"
        );
        assert_eq!(
            std::fs::read(out.join("assets").join("a.png")).unwrap(),
            b"png"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_all_rejects_a_symlink() {
        let secret = tempfile::tempdir().unwrap();
        std::fs::write(secret.path().join("secret.txt"), b"top-secret").unwrap();

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        std::os::unix::fs::symlink(
            secret.path().join("secret.txt"),
            src.path().join("leak.txt"),
        )
        .unwrap();

        let dst = tempfile::tempdir().unwrap();
        let out = dst.path().join("demo");
        let err = copy_dir_all(src.path(), &out).unwrap_err();
        assert!(err.to_string().contains("symlink"));
        // The dereferenced target's contents must never land in the plugins dir.
        assert!(!out.join("leak.txt").exists());
    }

    #[test]
    fn sanitize_slugs_a_file_name() {
        assert_eq!(sanitize_slug("My Driver.lua"), "my-driver");
        assert_eq!(sanitize_slug("wled_udp"), "wled-udp");
        assert_eq!(sanitize_slug("a  b--c.lua"), "a-b-c");
    }

    #[test]
    fn sanitize_strips_path_traversal_and_handles_empty() {
        // A path separator can never survive into the written directory name.
        assert!(!sanitize_slug("../../etc/passwd").contains('/'));
        assert_eq!(sanitize_slug("///"), "plugin");
        assert_eq!(sanitize_slug(""), "plugin");
    }

    #[tokio::test]
    async fn set_enabled_reconciles_without_touching_unrelated_devices() {
        crate::test_support::with_tmp_config(|app| async move {
            app.devices.write().await.push(std::sync::Arc::new(
                crate::test_support::MockDevice::new("stays-open"),
            ));

            set_enabled("some_plugin".into(), false, app.clone())
                .await
                .unwrap();

            assert_eq!(
                app.devices.read().await.len(),
                1,
                "scoped reconciliation must leave unrelated devices alone"
            );
            assert!(app
                .config
                .read()
                .await
                .plugins
                .disabled
                .contains(&"some_plugin".to_string()));
        })
        .await;
    }

    #[tokio::test]
    async fn scoped_teardown_prunes_hid_tracking_so_reprobe_can_readd() {
        // Regression: the scoped teardown must drop the torn-down device's HID
        // key, or the HID rescan skips the still-tracked key and the device never
        // comes back on re-enable/config-change (the full path clears all HID
        // tracking; the scoped path must clear just these keys).
        use crate::registry::HidTrackingEntry;
        use crate::test_support::MockDevice;
        use std::sync::Arc;

        crate::test_support::with_tmp_config(|app| async move {
            let owned: Arc<dyn crate::drivers::Device> =
                Arc::new(MockDevice::new("P-dev").with_owning_plugin_id("P"));
            let other: Arc<dyn crate::drivers::Device> =
                Arc::new(MockDevice::new("Q-dev").with_owning_plugin_id("Q"));
            app.devices.write().await.push(owned.clone());
            app.devices.write().await.push(other.clone());
            app.hid
                .track("1234:5678:P".into(), HidTrackingEntry::Primary(vec![owned]))
                .await;
            app.hid
                .track("9abc:def0:Q".into(), HidTrackingEntry::Primary(vec![other]))
                .await;

            teardown_owned_devices(&app, &["P".to_string()]).await;

            let keys = app.hid.keys().await;
            assert!(
                !keys.contains("1234:5678:P"),
                "torn-down plugin's HID key must be untracked so a re-probe re-adds it"
            );
            assert!(
                keys.contains("9abc:def0:Q"),
                "an untouched plugin's HID key must survive the scoped teardown"
            );
            assert!(app.find_device_by_id("P-dev").await.is_none());
            assert!(app.find_device_by_id("Q-dev").await.is_some());
        })
        .await;
    }

    #[tokio::test]
    async fn scoped_teardown_untracks_a_key_shared_by_a_parent_and_its_children() {
        // Regression: a plugin controller and its children (e.g. NZXT Control Hub
        // + fan cores) share one HID key. Only the parent carries the owning
        // plugin id, but `untrack_devices` prunes a key only once EVERY device it
        // tracks is torn down — so teardown must feed it the whole subtree, not
        // just the owning parents, or the key survives and the re-probe skips the
        // device.
        use crate::registry::HidTrackingEntry;
        use crate::test_support::MockDevice;
        use std::sync::Arc;

        crate::test_support::with_tmp_config(|app| async move {
            let parent: Arc<dyn crate::drivers::Device> =
                Arc::new(MockDevice::new("nzxt-abc").with_owning_plugin_id("nzxt"));
            // Chain-accessory child: shares the parent key, no owning id of its own.
            let child: Arc<dyn crate::drivers::Device> =
                Arc::new(MockDevice::new("nzxt-abc_acc_0_1"));
            app.devices.write().await.push(parent.clone());
            app.devices.write().await.push(child.clone());
            app.hid
                .track(
                    "1e71:2022:S".into(),
                    HidTrackingEntry::Primary(vec![parent, child]),
                )
                .await;

            teardown_owned_devices(&app, &["nzxt".to_string()]).await;

            assert!(
                app.hid.keys().await.is_empty(),
                "the shared key must be untracked once the whole subtree is torn down"
            );
            assert!(app.find_device_by_id("nzxt-abc").await.is_none());
            assert!(app.find_device_by_id("nzxt-abc_acc_0_1").await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn teardown_owned_devices_removes_only_the_owning_plugins_subtree() {
        use crate::test_support::MockDevice;
        crate::test_support::with_tmp_config(|app| async move {
            let root = Arc::new(MockDevice::new("p1-abc").with_owning_plugin_id("p1"));
            // A child registered alongside the plugin root (the `_ctrl_` scheme
            // `unregister_device_and_children` prunes) — no owner id of its own.
            let child = Arc::new(MockDevice::new("p1-abc_ctrl_0"));
            let other_plugin = Arc::new(MockDevice::new("p2-xyz").with_owning_plugin_id("p2"));
            let native = Arc::new(MockDevice::new("native-dev"));
            {
                let mut devices = app.devices.write().await;
                devices.push(root.clone());
                devices.push(child.clone());
                devices.push(other_plugin.clone());
                devices.push(native.clone());
            }

            teardown_owned_devices(&app, &["p1".to_string()]).await;

            let remaining: Vec<String> = app
                .devices
                .read()
                .await
                .iter()
                .map(|d| d.id().to_owned())
                .collect();
            assert_eq!(remaining, vec!["p2-xyz", "native-dev"]);

            assert!(root.closed.load(Ordering::SeqCst));
            assert!(child.closed.load(Ordering::SeqCst));
            assert!(!other_plugin.closed.load(Ordering::SeqCst));
            assert!(!native.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    #[tokio::test]
    async fn teardown_owned_devices_is_a_noop_when_nothing_is_owned() {
        use crate::test_support::MockDevice;
        crate::test_support::with_tmp_config(|app| async move {
            let native = Arc::new(MockDevice::new("native-dev"));
            app.devices.write().await.push(native.clone());

            teardown_owned_devices(&app, &["p1".to_string()]).await;

            assert_eq!(app.devices.read().await.len(), 1);
            assert!(!native.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    const CONFIG_TEST_PLUGIN: &str = r#"
        return {
          config = { fields = {
            { key = "host", label = "Host" },
            { key = "token", label = "Token", secure = true },
          } },
        }
    "#;

    /// `devices` must be declared here — a directory plugin's own Lua manifest fields are overlaid away.
    const CONFIG_TEST_PLUGIN_YAML: &str =
        "id: cfgtest\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n";

    fn write_config_test_plugin(root: &std::path::Path) {
        let dir = root.join("cfgtest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), CONFIG_TEST_PLUGIN_YAML).unwrap();
        std::fs::write(dir.join("main.lua"), CONFIG_TEST_PLUGIN).unwrap();
    }

    /// Loads `CONFIG_TEST_PLUGIN` into `app`'s plugin registry for the duration
    /// of `f`, then restores the registry to just the built-ins.
    /// Requires `with_tmp_config`.
    async fn with_config_test_plugin<F, Fut>(app: &Arc<AppState>, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = crate::config::plugins_dir();
        std::fs::create_dir_all(&dir).unwrap();
        write_config_test_plugin(&dir);
        app.registry.load_all(&dir);
        f().await;
        app.registry.load_all(std::path::Path::new("/nonexistent"));
    }

    fn test_client() -> (
        ClientHandle,
        tokio::sync::mpsc::Receiver<std::sync::Arc<Vec<u8>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<std::sync::Arc<Vec<u8>>>(16);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        (client, rx)
    }

    #[tokio::test]
    async fn get_asset_replies_with_base64_bytes() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("assetplug");
        std::fs::create_dir_all(plugin_dir.join("assets")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: assetplug\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n\
             logo: logo.png\n",
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.lua"), "return {}").unwrap();
        let png = {
            let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]));
            let mut b = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut b, image::ImageFormat::Png)
                .unwrap();
            b.into_inner()
        };
        std::fs::write(plugin_dir.join("assets/logo.png"), &png).unwrap();
        app.registry.load_all(dir.path());

        let (client, mut rx) = test_client();
        get_asset("assetplug".into(), "logo.png".into(), client, app.clone())
            .await
            .unwrap();

        let frame = rx.try_recv().expect("a frame was queued");
        let v: serde_json::Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(v["type"], "plugin_asset");
        assert_eq!(v["plugin_id"], "assetplug");
        assert_eq!(v["name"], "logo.png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["data_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, png);

        app.registry.load_all(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test]
    async fn get_asset_errors_for_unknown_plugin() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let (client, _rx) = test_client();
        let err = get_asset("does-not-exist".into(), "logo.png".into(), client, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown plugin"));
    }

    #[tokio::test]
    async fn set_trust_records_the_acknowledged_content_hash() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                set_trust(
                    "cfgtest".into(),
                    vec![Permission::Network],
                    true,
                    app.clone(),
                )
                .await
                .unwrap();
                let expected = app.registry.content_hash_for("cfgtest");
                assert!(expected.is_some());
                let cfg = app.config.read().await;
                assert_eq!(cfg.plugins.acknowledged.get("cfgtest"), expected.as_ref());
                assert_eq!(
                    cfg.plugins.granted.get("cfgtest"),
                    Some(&vec![Permission::Network])
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn revoking_clears_both_the_grant_and_the_content_pin() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                set_trust(
                    "cfgtest".into(),
                    vec![Permission::Network],
                    true,
                    app.clone(),
                )
                .await
                .unwrap();
                assert!(app
                    .config
                    .read()
                    .await
                    .plugins
                    .acknowledged
                    .contains_key("cfgtest"));

                // Empty grant = revoke: both the grant and its pin are dropped.
                set_trust("cfgtest".into(), vec![], false, app.clone())
                    .await
                    .unwrap();
                let cfg = app.config.read().await;
                assert!(!cfg.plugins.granted.contains_key("cfgtest"));
                assert!(!cfg.plugins.acknowledged.contains_key("cfgtest"));
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn set_config_splits_secure_values_into_the_secret_store() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                let mut values = std::collections::HashMap::new();
                values.insert("host".to_string(), "127.0.0.1".to_string());
                values.insert("token".to_string(), "s3cr3t".to_string());
                set_config("cfgtest".into(), values, app.clone())
                    .await
                    .unwrap();

                let cfg = app.config.read().await;
                assert_eq!(
                    cfg.plugins
                        .config
                        .get("cfgtest")
                        .and_then(|m| m.get("host")),
                    Some(&"127.0.0.1".to_string())
                );
                assert!(
                    !cfg.plugins
                        .config
                        .get("cfgtest")
                        .is_some_and(|m| m.contains_key("token")),
                    "a secure value must never land in the plaintext config map"
                );
                drop(cfg);
                assert_eq!(
                    app.secret_store.get("cfgtest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn set_config_with_blank_secure_value_keeps_the_existing_secret() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                let mut first = std::collections::HashMap::new();
                first.insert("token".to_string(), "s3cr3t".to_string());
                set_config("cfgtest".into(), first, app.clone())
                    .await
                    .unwrap();

                // Re-saving with an empty secure value must not clear it.
                let mut second = std::collections::HashMap::new();
                second.insert("token".to_string(), "".to_string());
                set_config("cfgtest".into(), second, app.clone())
                    .await
                    .unwrap();

                assert_eq!(
                    app.secret_store.get("cfgtest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn set_config_rejects_unknown_key_and_out_of_range_number() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = crate::config::plugins_dir();
            let pdir = dir.join("numcfg");
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(
                pdir.join("plugin.yaml"),
                "id: numcfg\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
            )
            .unwrap();
            std::fs::write(
                pdir.join("main.lua"),
                r#"return { config = { fields = { { key = "hz", label = "Hz", kind = "number", min = 1, max = 100 } } } }"#,
            )
            .unwrap();
            app.registry.load_all(&dir);

            let one = |k: &str, v: &str| {
                let mut m = std::collections::HashMap::new();
                m.insert(k.to_string(), v.to_string());
                m
            };
            assert!(set_config("numcfg".into(), one("nope", "1"), app.clone())
                .await
                .is_err());
            assert!(set_config("numcfg".into(), one("hz", "999"), app.clone())
                .await
                .is_err());
            assert!(set_config("numcfg".into(), one("hz", "abc"), app.clone())
                .await
                .is_err());
            set_config("numcfg".into(), one("hz", "50"), app.clone())
                .await
                .unwrap();

            app.registry.load_all(std::path::Path::new("/nonexistent"));
        })
        .await;
    }

    #[tokio::test]
    async fn delete_purges_the_plugins_stored_secret() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = crate::config::plugins_dir();
            std::fs::create_dir_all(&dir).unwrap();
            write_config_test_plugin(&dir);
            app.registry.load_all(&dir);

            let mut values = std::collections::HashMap::new();
            values.insert("token".to_string(), "s3cr3t".to_string());
            set_config("cfgtest".into(), values, app.clone())
                .await
                .unwrap();
            assert_eq!(
                app.secret_store.get("cfgtest", "token").unwrap(),
                Some("s3cr3t".to_string())
            );

            delete("cfgtest".into(), app.clone()).await.unwrap();

            assert_eq!(app.secret_store.get("cfgtest", "token").unwrap(), None);
        })
        .await;
    }
}

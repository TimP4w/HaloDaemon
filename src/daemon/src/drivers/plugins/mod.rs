// SPDX-License-Identifier: GPL-3.0-or-later
//! Device plugins: add a device without recompiling the daemon.
//!
//! A plugin is a Lua script in the plugins directory that declares a `match`
//! (which hardware it drives) and an `identity`, plus — in later steps —
//! callbacks that turn capability calls into transport bytes. Plugins expose
//! only *existing* capability kinds; Halo owns the capability taxonomy.
//!
//! Registration is at runtime (not the compile-time `inventory` path native
//! drivers use): `load_all` reads the plugins directory into the registry
//! snapshot, and `make_device` consults `match_handle` before the native
//! descriptors, so a plugin shadows a native driver for the same hardware.

mod backends;
mod bytebuf;
mod chain_leaf;
mod device;
mod effect_worker;
mod ffi;
mod image_api;
pub(crate) mod integration_scan;
mod lua_worker;
mod manifest;
#[cfg(feature = "plugin-test")]
pub mod plugin_test;
pub mod repo;
mod sandbox;
mod transport;
mod transport_api;
mod worker;

pub use device::LuaDevice;
pub use effect_worker::{LedCoord, PluginEffectHandle};
#[cfg(test)]
pub(crate) use manifest::parse_manifest;
pub use manifest::{parse_manifest_from_dir, DeviceSpec, EffectKind, PluginManifest, ProbeMode};
pub use scan::plugin_smbus_scan_entries;
pub use worker::run_pre_scan;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use halod_shared::types::{Animation, EffectParamValue, Permission, PluginInfo, PluginKind};

use crate::drivers::Device;
use crate::registry::discovery::DiscoveryHandle;

mod scan;

#[cfg(test)]
mod tests;

/// The whole registry lives in one immutable snapshot (see [`PluginState`]);
/// any test (in this module or elsewhere, e.g. the RGB engine's plugin-effect
/// integration tests) that mutates it via `load_all`/`set_disabled`/`set_granted`
/// must hold this lock for its duration so it can't race a sibling test on
/// another thread.
#[cfg(test)]
pub(crate) static TEST_GLOBALS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One RGB effect a plugin declares, registered under its namespaced catalog
/// id (`<plugin_id>:<effect.id>`) so it can never collide with a native
/// effect or another plugin's.
#[derive(Clone)]
pub struct PluginEffectEntry {
    pub plugin_id: String,
    pub script_source: String,
    pub kind: EffectKind,
    pub catalog_id: String,
    pub descriptor: Animation,
}

/// Immutable snapshot of every piece of registry state a reader needs. Readers
/// take a cheap `Arc` clone via [`snapshot`] and traverse it lock-free, so a
/// re-entrant read (e.g. `build_device` resolving config while matching a
/// handle) is plain field access — never a recursive lock that could deadlock a
/// pending write. Mutators build a new snapshot and swap it in under one write
/// lock via [`update`].
#[derive(Clone, Default)]
struct PluginState {
    manifests: Vec<PluginManifest>,
    effects: Vec<PluginEffectEntry>,
    /// Plugin ids the user disabled. `match_handle` skips these, so a disabled
    /// plugin no longer shadows its native driver.
    disabled: HashSet<String>,
    /// Integration ids disabled *as an integration* — independent of `disabled`
    /// (which governs whether the Lua may run at all). Only meaningful for
    /// `PluginKind::Integration` plugins.
    integrations_disabled: HashSet<String>,
    /// Permissions the user granted per plugin (see [`granted_for`]). Every
    /// plugin — built-in or not — must be granted its permissions through
    /// consent; nothing is auto-granted.
    granted: HashMap<String, Vec<Permission>>,
    /// Content hash (hex SHA-256) the user consented to per plugin. A disk
    /// plugin is consent-satisfied only when its script still hashes to this.
    acknowledged: HashMap<String, String>,
    /// Non-secure config values the user set per plugin. Secure values never
    /// live here — see the secret store.
    config_values: HashMap<String, HashMap<String, String>>,
}

static STATE: LazyLock<RwLock<Arc<PluginState>>> =
    LazyLock::new(|| RwLock::new(Arc::new(PluginState::default())));

/// Recover the guard if a panicked plugin poisoned the lock, so one bad plugin
/// can't cascade into a daemon crash. Safe here: the guarded data stays
/// structurally consistent even if a mutator panicked mid-update.
fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| {
        log::warn!("recovered poisoned plugin lock");
        e.into_inner()
    })
}

fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| {
        log::warn!("recovered poisoned plugin lock");
        e.into_inner()
    })
}

/// The current registry snapshot. The lock is held only for the `Arc` clone,
/// never across the caller's use of the data — so re-entrant reads can't deadlock.
fn snapshot() -> Arc<PluginState> {
    read_recover(&STATE).clone()
}

/// Swap in a new snapshot by applying `f` to a clone of the current one. Held
/// under the write lock so concurrent mutators can't lose each other's edits;
/// `f` only mutates fields (never re-locks), so it cannot deadlock.
fn update(f: impl FnOnce(&mut PluginState)) {
    let mut guard = write_recover(&STATE);
    let mut next = (**guard).clone();
    f(&mut next);
    *guard = Arc::new(next);
}

fn effect_entries_for(manifest: &PluginManifest) -> Vec<PluginEffectEntry> {
    manifest
        .effects
        .iter()
        .map(|e| PluginEffectEntry {
            plugin_id: manifest.plugin_id.clone(),
            script_source: manifest.script_source.clone(),
            kind: e.kind,
            catalog_id: e.catalog_id(&manifest.plugin_id),
            descriptor: e.descriptor(&manifest.plugin_id),
        })
        .collect()
}

/// Every enabled plugin's declared descriptors of one effect kind, for the
/// RGB engine's dynamic catalog.
fn effect_descriptors(kind: EffectKind) -> Vec<Animation> {
    let state = snapshot();
    state
        .effects
        .iter()
        .filter(|e| e.kind == kind && !state.disabled.contains(&e.plugin_id))
        .map(|e| e.descriptor.clone())
        .collect()
}

/// Descriptors for every enabled plugin-declared pixmap effect.
pub fn pixmap_effect_descriptors() -> Vec<Animation> {
    effect_descriptors(EffectKind::Pixmap)
}

/// Descriptors for every enabled plugin-declared direct effect.
pub fn direct_effect_descriptors() -> Vec<Animation> {
    effect_descriptors(EffectKind::Direct)
}

/// Look up a registered effect entry by its namespaced catalog id. `None` if
/// unknown or its plugin is disabled.
pub fn effect_entry(catalog_id: &str) -> Option<PluginEffectEntry> {
    let state = snapshot();
    state
        .effects
        .iter()
        .find(|e| e.catalog_id == catalog_id && !state.disabled.contains(&e.plugin_id))
        .cloned()
}

/// Spawn a worker for a registered pixmap effect. `None` for an unknown,
/// disabled, or wrong-kind id — the caller falls back to a native default.
pub fn build_pixmap_effect(
    catalog_id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<PluginEffectHandle> {
    build_effect_handle(EffectKind::Pixmap, catalog_id, params)
}

/// Spawn a worker for a registered direct effect. `None` for an unknown,
/// disabled, or wrong-kind id — the caller falls back to a native default.
pub fn build_direct_effect(
    catalog_id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<PluginEffectHandle> {
    build_effect_handle(EffectKind::Direct, catalog_id, params)
}

fn build_effect_handle(
    kind: EffectKind,
    catalog_id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<PluginEffectHandle> {
    let entry = effect_entry(catalog_id)?;
    if entry.kind != kind {
        return None;
    }
    let effect_id = catalog_id
        .strip_prefix(&format!("{}:", entry.plugin_id))?
        .to_string();
    let granted = granted_for(&entry.plugin_id);
    let config = resolved_config_for(&entry.plugin_id, &granted);
    Some(PluginEffectHandle::spawn(
        entry.script_source,
        effect_id,
        params.clone(),
        granted,
        config,
    ))
}

/// Replace the disabled-plugin set (from `config.plugins_disabled`).
pub fn set_disabled(ids: &[String]) {
    update(|s| s.disabled = ids.iter().cloned().collect());
}

fn is_disabled(plugin_id: &str) -> bool {
    snapshot().disabled.contains(plugin_id)
}

/// Replace the integration-disabled set (from `config.integrations_disabled`).
pub fn set_integrations_disabled(ids: &[String]) {
    update(|s| s.integrations_disabled = ids.iter().cloned().collect());
}

fn is_integration_disabled(plugin_id: &str) -> bool {
    snapshot().integrations_disabled.contains(plugin_id)
}

/// Replace the granted-permissions map (from `config.plugin_permissions`).
pub fn set_granted(granted: &HashMap<String, Vec<Permission>>) {
    update(|s| s.granted = granted.clone());
}

/// Permissions actually granted to `plugin_id`'s Lua sandbox: the user-set grants.
pub(crate) fn granted_for(plugin_id: &str) -> Vec<Permission> {
    snapshot()
        .granted
        .get(plugin_id)
        .cloned()
        .unwrap_or_default()
}

/// Replace the acknowledged-hash map (from `config.plugin_acknowledged`).
pub fn set_acknowledged(acknowledged: &HashMap<String, String>) {
    update(|s| s.acknowledged = acknowledged.clone());
}

/// The content hash the user acknowledged for `plugin_id`, if any.
fn acknowledged_hash_for(plugin_id: &str) -> Option<String> {
    snapshot().acknowledged.get(plugin_id).cloned()
}

/// The current on-disk content hash for `plugin_id` from the loaded registry,
/// for recording an acknowledgment when the user consents. `None` if unknown.
pub fn content_hash_for(plugin_id: &str) -> Option<String> {
    snapshot()
        .manifests
        .iter()
        .find(|m| m.plugin_id == plugin_id)
        .map(|m| m.content_hash())
}

/// Replace the plugin config-values map (from `config.plugin_config`).
pub fn set_config_values(values: &HashMap<String, HashMap<String, String>>) {
    update(|s| s.config_values = values.clone());
}

/// A plugin's resolved non-secure config: every declared field defaults to its
/// manifest `default`, overridden by any value the user has set. Unknown keys
/// the user may have stored (e.g. after a manifest edit removed a field) are
/// not included — only keys the manifest still declares.
pub fn config_for(plugin_id: &str) -> HashMap<String, String> {
    let state = snapshot();
    match state.manifests.iter().find(|m| m.plugin_id == plugin_id) {
        Some(manifest) => config_values_for(&state, manifest),
        None => HashMap::new(),
    }
}

/// Non-secure config for a manifest already in hand, resolved against `state`'s
/// stored values. Takes the snapshot as a parameter so a caller already holding
/// one (e.g. `list`) reuses it instead of taking another.
fn config_values_for(state: &PluginState, manifest: &PluginManifest) -> HashMap<String, String> {
    let stored = state.config_values.get(&manifest.plugin_id);
    manifest
        .config_fields()
        .iter()
        .filter(|f| !f.secure)
        .map(|f| {
            let value = stored
                .and_then(|m| m.get(&f.key))
                .cloned()
                .unwrap_or_else(|| f.default.clone());
            (f.key.clone(), value)
        })
        .collect()
}

/// The secret store backing plugin-declared `secure` config fields, shared
/// process-wide (set once at startup/reload). Kept out of [`PluginState`]: it is
/// a set-once handle, not part of the read-mostly registry snapshot. `Arc` so
/// `AppState::secret_store` and this static point at the same instance without
/// cloning the store itself.
static SECRET_STORE: RwLock<Option<Arc<dyn crate::secrets::SecretStore>>> = RwLock::new(None);

/// Point the process-wide secret-store reference at `store` (from
/// `AppState::secret_store`).
pub fn set_secret_store(store: Arc<dyn crate::secrets::SecretStore>) {
    *write_recover(&SECRET_STORE) = Some(store);
}

/// Process-wide sink for plugin runtime-error notifications, set once at startup
/// (mirrors [`set_secret_store`]). `Weak` so it never keeps `AppState` alive.
/// Background/engine paths would otherwise only log a failed Lua callback; this
/// lets the device layer (which has no `AppState` of its own) push a toast.
static NOTIFY_SINK: RwLock<Option<std::sync::Weak<crate::state::AppState>>> = RwLock::new(None);

/// Device ids with an outstanding runtime error, so a plugin that fails every
/// engine tick is announced once per failure episode, not on every frame.
/// Cleared by [`clear_runtime_error`] on the next successful call.
static FAILING_DEVICES: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// Install the runtime-error notification sink (from `AppState`).
pub fn set_notification_sink(app: &Arc<crate::state::AppState>) {
    *write_recover(&NOTIFY_SINK) = Some(Arc::downgrade(app));
}

/// Surface a plugin device's runtime callback failure to the user as a
/// [`NotificationCode::PluginRuntimeError`], once per failure episode (deduped
/// on `device_id`). No-op if the device already has an outstanding error or no
/// sink is installed.
pub(super) async fn report_runtime_error(device_id: &str, device_name: &str, detail: String) {
    {
        let mut guard = write_recover(&FAILING_DEVICES);
        if !guard
            .get_or_insert_with(HashSet::new)
            .insert(device_id.to_owned())
        {
            return; // already reported this episode
        }
    }
    let Some(app) = read_recover(&NOTIFY_SINK)
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
    else {
        return;
    };
    crate::platform::notify::send(
        &app,
        halod_shared::types::NotificationCode::PluginRuntimeError {
            plugin: device_name.to_owned(),
            detail,
        },
    )
    .await;
}

/// Clear a device's outstanding-error flag after a successful call, so a later
/// failure alerts again rather than being deduped away.
pub(super) fn clear_runtime_error(device_id: &str) {
    if let Some(set) = write_recover(&FAILING_DEVICES).as_mut() {
        set.remove(device_id);
    }
}

/// A plugin's full resolved config for its Lua VM: `config_for` plus, only
/// when `Permission::SecureStorage` is granted, its decrypted secure values.
/// Without that grant the secure keys are simply absent — a plugin can never
/// read its own secret without the user having consented to the permission.
pub fn resolved_config_for(plugin_id: &str, granted: &[Permission]) -> HashMap<String, String> {
    let mut config = config_for(plugin_id);
    if !granted.contains(&Permission::SecureStorage) {
        return config;
    }
    let Some(store) = read_recover(&SECRET_STORE).clone() else {
        return config;
    };
    for key in secure_config_keys_for(plugin_id) {
        match store.get(plugin_id, &key) {
            Ok(Some(value)) => {
                config.insert(key, value);
            }
            Ok(None) => {}
            Err(e) => log::warn!("reading secret '{key}' for plugin '{plugin_id}': {e:#}"),
        }
    }
    config
}

/// Keys of `plugin_id`'s declared `secure = true` config fields, for splitting
/// an incoming `SetPluginConfig` (or a plugin delete) between the plaintext
/// config store and the secret store. Empty for an unknown plugin id.
pub fn secure_config_keys_for(plugin_id: &str) -> Vec<String> {
    snapshot()
        .manifests
        .iter()
        .find(|m| m.plugin_id == plugin_id)
        .map(|m| {
            m.secure_config_keys()
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// True when the plugin may activate.
fn consent_satisfied(manifest: &PluginManifest) -> bool {
    if manifest.permissions.is_empty() {
        return true;
    }
    let granted = granted_for(&manifest.plugin_id);
    if !manifest.permissions.iter().all(|p| granted.contains(p)) {
        return false;
    }
    acknowledged_hash_for(&manifest.plugin_id).as_deref() == Some(&manifest.content_hash())
}

/// Every enabled, permission-satisfied `Integration` plugin, for the
/// integration `TransportScanner` (`integration_scan.rs`) — it has no
/// `DiscoveryHandle` to match against, so it iterates these directly instead
/// of going through `match_handle`.
pub(super) fn integration_manifests() -> Vec<PluginManifest> {
    let state = snapshot();
    state
        .manifests
        .iter()
        .filter(|m| {
            m.plugin_type == PluginKind::Integration
                && !state.disabled.contains(&m.plugin_id)
                && !state.integrations_disabled.contains(&m.plugin_id)
                && consent_satisfied(m)
        })
        .cloned()
        .collect()
}

/// The single enabled, permission-satisfied `Integration` manifest for
/// `plugin_id`, for a scoped reconnect of just that one integration (see
/// `registry::usecases::integrations`). `None` if it's missing, plugin-
/// disabled, integration-disabled, or lacks its declared permissions.
pub(super) fn integration_manifest(plugin_id: &str) -> Option<PluginManifest> {
    integration_manifests()
        .into_iter()
        .find(|m| m.plugin_id == plugin_id)
}

/// Plugin ids already surfaced via a "needs permission" notification (or
/// explicitly suppressed — see [`suppress_permission_notice`]), so a plugin
/// is only ever announced once, not on every rescan.
static NOTIFIED: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// Suppress the auto-discovery notification for a plugin id — used when the
/// GUI is already showing its own consent modal (a manual "Add plugin"
/// import), so the user isn't told about it twice.
pub fn suppress_permission_notice(plugin_id: &str) {
    write_recover(&NOTIFIED)
        .get_or_insert_with(HashSet::new)
        .insert(plugin_id.to_owned());
}

/// Why a plugin can't currently activate, so the daemon picks the right alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UngrantedReason {
    /// Never approved (or explicitly revoked): the user must grant permissions.
    NeedsPermission,
    /// Previously approved, but the on-disk content hash changed since — an
    /// edit or an update. The user re-approves the new content.
    ContentChanged,
}

/// Pure filter behind [`take_newly_ungranted_plugins`]: which manifests can't
/// activate and aren't yet in `notified` (inserting each as it's returned),
/// paired with the reason so the caller can choose the notification.
fn ungranted_in(
    manifests: &[PluginManifest],
    notified: &mut HashSet<String>,
) -> Vec<(String, UngrantedReason)> {
    manifests
        .iter()
        .filter(|m| !consent_satisfied(m) && notified.insert(m.plugin_id.clone()))
        .map(|m| {
            // A stored acknowledgment that no longer matches means the content
            // changed under a previously-approved plugin, rather than a plugin
            // the user has simply never approved.
            let reason =
                if acknowledged_hash_for(&m.plugin_id).is_some_and(|h| h != m.content_hash()) {
                    UngrantedReason::ContentChanged
                } else {
                    UngrantedReason::NeedsPermission
                };
            (m.display_name().to_owned(), reason)
        })
        .collect()
}

/// Display names (with reason) of plugins that can't activate and haven't been
/// announced yet (auto-discovered, not manually imported — those are marked
/// via [`suppress_permission_notice`] before this is ever called). Marks
/// every returned plugin as notified so a later rescan won't repeat it.
pub fn take_newly_ungranted_plugins() -> Vec<(String, UngrantedReason)> {
    let state = snapshot();
    let mut guard = write_recover(&NOTIFIED);
    let notified = guard.get_or_insert_with(HashSet::new);
    ungranted_in(&state.manifests, notified)
}

/// Where `plugin_dir` came from: `Repo { slug }` under `plugin_repos_dir()`, else `Local`.
fn plugin_source_for(plugin_dir: &Path) -> halod_shared::types::PluginSource {
    let repos_dir = crate::config::plugin_repos_dir();
    match plugin_dir.strip_prefix(&repos_dir).ok().and_then(|rel| {
        rel.components().next().and_then(|c| match c {
            std::path::Component::Normal(slug) => Some(slug.to_string_lossy().into_owned()),
            _ => None,
        })
    }) {
        Some(slug) => halod_shared::types::PluginSource::Repo { slug },
        None => halod_shared::types::PluginSource::Local,
    }
}

/// For a repo-sourced plugin, its repo slug and path within that repo's clone
/// (`""` for a root-level plugin, `plugins/<id>` for a subdir one) — the same
/// pair [`crate::drivers::plugins::repo::remote_plugin_content`] and
/// `checkout_subtree` need. `None` for an unknown or `Local` plugin.
pub fn repo_location_for(plugin_id: &str) -> Option<(String, std::path::PathBuf)> {
    let state = snapshot();
    let manifest = state.manifests.iter().find(|m| m.plugin_id == plugin_id)?;
    let repos_dir = crate::config::plugin_repos_dir();
    let rel = manifest.plugin_dir.strip_prefix(&repos_dir).ok()?;
    let mut components = rel.components();
    let slug = match components.next()? {
        std::path::Component::Normal(s) => s.to_string_lossy().into_owned(),
        _ => return None,
    };
    let subpath = components.as_path().to_path_buf();
    Some((slug, subpath))
}

/// Every loaded plugin with its enable state, for the management UI.
/// `secrets` resolves whether each declared secure field currently has a
/// value stored, without ever reading the plaintext (see `PluginInfo::secret_set`).
pub fn list(secrets: &dyn crate::secrets::SecretStore) -> Vec<PluginInfo> {
    let state = snapshot();
    state
        .manifests
        .iter()
        .map(|m| {
            let secret_set = m
                .config_fields()
                .iter()
                .filter(|f| f.secure)
                .map(|f| {
                    let is_set = secrets
                        .get(&m.plugin_id, &f.key)
                        .unwrap_or_else(|e| {
                            log::warn!(
                                "checking secret '{}' for plugin '{}': {e:#}",
                                f.key,
                                m.plugin_id
                            );
                            None
                        })
                        .is_some();
                    (f.key.clone(), is_set)
                })
                .collect();
            let consented = consent_satisfied(m);
            PluginInfo {
                id: m.plugin_id.clone(),
                name: m.display_name(),
                path: m.source_path.display().to_string(),
                plugin_type: m.plugin_type,
                capabilities: m.capability_labels(),
                effect_names: m.effects.iter().map(|e| e.name.clone()).collect(),
                enabled: !is_disabled(&m.plugin_id) && consented,
                author: m.author().to_owned(),
                version: m.version().to_owned(),
                license: m.license().to_owned(),
                description: m.description().to_owned(),
                targets: m.target_labels(),
                devices: m
                    .devices
                    .iter()
                    .map(|d| halod_shared::types::PluginDeviceInfo {
                        vendor: d.vendor.clone(),
                        model: d.model.clone(),
                        name: d.display_name().to_owned(),
                        device_type: d.device_type,
                    })
                    .collect(),
                logo: m.logo.clone(),
                effect_thumbnails: m
                    .effect_thumbnails
                    .iter()
                    .map(|e| halod_shared::types::PluginEffectAsset {
                        id: e.id.clone(),
                        thumbnail: e.thumbnail.clone(),
                    })
                    .collect(),
                source: plugin_source_for(&m.plugin_dir),
                declared_permissions: m.permissions.clone(),
                granted_permissions: granted_for(&m.plugin_id),
                config_fields: m.config_fields().iter().map(Into::into).collect(),
                config_values: config_values_for(&state, m),
                secret_set,
                integration_enabled: !is_integration_disabled(&m.plugin_id),
                consented,
                content_changed: acknowledged_hash_for(&m.plugin_id)
                    .is_some_and(|h| h != m.content_hash()),
            }
        })
        .collect()
}

/// Read a plugin's display-only asset (logo/effect thumbnail) from `<plugin_dir>/assets/<name>`.
pub fn read_asset(plugin_id: &str, name: &str) -> anyhow::Result<Vec<u8>> {
    halod_shared::types::validate_image_filename(name)
        .map_err(|e| anyhow::anyhow!("invalid asset name '{name}': {e}"))?;
    let state = snapshot();
    let manifest = state
        .manifests
        .iter()
        .find(|m| m.plugin_id == plugin_id)
        .ok_or_else(|| anyhow::anyhow!("unknown plugin '{plugin_id}'"))?;
    if manifest.plugin_dir.as_os_str().is_empty() {
        anyhow::bail!("plugin '{plugin_id}' has no on-disk assets");
    }
    let path = manifest.plugin_dir.join("assets").join(name);
    let len = std::fs::metadata(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?
        .len();
    if len > halod_shared::types::MAX_PLUGIN_ASSET_BYTES {
        anyhow::bail!(
            "asset '{name}' is {len} bytes, over the {} byte limit",
            halod_shared::types::MAX_PLUGIN_ASSET_BYTES
        );
    }
    std::fs::read(&path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

/// A plugin id is owned by whichever source loads it first (see
/// [`load_all_with_repos`]'s load order: official repo, then local `plugins/`,
/// then other repos). Any later source declaring the same id is rejected here
/// and surfaced via [`take_plugin_load_warnings`] instead of silently
/// dropped — so a community repo can never shadow an existing plugin id.
static LOAD_WARNINGS: RwLock<Vec<PluginLoadWarning>> = RwLock::new(Vec::new());

/// A rejected plugin load: an id collision (with an earlier source) or a bad manifest.
#[derive(Clone, Debug)]
pub struct PluginLoadWarning {
    pub plugin_id: String,
    pub path: String,
    pub reason: String,
}

/// Every load warning recorded during the most recent [`load_all_with_repos`],
/// draining the set so a later poll doesn't repeat it.
pub fn take_plugin_load_warnings() -> Vec<PluginLoadWarning> {
    std::mem::take(&mut write_recover(&LOAD_WARNINGS))
}

/// Reason a declared logo is unusable, or `None` if it passes every bound.
/// A logo whose file is absent isn't rejected here — it's left for the GUI's
/// initials fallback; only a *present* logo is held to the size/shape bounds.
fn logo_rejection(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join("assets").join(name);
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > halod_shared::types::MAX_PLUGIN_ASSET_BYTES {
        return Some(format!(
            "logo is {} bytes, over the {} byte limit",
            bytes.len(),
            halod_shared::types::MAX_PLUGIN_ASSET_BYTES
        ));
    }
    match image::load_from_memory(&bytes) {
        Ok(img) => halod_shared::types::validate_logo_dimensions(img.width(), img.height()).err(),
        Err(e) => Some(format!("logo is not a decodable image: {e}")),
    }
}

/// Enforce the logo bounds at load: drop a declared logo that's too big or the
/// wrong shape to `None` and record a warning, so the GUI never advertises or
/// requests an asset it would only distort or choke on.
fn validate_logo(dir: &Path, manifest: &mut PluginManifest) {
    let Some(name) = manifest.logo.clone() else {
        return;
    };
    if let Some(reason) = logo_rejection(dir, &name) {
        log::warn!(
            "Ignoring logo for plugin '{}': {reason}",
            manifest.plugin_id
        );
        write_recover(&LOAD_WARNINGS).push(PluginLoadWarning {
            plugin_id: manifest.plugin_id.clone(),
            path: dir.join("assets").join(&name).display().to_string(),
            reason,
        });
        manifest.logo = None;
    }
}

/// Parse `dir` as one plugin directory and push it into `out`, skipping (never failing) a bad manifest.
fn try_load_plugin_dir(dir: &Path, out: &mut Vec<PluginManifest>) {
    if !dir.join("plugin.yaml").is_file() {
        return;
    }
    match parse_manifest_from_dir(dir) {
        Ok(m) if out.iter().any(|e| e.plugin_id == m.plugin_id) => {
            let reason = format!("id '{}' is already claimed by another source", m.plugin_id);
            log::warn!("Ignoring plugin {}: {reason}", dir.display());
            write_recover(&LOAD_WARNINGS).push(PluginLoadWarning {
                plugin_id: m.plugin_id,
                path: dir.display().to_string(),
                reason,
            });
        }
        Ok(mut m) => {
            validate_logo(dir, &mut m);
            log::info!(
                "Loaded device plugin '{}' from {}",
                m.plugin_id,
                dir.display()
            );
            out.push(m);
        }
        Err(e) => log::warn!("Skipping plugin {}: {e:#}", dir.display()),
    }
}

/// Scan every immediate subdirectory of `root` that contains a `plugin.yaml`.
fn scan_plugin_subdirs(root: &Path, out: &mut Vec<PluginManifest>) {
    match std::fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    try_load_plugin_dir(&path, out);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!("No plugins directory at {}", root.display());
        }
        Err(e) => log::warn!("Cannot read plugins directory {}: {e}", root.display()),
    }
}

/// Load a git-repo plugin source: a single plugin at `repo_dir`, packages as
/// immediate sibling subdirectories of `repo_dir`, and/or nested under a
/// `plugins/` subdirectory — a repo may use any combination of the three.
fn scan_repo(repo_dir: &Path, out: &mut Vec<PluginManifest>) {
    try_load_plugin_dir(repo_dir, out);
    scan_plugin_subdirs(repo_dir, out);
    scan_plugin_subdirs(&repo_dir.join("plugins"), out);
}

/// Plugin ids discoverable under a repo's clone directory, for purging a removed repo's state.
pub fn repo_plugin_ids(repo_dir: &Path) -> Vec<String> {
    let mut manifests = Vec::new();
    scan_repo(repo_dir, &mut manifests);
    manifests.into_iter().map(|m| m.plugin_id).collect()
}

/// Every configured repo's checked-out clone directory, for [`load_all_with_repos`].
pub fn repo_plugin_dirs(repos: &[crate::config::PluginRepoRecord]) -> Vec<std::path::PathBuf> {
    repos
        .iter()
        .map(|r| crate::config::plugin_repos_dir().join(&r.slug))
        .collect()
}

/// [`load_all_with_repos`] with no configured repos.
pub fn load_all(dir: &Path) {
    load_all_with_repos(dir, &[]);
}

/// (Re)load the local and git-repo (`repo_dirs`) plugins, replacing prior
/// contents. Load order is security-ranked: the official repo first, then
/// local `plugins/`, then other repos in config order — so an id is owned by
/// whichever source provides it first and no later source can shadow it (see
/// [`try_load_plugin_dir`]'s collision handling).
pub fn load_all_with_repos(dir: &Path, repo_dirs: &[std::path::PathBuf]) {
    write_recover(&LOAD_WARNINGS).clear();
    let mut manifests = Vec::new();
    let is_official = |d: &std::path::PathBuf| {
        d.file_name().and_then(|n| n.to_str()) == Some(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
    };
    for repo_dir in repo_dirs.iter().filter(|d| is_official(d)) {
        scan_repo(repo_dir, &mut manifests);
    }
    scan_plugin_subdirs(dir, &mut manifests);
    for repo_dir in repo_dirs.iter().filter(|d| !is_official(d)) {
        scan_repo(repo_dir, &mut manifests);
    }
    let effects: Vec<PluginEffectEntry> = manifests.iter().flat_map(effect_entries_for).collect();
    update(|s| {
        s.manifests = manifests;
        s.effects = effects;
    });
}

/// Stable device id from a matched manifest + handle (suffix per transport).
fn device_id(
    manifest: &PluginManifest,
    spec: &manifest::DeviceSpec,
    handle: &DiscoveryHandle<'_>,
) -> String {
    let suffix = transport::descriptor_for(&spec.transport)
        .map(|d| (d.id_suffix)(handle))
        .unwrap_or_else(|| "0".to_owned());
    format!("{}-{}", manifest.id_prefix(), suffix)
}

/// Build a device from a matched manifest, the spec that matched, and the
/// handle. Device-only plugins need no runtime/transport; capability plugins
/// open their transport and spawn a worker. Returns `None` if the transport
/// can't be opened (so a native driver can still claim the hardware).
fn build_device(
    manifest: &PluginManifest,
    spec: &manifest::DeviceSpec,
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    let id = device_id(manifest, spec, handle);
    if !manifest.needs_worker() {
        return Some(Arc::new(LuaDevice::device_only(id, manifest, spec)));
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn!(
            "plugin '{}' needs a worker but no runtime is available",
            manifest.plugin_id
        );
        return None;
    };
    let granted = granted_for(&manifest.plugin_id);
    let config = resolved_config_for(&manifest.plugin_id, &granted);
    let transport = match transport::descriptor_for(&spec.transport)
        .map(|d| (d.open)(manifest, handle, &config, &granted))
    {
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            log::warn!(
                "plugin '{}' transport open failed: {e:#}",
                manifest.plugin_id
            );
            return None;
        }
        None => return None,
    };

    let dev_match = worker::DevMatch {
        transport: spec.transport.clone(),
        bus: spec.bus.clone(),
        addr: match handle {
            DiscoveryHandle::Smbus { addr, .. } => Some(*addr),
            _ => None,
        },
        pid: match handle {
            DiscoveryHandle::Hid { pid, .. } => Some(*pid),
            DiscoveryHandle::UsbNonHid { pid, .. } => Some(*pid),
            _ => None,
        },
        index: None,
    };

    // `new_cyclic` so the device can hand its children a `FanHub` back-reference
    // for the chain machinery (e.g. an NZXT Kraken/Control Hub accessory fan).
    let device = Arc::new_cyclic(|weak| {
        let mut dev = LuaDevice::with_transport(id, manifest, spec, dev_match, transport, runtime);
        dev.set_self_ref(weak.clone());
        dev
    });
    if manifest.chain.is_some() {
        let adapter: Arc<dyn crate::drivers::chain::ChainAdapter> = device.clone();
        let host = crate::drivers::chain::ChainHost::new(adapter);
        device.install_chain_host(host);
    }
    Some(device as Arc<dyn Device>)
}

/// Match a handle against a given manifest slice (pure — used by tests and by
/// [`match_handle`]).
pub fn match_in(
    manifests: &[PluginManifest],
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    manifests
        .iter()
        .filter(|m| !is_disabled(&m.plugin_id) && consent_satisfied(m))
        .find_map(|m| m.device_for(handle).map(|spec| (m, spec)))
        .and_then(|(m, spec)| build_device(m, spec, handle))
}

/// Match a discovery handle against every loaded plugin. Consulted by
/// `make_device` *before* the native descriptors so a plugin shadows native.
pub fn match_handle(handle: &DiscoveryHandle<'_>) -> Option<Arc<dyn Device>> {
    // The snapshot is a frozen `Arc`, not a lock guard, so `build_device` can
    // freely take its own snapshots (via `config_for` / `secure_config_keys_for`)
    // with no risk of a recursive-read deadlock, and no manifest/spec clone is
    // needed — both borrow from the snapshot that outlives the call.
    let state = snapshot();
    let (manifest, spec) = state
        .manifests
        .iter()
        .filter(|m| !state.disabled.contains(&m.plugin_id) && consent_satisfied(m))
        .find_map(|m| m.device_for(handle).map(|spec| (m, spec)))?;
    build_device(manifest, spec, handle)
}

pub fn has_match(handle: &DiscoveryHandle<'_>) -> bool {
    let state = snapshot();
    state
        .manifests
        .iter()
        .filter(|m| !state.disabled.contains(&m.plugin_id) && consent_satisfied(m))
        .any(|m| m.device_for(handle).is_some())
}

/// Collect every [`DeviceSpec`] declared by the named plugins' manifests
/// (current registry snapshot). Used to build a scoped [`DiscoveryFilter`].
pub fn device_specs_for(plugin_ids: &[String]) -> Vec<DeviceSpec> {
    let state = snapshot();
    state
        .manifests
        .iter()
        .filter(|m| plugin_ids.contains(&m.plugin_id))
        .flat_map(|m| m.devices.clone())
        .collect()
}

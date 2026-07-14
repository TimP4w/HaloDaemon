// SPDX-License-Identifier: GPL-3.0-or-later
//! Device plugins: add a device without recompiling the daemon.
//!
//! A plugin is a directory package whose `plugin.yaml` declares hardware,
//! identity, permissions, transports, and capabilities. Its Lua entry contains
//! callbacks that turn capability calls into transport bytes and is not
//! evaluated during manifest loading. Plugins expose
//! only *existing* capability kinds; Halo owns the capability taxonomy.
//!
//! Registration is at runtime (not the compile-time `inventory` path native
//! drivers use): `load_all` reads the plugins directory into the registry
//! snapshot, and `make_device` consults `match_handle` before the native
//! descriptors, so a plugin shadows a native driver for the same hardware.

mod audio_api;
pub(crate) mod backends;
mod bytebuf;
mod chain_leaf;
mod device;
mod effect_worker;
mod ffi;
mod image_api;
pub(crate) mod integration_monitor;
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
pub use worker::run_pre_scan;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use halod_shared::types::{
    Animation, EffectParamValue, Permission, PluginInfo, PluginIssue, PluginIssueKind, PluginKind,
    SkippedPlugin, WriteRateLimit,
};

use crate::drivers::Device;
use crate::registry::discovery::DiscoveryHandle;

mod scan;

/// Heap cap for a plugin VM and ceiling on any single plugin-driven native
/// allocation; shared by the device worker, effect worker, and `bytebuf`.
pub(crate) const PLUGIN_VM_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Instruction budget per plugin callback / effect render, reset per call.
pub(crate) const PLUGIN_INSTRUCTION_BUDGET: u64 = 50_000_000;

/// A callback failure already reported through `PluginRuntimeError`. The IPC
/// layer uses this marker to avoid a second generic stack-trace toast.
#[derive(Debug)]
pub(crate) struct SurfacedPluginError {
    pub plugin: String,
    pub detail: String,
}

impl std::fmt::Display for SurfacedPluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plugin '{}' failed: {}", self.plugin, self.detail)
    }
}

impl std::error::Error for SurfacedPluginError {}

#[cfg(test)]
mod tests;

fn declared_write_rate_limit(max_bytes_per_sec: Option<u32>) -> Option<WriteRateLimit> {
    max_bytes_per_sec.map(|max_bytes_per_sec| WriteRateLimit { max_bytes_per_sec })
}

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

/// The validated inputs a device spawn needs, produced by
/// [`Registry::activation_status`] only after the consent gate passes.
/// [`Registry::build_device`] accepts one of these, so it is unreachable without
/// consent.
struct ReadyPlugin {
    granted: Vec<Permission>,
    config: HashMap<String, String>,
}

/// Whether a plugin may activate, from [`Registry::activation_status`].
enum ActivationState {
    /// The user disabled the plugin.
    Disabled,
    /// Declared permissions have not been granted.
    AwaitingConsent,
    /// Cleared to spawn, carrying its resolved grants + config.
    Ready(ReadyPlugin),
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
    /// Installed package hashes are update metadata, not an execution gate.
    installed_hashes: HashMap<String, String>,
    /// Non-secure config values the user set per plugin. Secure values never
    /// live here — see the secret store.
    config_values: HashMap<String, HashMap<String, String>>,
}

/// The device-plugin registry: every piece of runtime-mutable plugin state the
/// daemon owns. Held as [`crate::state::AppState::registry`] (one per process in
/// production, one per `AppState` in tests — so tests are isolated without a
/// shared-globals lock). The native driver registry it composes with is the
/// compile-time `inventory` set consulted by [`crate::registry::discovery`];
/// [`Registry::make_device`] tries plugins first (so a plugin shadows a native
/// driver) then falls back to those native descriptors.
#[derive(Default)]
pub struct Registry {
    /// The read-mostly plugin snapshot (manifests/effects/consent/config).
    plugins: RwLock<Arc<PluginState>>,
    /// Plugin ids already surfaced via a "needs permission" notice (see
    /// [`Registry::take_newly_ungranted_plugins`]).
    notified: RwLock<HashSet<String>>,
    /// Device ids with an outstanding runtime error, so a plugin that fails every
    /// engine tick is announced once per failure episode, not on every frame.
    failing_devices: RwLock<HashSet<String>>,
    /// Plugin ids with an outstanding connect failure, so a persistently
    /// unreachable integration (the reconnect watcher retries on a backoff)
    /// alerts once per failure episode, not every tick. Keyed by plugin_id.
    connect_failed: RwLock<HashSet<String>>,
    /// Each plugin's most recent outstanding issue, surfaced to the GUI via
    /// [`PluginInfo::issue`] so it persists on the plugin page and the sidebar
    /// badge past the transient toast. Keyed by plugin_id.
    issues: RwLock<HashMap<String, PluginIssue>>,
    invalid_manifests: RwLock<Vec<(PluginManifest, PluginIssue)>>,
    skipped: RwLock<Vec<SkippedPlugin>>,
    /// Rejected-load warnings retained for validation tests.
    #[cfg(test)]
    load_warnings: RwLock<Vec<PluginLoadWarning>>,
}

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

impl Registry {
    /// The current registry snapshot. The lock is held only for the `Arc` clone,
    /// never across the caller's use of the data — so re-entrant reads can't deadlock.
    fn snapshot(&self) -> Arc<PluginState> {
        read_recover(&self.plugins).clone()
    }

    /// Swap in a new snapshot by applying `f` to a clone of the current one. Held
    /// under the write lock so concurrent mutators can't lose each other's edits;
    /// `f` only mutates fields (never re-locks), so it cannot deadlock.
    fn update(&self, f: impl FnOnce(&mut PluginState)) {
        let mut guard = write_recover(&self.plugins);
        let mut next = (**guard).clone();
        f(&mut next);
        *guard = Arc::new(next);
    }
}

fn consent_satisfied_in(state: &PluginState, manifest: &PluginManifest) -> bool {
    if manifest.permissions.is_empty() {
        return true;
    }
    let granted = state
        .granted
        .get(&manifest.plugin_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    manifest
        .permissions
        .iter()
        .all(|permission| granted.contains(permission))
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

impl Registry {
    /// Whether the plugin owning an effect has granted all of its declared
    /// permissions. Installed content hashes intentionally do not gate code.
    fn effect_consent_ok(state: &PluginState, plugin_id: &str) -> bool {
        state
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .is_some_and(|m| !state.disabled.contains(plugin_id) && consent_satisfied_in(state, m))
    }

    /// Every enabled, consent-satisfied plugin's declared descriptors of one effect
    /// kind, for the RGB engine's dynamic catalog.
    fn effect_descriptors(&self, kind: EffectKind) -> Vec<Animation> {
        let state = self.snapshot();
        state
            .effects
            .iter()
            .filter(|e| e.kind == kind && !state.disabled.contains(&e.plugin_id))
            .filter(|e| Self::effect_consent_ok(&state, &e.plugin_id))
            .map(|e| e.descriptor.clone())
            .collect()
    }

    /// Descriptors for every enabled plugin-declared pixmap effect.
    pub fn pixmap_effect_descriptors(&self) -> Vec<Animation> {
        self.effect_descriptors(EffectKind::Pixmap)
    }

    /// Descriptors for every enabled plugin-declared direct effect.
    pub fn direct_effect_descriptors(&self) -> Vec<Animation> {
        self.effect_descriptors(EffectKind::Direct)
    }

    /// Look up a registered effect entry by its namespaced catalog id. `None` if
    /// unknown, disabled, or its plugin's consent is no longer satisfied.
    pub fn effect_entry(&self, catalog_id: &str) -> Option<PluginEffectEntry> {
        let state = self.snapshot();
        state
            .effects
            .iter()
            .find(|e| {
                e.catalog_id == catalog_id
                    && !state.disabled.contains(&e.plugin_id)
                    && Self::effect_consent_ok(&state, &e.plugin_id)
            })
            .cloned()
    }

    /// Spawn a worker for a registered pixmap effect. `None` for an unknown,
    /// disabled, or wrong-kind id — the caller falls back to a native default.
    pub fn build_pixmap_effect(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        catalog_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) -> Option<PluginEffectHandle> {
        self.build_effect_handle(secrets, EffectKind::Pixmap, catalog_id, params)
    }

    /// Spawn a worker for a registered direct effect. `None` for an unknown,
    /// disabled, or wrong-kind id — the caller falls back to a native default.
    pub fn build_direct_effect(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        catalog_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) -> Option<PluginEffectHandle> {
        self.build_effect_handle(secrets, EffectKind::Direct, catalog_id, params)
    }

    fn build_effect_handle(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        kind: EffectKind,
        catalog_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) -> Option<PluginEffectHandle> {
        let entry = self.effect_entry(catalog_id)?;
        if entry.kind != kind {
            return None;
        }
        let effect_id = catalog_id
            .strip_prefix(&format!("{}:", entry.plugin_id))?
            .to_string();
        let granted = self.granted_for(&entry.plugin_id);
        let config = self.resolved_config_for(secrets, &entry.plugin_id, &granted);
        Some(PluginEffectHandle::spawn(
            entry.script_source,
            effect_id,
            params.clone(),
            granted,
            config,
        ))
    }

    /// Atomically replace every persisted plugin-policy field in one snapshot.
    pub fn replace_policy(&self, policy: &crate::config::PluginPolicy) {
        self.update(|s| {
            s.disabled = policy.disabled.iter().cloned().collect();
            s.integrations_disabled = policy.integrations_disabled.iter().cloned().collect();
            s.granted = policy.granted.clone();
            s.installed_hashes = policy.installed_hashes.clone();
            s.config_values = policy.config.clone();
        });
    }

    /// Test-only field mutation helpers for focused registry state tests.
    #[cfg(test)]
    pub fn set_disabled(&self, ids: &[String]) {
        self.update(|s| {
            s.disabled = ids.iter().cloned().collect();
        });
    }

    fn is_disabled(&self, plugin_id: &str) -> bool {
        self.snapshot().disabled.contains(plugin_id)
    }

    #[cfg(test)]
    pub fn set_integrations_disabled(&self, ids: &[String]) {
        self.update(|s| s.integrations_disabled = ids.iter().cloned().collect());
    }

    fn is_integration_disabled(&self, plugin_id: &str) -> bool {
        self.snapshot().integrations_disabled.contains(plugin_id)
    }

    #[cfg(test)]
    pub fn set_granted(&self, granted: &HashMap<String, Vec<Permission>>) {
        self.update(|s| {
            s.granted = granted.clone();
        });
    }

    /// Permissions declared by `plugin_id`'s current manifest. Unknown plugin ids
    /// have no declared permissions.
    pub(crate) fn declared_permissions_for(&self, plugin_id: &str) -> Vec<Permission> {
        self.snapshot()
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .map(|m| m.permissions.clone())
            .unwrap_or_default()
    }

    /// Effective permissions granted to `plugin_id`'s Lua sandbox: persisted user
    /// grants intersected with the current manifest declaration. This is the
    /// authoritative capability boundary even if persisted config was edited or an
    /// internal caller supplied an undeclared permission.
    pub(crate) fn granted_for(&self, plugin_id: &str) -> Vec<Permission> {
        let state = self.snapshot();
        let Some(manifest) = state.manifests.iter().find(|m| m.plugin_id == plugin_id) else {
            return Vec::new();
        };
        state
            .granted
            .get(plugin_id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|permission| manifest.permissions.contains(permission))
            .collect()
    }

    #[cfg(test)]
    pub fn set_acknowledged(&self, hashes: &HashMap<String, String>) {
        self.update(|s| s.installed_hashes = hashes.clone());
    }

    /// The installed package hash for `plugin_id`, if any.
    fn installed_hash_for(&self, plugin_id: &str) -> Option<String> {
        self.snapshot().installed_hashes.get(plugin_id).cloned()
    }

    /// The current on-disk content hash for `plugin_id` from the loaded registry,
    /// for recording an acknowledgment when the user consents. `None` if unknown.
    pub fn content_hash_for(&self, plugin_id: &str) -> Option<String> {
        self.snapshot()
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .map(|m| m.content_hash())
    }

    #[cfg(test)]
    pub fn set_config_values(&self, values: &HashMap<String, HashMap<String, String>>) {
        self.update(|s| s.config_values = values.clone());
    }

    /// A plugin's resolved non-secure config: every declared field defaults to its
    /// manifest `default`, overridden by any value the user has set. Unknown keys
    /// the user may have stored (e.g. after a manifest edit removed a field) are
    /// not included — only keys the manifest still declares.
    pub fn config_for(&self, plugin_id: &str) -> HashMap<String, String> {
        let state = self.snapshot();
        match state.manifests.iter().find(|m| m.plugin_id == plugin_id) {
            Some(manifest) => config_values_for(&state, manifest),
            None => HashMap::new(),
        }
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
                .filter(|value| validate_config_value(f, value).is_ok())
                .cloned()
                .unwrap_or_else(|| f.default.clone());
            (f.key.clone(), value)
        })
        .collect()
}

/// Bounds a single configuration value at every ingress and again while
/// resolving persisted values for a plugin worker.  The second use makes stale
/// or hand-edited config inert instead of passing it to Lua or a transport.
fn validate_config_value(field: &manifest::ConfigFieldDef, value: &str) -> anyhow::Result<()> {
    use halod_shared::types::PluginConfigFieldKind;

    anyhow::ensure!(
        value.len() <= 4096 && !value.contains('\0'),
        "config '{}' exceeds the text bounds",
        field.key
    );
    if field.kind == PluginConfigFieldKind::Number && !value.is_empty() {
        let n: f64 = value
            .parse()
            .map_err(|_| anyhow::anyhow!("config '{}' must be a number", field.key))?;
        anyhow::ensure!(n.is_finite(), "config '{}' must be finite", field.key);
        if let Some(min) = field.min {
            anyhow::ensure!(
                n >= min,
                "config '{}' is below the minimum {min}",
                field.key
            );
        }
        if let Some(max) = field.max {
            anyhow::ensure!(
                n <= max,
                "config '{}' is above the maximum {max}",
                field.key
            );
        }
    }
    Ok(())
}

impl Registry {
    /// Record a plugin's most recent outstanding issue for the GUI's plugin page
    /// / sidebar badge (last-writer-wins; one issue per plugin).
    fn set_issue(&self, plugin_id: &str, kind: PluginIssueKind, detail: String) {
        write_recover(&self.issues).insert(
            plugin_id.to_owned(),
            PluginIssue {
                kind,
                detail,
                timestamp_ms: crate::util::time::now_ms(),
            },
        );
    }

    /// Clear a plugin's outstanding issue only when it is of `kind`, so clearing
    /// (say) a recovered runtime error can't wipe an unrelated load warning.
    fn clear_issue_of(&self, plugin_id: &str, kind: PluginIssueKind) {
        let mut issues = write_recover(&self.issues);
        if issues.get(plugin_id).is_some_and(|i| i.kind == kind) {
            issues.remove(plugin_id);
        }
    }

    /// The plugin's current outstanding issue, if any (for [`Registry::list`]).
    fn issue_for(&self, plugin_id: &str) -> Option<PluginIssue> {
        read_recover(&self.issues).get(plugin_id).cloned()
    }

    /// Surface a plugin device's runtime callback failure to the user as a
    /// [`NotificationCode::PluginRuntimeError`], once per failure episode (deduped
    /// on `device_id`). The issue is also persisted per-plugin for the GUI page.
    pub(super) async fn report_runtime_error(
        &self,
        app: &Arc<crate::state::AppState>,
        plugin_id: &str,
        device_id: &str,
        detail: String,
    ) {
        // A callback may finish after either toggle was switched off. Its
        // result belongs to the old activation episode and must not resurrect
        // an issue that the disable path just cleared.
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            return;
        }
        self.set_issue(plugin_id, PluginIssueKind::RuntimeError, detail.clone());
        // Close the check/write race: if disable landed between the first
        // policy read and `set_issue`, remove the stale write before anything
        // can observe or broadcast it. If disable starts after this check, its
        // own cleanup runs later and wins instead.
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_issue_of(plugin_id, PluginIssueKind::RuntimeError);
            return;
        }
        if !write_recover(&self.failing_devices).insert(device_id.to_owned()) {
            return; // already reported this episode
        }
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            write_recover(&self.failing_devices).remove(device_id);
            self.clear_issue_of(plugin_id, PluginIssueKind::RuntimeError);
            return;
        }
        crate::platform::notify::send(
            app,
            halod_shared::types::NotificationCode::PluginRuntimeError {
                plugin: plugin_id.to_owned(),
                detail,
            },
        )
        .await;
    }

    /// Clear a device's outstanding-error flag after a successful call. On the
    /// failing→ok transition the plugin's persisted runtime issue is cleared too.
    pub(super) fn clear_runtime_error(&self, plugin_id: &str, device_id: &str) {
        if write_recover(&self.failing_devices).remove(device_id) {
            self.clear_issue_of(plugin_id, PluginIssueKind::RuntimeError);
        }
    }

    /// Surface a config-instantiated integration plugin's connect failure as a
    /// [`NotificationCode::PluginConnectFailed`], once per failure episode
    /// (deduped on `plugin_id`). The issue is also persisted for the GUI page.
    pub(super) async fn report_connect_error(
        &self,
        app: &Arc<crate::state::AppState>,
        plugin_id: &str,
        display_name: &str,
        detail: String,
    ) {
        // The blocking connect may complete after the integration/plugin was
        // disabled. Discard that stale completion instead of restoring its
        // cleared card/sidebar warning.
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            return;
        }
        self.set_issue(plugin_id, PluginIssueKind::ConnectFailed, detail.clone());
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_issue_of(plugin_id, PluginIssueKind::ConnectFailed);
            return;
        }
        if !write_recover(&self.connect_failed).insert(plugin_id.to_owned()) {
            return; // already reported this episode
        }
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            write_recover(&self.connect_failed).remove(plugin_id);
            self.clear_issue_of(plugin_id, PluginIssueKind::ConnectFailed);
            return;
        }
        crate::platform::notify::send(
            app,
            halod_shared::types::NotificationCode::PluginConnectFailed {
                plugin: display_name.to_owned(),
                detail,
            },
        )
        .await;
    }

    /// Clear a plugin's outstanding connect-error flag after a successful connect.
    pub(super) fn clear_connect_error(&self, plugin_id: &str) {
        write_recover(&self.connect_failed).remove(plugin_id);
        self.clear_issue_of(plugin_id, PluginIssueKind::ConnectFailed);
    }

    /// Clear all operational error state when a plugin or integration is
    /// deliberately stopped. Removing episode-dedup keys lets a later re-enable
    /// report a genuinely fresh failure, while clearing the persisted issue
    /// immediately removes its card/sidebar warning.
    pub(crate) fn clear_operational_errors(&self, plugin_id: &str, device_ids: &[String]) {
        write_recover(&self.connect_failed).remove(plugin_id);
        {
            let mut failing = write_recover(&self.failing_devices);
            for device_id in device_ids {
                failing.remove(device_id);
            }
        }
        let mut issues = write_recover(&self.issues);
        if issues.get(plugin_id).is_some_and(|issue| {
            matches!(
                issue.kind,
                PluginIssueKind::ConnectFailed | PluginIssueKind::RuntimeError
            )
        }) {
            issues.remove(plugin_id);
        }
    }

    /// Record a non-fatal load warning (bad logo, id collision) as a persisted
    /// plugin issue, surfaced on the plugin page / sidebar without a toast.
    fn set_load_warning(&self, plugin_id: &str, reason: String) {
        self.set_issue(plugin_id, PluginIssueKind::LoadWarning, reason);
    }

    /// Drop any stale load-warning issues before a reload re-derives them, so a
    /// warning the user has since fixed doesn't linger. Runtime/connect issues
    /// (a different `kind`) are untouched.
    fn clear_load_warnings(&self) {
        write_recover(&self.issues).retain(|_, i| i.kind != PluginIssueKind::LoadWarning);
    }

    /// A plugin's full resolved config for its Lua VM: `config_for` plus, only
    /// when `Permission::SecureStorage` is granted, its decrypted secure values.
    pub fn resolved_config_for(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        plugin_id: &str,
        granted: &[Permission],
    ) -> HashMap<String, String> {
        let mut config = self.config_for(plugin_id);
        if !granted.contains(&Permission::SecureStorage) {
            return config;
        }
        let snapshot = self.snapshot();
        let manifest = snapshot.manifests.iter().find(|m| m.plugin_id == plugin_id);
        for key in self.secure_config_keys_for(plugin_id) {
            match secrets.get(plugin_id, &key) {
                Ok(Some(value)) => {
                    let valid = manifest
                        .as_ref()
                        .and_then(|m| m.config_fields().iter().find(|f| f.key == key))
                        .is_some_and(|field| validate_config_value(field, &value).is_ok());
                    if valid {
                        config.insert(key, value);
                    } else {
                        log::warn!("ignoring invalid persisted secret config '{key}' for plugin '{plugin_id}'");
                    }
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
    pub fn secure_config_keys_for(&self, plugin_id: &str) -> Vec<String> {
        self.snapshot()
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

    /// Reject config values whose key isn't a declared field, or whose `Number`
    /// value doesn't parse or falls outside the field's declared `min`/`max`.
    pub fn validate_config_values(
        &self,
        plugin_id: &str,
        values: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let snap = self.snapshot();
        let manifest = snap
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin '{plugin_id}'"))?;
        let fields = manifest.config_fields();
        for (key, value) in values {
            let field = fields.iter().find(|f| &f.key == key).ok_or_else(|| {
                anyhow::anyhow!("unknown config key '{key}' for plugin '{plugin_id}'")
            })?;
            validate_config_value(field, value)?;
        }
        Ok(())
    }

    /// True when the plugin's declared permissions have all been granted. Being
    /// permission-satisfied is
    /// necessary but not sufficient to activate — see [`Registry::activation_status`],
    /// which also accounts for the disabled set.
    fn consent_satisfied(&self, manifest: &PluginManifest) -> bool {
        consent_satisfied_in(&self.snapshot(), manifest)
    }

    /// Why `manifest` is not permission-satisfied.
    fn ungranted_reason(&self, manifest: &PluginManifest) -> UngrantedReason {
        let _ = manifest;
        UngrantedReason::NeedsPermission
    }

    /// The single gate for "may this plugin activate, and with what?": disabled,
    /// awaiting consent (with the reason), or [`Ready`](ActivationState::Ready) with
    /// the resolved grants + config a spawn needs. Every device spawn resolves
    /// through here and [`build_device`](Registry::build_device) accepts only a
    /// [`ReadyPlugin`], so a device can't be built without passing the consent gate
    /// (the ARCH-N1 class of bug becomes unrepresentable rather than checked by
    /// convention).
    fn activation_status(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        manifest: &PluginManifest,
    ) -> ActivationState {
        if self.is_disabled(&manifest.plugin_id) {
            ActivationState::Disabled
        } else if !self.consent_satisfied(manifest) {
            ActivationState::AwaitingConsent
        } else {
            let granted = self.granted_for(&manifest.plugin_id);
            let config = self.resolved_config_for(secrets, &manifest.plugin_id, &granted);
            ActivationState::Ready(ReadyPlugin { granted, config })
        }
    }
}

impl Registry {
    /// Every enabled, permission-satisfied `Integration` plugin, for the
    /// integration `TransportScanner` (`integration_scan.rs`) — it has no
    /// `DiscoveryHandle` to match against, so it iterates these directly instead
    /// of going through `match_handle`.
    pub(super) fn integration_manifests(&self) -> Vec<PluginManifest> {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| {
                m.plugin_type == PluginKind::Integration
                    && !state.integrations_disabled.contains(&m.plugin_id)
                    && !state.disabled.contains(&m.plugin_id)
                    && consent_satisfied_in(&state, m)
            })
            .cloned()
            .collect()
    }

    /// The single enabled, permission-satisfied `Integration` manifest for
    /// `plugin_id`, for a scoped reconnect of just that one integration. `None`
    /// if it's missing, plugin-disabled, integration-disabled, or ungranted.
    pub(super) fn integration_manifest(&self, plugin_id: &str) -> Option<PluginManifest> {
        self.integration_manifests()
            .into_iter()
            .find(|m| m.plugin_id == plugin_id)
    }

    /// Suppress the auto-discovery notification for a plugin id — used when the
    /// GUI is already showing its own consent modal (a manual "Add plugin"
    /// import), so the user isn't told about it twice.
    pub fn suppress_permission_notice(&self, plugin_id: &str) {
        write_recover(&self.notified).insert(plugin_id.to_owned());
    }
}

/// Why a plugin can't currently activate, so the daemon picks the right alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UngrantedReason {
    /// Never approved (or explicitly revoked): the user must grant permissions.
    NeedsPermission,
}

impl Registry {
    /// Filter behind [`Registry::take_newly_ungranted_plugins`]: which manifests
    /// can't activate and aren't yet in `notified` (inserting each as returned),
    /// paired with the reason so the caller can choose the notification.
    fn ungranted_in(
        &self,
        manifests: &[PluginManifest],
        notified: &mut HashSet<String>,
    ) -> Vec<(String, UngrantedReason)> {
        manifests
            .iter()
            .filter(|m| !self.consent_satisfied(m) && notified.insert(m.plugin_id.clone()))
            .map(|m| (m.display_name().to_owned(), self.ungranted_reason(m)))
            .collect()
    }

    /// Display names (with reason) of plugins that can't activate and haven't been
    /// announced yet. Marks every returned plugin as notified.
    pub fn take_newly_ungranted_plugins(&self) -> Vec<(String, UngrantedReason)> {
        let state = self.snapshot();
        let mut notified = write_recover(&self.notified);
        self.ungranted_in(&state.manifests, &mut notified)
    }
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
impl Registry {
    pub fn repo_location_for(&self, plugin_id: &str) -> Option<(String, std::path::PathBuf)> {
        let state = self.snapshot();
        // Keep repo operations available for a manifest that was parsed far
        // enough to identify but failed semantic validation. Updating is the
        // recovery path when a Halo/plugin-API change makes the checked-out
        // version invalid, so restricting this lookup to successfully loaded
        // manifests would strand exactly the plugins that need it most.
        let invalid = read_recover(&self.invalid_manifests);
        let manifest = state
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .or_else(|| {
                invalid
                    .iter()
                    .map(|(manifest, _)| manifest)
                    .find(|m| m.plugin_id == plugin_id)
            })?;
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
    pub fn list(&self, secrets: &dyn crate::secrets::SecretStore) -> Vec<PluginInfo> {
        let state = self.snapshot();
        let mut infos: Vec<PluginInfo> = state
            .manifests
            .iter()
            .map(|m| self.build_plugin_info(m, &state, secrets))
            .collect();
        let loaded: HashSet<&str> = state
            .manifests
            .iter()
            .map(|m| m.plugin_id.as_str())
            .collect();
        for (m, issue) in read_recover(&self.invalid_manifests).iter() {
            if loaded.contains(m.plugin_id.as_str()) {
                continue;
            }
            let mut info = self.build_plugin_info(m, &state, secrets);
            info.enabled = false;
            info.consented = false;
            info.integration_enabled = false;
            info.issue = Some(issue.clone());
            info.integration_issue = None;
            infos.push(info);
        }
        infos
    }

    fn build_plugin_info(
        &self,
        m: &PluginManifest,
        state: &PluginState,
        secrets: &dyn crate::secrets::SecretStore,
    ) -> PluginInfo {
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
        let consented = self.consent_satisfied(m);
        let stored_issue = self.issue_for(&m.plugin_id);
        let integration_issue = stored_issue.clone().filter(|issue| {
            issue.kind == PluginIssueKind::ConnectFailed
                || (m.plugin_type == PluginKind::Integration
                    && issue.kind == PluginIssueKind::RuntimeError)
        });
        let issue = stored_issue.filter(|_| integration_issue.is_none());
        PluginInfo {
            id: m.plugin_id.clone(),
            name: m.display_name(),
            path: m.source_path.display().to_string(),
            plugin_type: m.plugin_type,
            capabilities: m.capability_labels(),
            effect_names: m.effects.iter().map(|e| e.name.clone()).collect(),
            enabled: !self.is_disabled(&m.plugin_id) && consented,
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
            provenance: match plugin_source_for(&m.plugin_dir) {
                halod_shared::types::PluginSource::Repo { ref slug }
                    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG =>
                {
                    halod_shared::types::PluginProvenance::VerifiedOfficial
                }
                halod_shared::types::PluginSource::Repo { .. } => {
                    halod_shared::types::PluginProvenance::UnsignedRepository
                }
                halod_shared::types::PluginSource::Local => {
                    halod_shared::types::PluginProvenance::LocalUnsigned
                }
            },
            declared_permissions: m.permissions.clone(),
            granted_permissions: self.granted_for(&m.plugin_id),
            config_fields: m.config_fields().iter().map(Into::into).collect(),
            config_values: config_values_for(state, m),
            secret_set,
            integration_enabled: !self.is_integration_disabled(&m.plugin_id),
            consented,
            content_changed: self
                .installed_hash_for(&m.plugin_id)
                .is_some_and(|h| h != m.content_hash()),
            issue,
            integration_issue,
        }
    }

    /// Read a plugin's display-only asset (logo/effect thumbnail) from `<plugin_dir>/assets/<name>`.
    pub fn read_asset(&self, plugin_id: &str, name: &str) -> anyhow::Result<Vec<u8>> {
        halod_shared::types::validate_image_filename(name)
            .map_err(|e| anyhow::anyhow!("invalid asset name '{name}': {e}"))?;
        let state = self.snapshot();
        let manifest = state
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin '{plugin_id}'"))?;
        if manifest.plugin_dir.as_os_str().is_empty() {
            anyhow::bail!("plugin '{plugin_id}' has no on-disk assets");
        }
        let path = manifest.plugin_dir.join("assets").join(name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("asset '{name}' is a symlink");
        }
        let len = meta.len();
        if len > halod_shared::types::MAX_PLUGIN_ASSET_BYTES {
            anyhow::bail!(
                "asset '{name}' is {len} bytes, over the {} byte limit",
                halod_shared::types::MAX_PLUGIN_ASSET_BYTES
            );
        }
        let data =
            std::fs::read(&path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        // Bound dimensions before the bytes reach the GUI's unbounded decoder.
        crate::util::image::decode_limited(&data)
            .map_err(|e| anyhow::anyhow!("asset '{name}' is not a decodable image: {e}"))?;
        Ok(data)
    }
}

/// A rejected plugin load: an id collision (with an earlier source) or a bad manifest.
/// An id is owned by whichever source loads it first (official repo, then local
/// `plugins/`, then other repos); a later source declaring the same id is
/// rejected and surfaced here instead of silently dropped.
#[derive(Clone, Debug)]
pub struct PluginLoadWarning {
    pub plugin_id: String,
    pub path: String,
    pub reason: String,
}

/// Reason a declared logo is unusable, or `None` if it passes every bound.
/// A logo whose file is absent isn't rejected here — it's left for the GUI's
/// initials fallback; only a *present* logo is held to the size/shape bounds.
fn logo_rejection(dir: &Path, name: &str) -> Option<String> {
    // Validate the name before touching the filesystem: an unchecked `logo:` like
    // `../../../etc/shadow` would otherwise read an arbitrary root-readable file and
    // leak its size/existence/decodability back through the load warning surfaced to
    // the GUI. `read_asset` already guards its fetch this way.
    if let Err(e) = halod_shared::types::validate_image_filename(name) {
        return Some(format!("logo name is invalid: {e}"));
    }
    let path = dir.join("assets").join(name);
    // Absent logo -> None (fall back to initials); symlink/oversize -> rejected.
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() {
        return Some("logo is a symlink".to_string());
    }
    if meta.len() > halod_shared::types::MAX_PLUGIN_ASSET_BYTES {
        return Some(format!(
            "logo is {} bytes, over the {} byte limit",
            meta.len(),
            halod_shared::types::MAX_PLUGIN_ASSET_BYTES
        ));
    }
    let bytes = std::fs::read(&path).ok()?;
    match crate::util::image::decode_limited(&bytes) {
        Ok(img) => halod_shared::types::validate_logo_dimensions(img.width(), img.height()).err(),
        Err(e) => Some(format!("logo is not a decodable image: {e}")),
    }
}

/// Enforce the logo bounds at load: drop a declared logo that's too big or the
/// wrong shape to `None` and record a warning, so the GUI never advertises or
/// requests an asset it would only distort or choke on.
fn validate_logo(dir: &Path, manifest: &mut PluginManifest, warnings: &mut Vec<PluginLoadWarning>) {
    let Some(name) = manifest.logo.clone() else {
        return;
    };
    if let Some(reason) = logo_rejection(dir, &name) {
        log::warn!(
            "Ignoring logo for plugin '{}': {reason}",
            manifest.plugin_id
        );
        warnings.push(PluginLoadWarning {
            plugin_id: manifest.plugin_id.clone(),
            path: dir.join("assets").join(&name).display().to_string(),
            reason,
        });
        manifest.logo = None;
    }
}

#[derive(Default)]
struct LoadScan {
    manifests: Vec<PluginManifest>,
    warnings: Vec<PluginLoadWarning>,
    invalid: Vec<(PluginManifest, String)>,
    skipped: Vec<SkippedPlugin>,
}

fn try_load_plugin_dir(dir: &Path, scan: &mut LoadScan) {
    if !dir.join("plugin.yaml").is_file() {
        return;
    }
    let manifest = match manifest::build_manifest_from_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            let reason = format!("{e:#}");
            log::warn!("Skipping plugin {}: {reason}", dir.display());
            scan.skipped.push(SkippedPlugin {
                path: dir.display().to_string(),
                reason,
            });
            return;
        }
    };
    if let Err(e) = manifest::validate_manifest(&manifest) {
        let reason = format!("{e:#}");
        log::warn!("Plugin {} is invalid: {reason}", dir.display());
        scan.invalid.push((manifest, reason));
        return;
    }
    if scan
        .manifests
        .iter()
        .any(|e| e.plugin_id == manifest.plugin_id)
    {
        let reason = format!(
            "id '{}' is already claimed by another source",
            manifest.plugin_id
        );
        log::warn!("Ignoring plugin {}: {reason}", dir.display());
        scan.warnings.push(PluginLoadWarning {
            plugin_id: manifest.plugin_id,
            path: dir.display().to_string(),
            reason,
        });
        return;
    }
    let mut m = manifest;
    validate_logo(dir, &mut m, &mut scan.warnings);
    log::info!(
        "Loaded device plugin '{}' from {}",
        m.plugin_id,
        dir.display()
    );
    scan.manifests.push(m);
}

/// Scan every immediate subdirectory of `root` that contains a `plugin.yaml`.
fn scan_plugin_subdirs(root: &Path, scan: &mut LoadScan) {
    match std::fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    try_load_plugin_dir(&path, scan);
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
fn scan_repo(repo_dir: &Path, scan: &mut LoadScan) {
    if let Ok(repository) = repo::read_repository_manifest(repo_dir) {
        for package in repository.packages {
            try_load_plugin_dir(&repo_dir.join(package.path), scan);
        }
        return;
    }
    try_load_plugin_dir(repo_dir, scan);
    scan_plugin_subdirs(repo_dir, scan);
    scan_plugin_subdirs(&repo_dir.join("plugins"), scan);
}

/// Plugin ids discoverable under a repo's clone directory, for purging a removed repo's state.
pub fn repo_plugin_ids(repo_dir: &Path) -> Vec<String> {
    let mut scan = LoadScan::default();
    scan_repo(repo_dir, &mut scan);
    scan.manifests.into_iter().map(|m| m.plugin_id).collect()
}

/// Every configured repo's checked-out clone directory, for [`Registry::load_all_with_repos`].
pub fn repo_plugin_dirs(repos: &[crate::config::PluginRepoRecord]) -> Vec<std::path::PathBuf> {
    repos
        .iter()
        .map(|r| crate::config::plugin_repos_dir().join(&r.slug))
        .collect()
}

impl Registry {
    /// Every load warning recorded during the most recent `load_all_with_repos`,
    /// draining the set so a later poll doesn't repeat it.
    #[cfg(test)]
    pub fn take_plugin_load_warnings(&self) -> Vec<PluginLoadWarning> {
        std::mem::take(&mut write_recover(&self.load_warnings))
    }

    /// [`Registry::load_all_with_repos`] with no configured repos.
    #[cfg(test)]
    pub fn load_all(&self, dir: &Path) {
        self.load_all_with_repos(dir, &[]);
    }

    /// (Re)load the local and git-repo (`repo_dirs`) plugins, replacing prior
    /// contents. Load order is security-ranked: the official repo first, then
    /// local `plugins/`, then other repos in config order — so an id is owned by
    /// whichever source provides it first and no later source can shadow it (see
    /// [`try_load_plugin_dir`]'s collision handling).
    pub fn load_all_with_repos(&self, dir: &Path, repo_dirs: &[std::path::PathBuf]) {
        self.load_all_with_priority_repo(dir, None, repo_dirs);
    }

    /// Load a directly supplied development repository ahead of the normal
    /// local and configured sources. It is intentionally not required to live
    /// under Halo's managed repository directory.
    pub fn load_all_with_priority_repo(
        &self,
        dir: &Path,
        priority_repo: Option<&Path>,
        repo_dirs: &[std::path::PathBuf],
    ) {
        let mut scan = LoadScan::default();
        let is_official = |d: &std::path::PathBuf| {
            d.file_name().and_then(|n| n.to_str())
                == Some(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
        };
        if let Some(repo_dir) = priority_repo {
            scan_repo(repo_dir, &mut scan);
        } else {
            for repo_dir in repo_dirs.iter().filter(|d| is_official(d)) {
                scan_repo(repo_dir, &mut scan);
            }
        }
        scan_plugin_subdirs(dir, &mut scan);
        for repo_dir in repo_dirs.iter().filter(|d| {
            !is_official(d) && priority_repo.is_none_or(|priority| d.as_path() != priority)
        }) {
            scan_repo(repo_dir, &mut scan);
        }
        let effects: Vec<PluginEffectEntry> =
            scan.manifests.iter().flat_map(effect_entries_for).collect();
        // Re-derive load-warning issues from scratch so a warning the user has
        // since fixed doesn't linger; only surface warnings for plugins that
        // actually loaded (the GUI lists nothing else).
        self.clear_load_warnings();
        let loaded: HashSet<&str> = scan
            .manifests
            .iter()
            .map(|m| m.plugin_id.as_str())
            .collect();
        for warning in &scan.warnings {
            log::warn!(
                "plugin '{}' rejected at {}: {}",
                warning.plugin_id,
                warning.path,
                warning.reason
            );
            if loaded.contains(warning.plugin_id.as_str()) {
                self.set_load_warning(&warning.plugin_id, warning.reason.clone());
            }
        }
        drop(loaded);
        let invalid = scan
            .invalid
            .into_iter()
            .map(|(m, reason)| {
                (
                    m,
                    PluginIssue {
                        kind: PluginIssueKind::LoadFailed,
                        detail: reason,
                        timestamp_ms: crate::util::time::now_ms(),
                    },
                )
            })
            .collect();
        *write_recover(&self.invalid_manifests) = invalid;
        *write_recover(&self.skipped) = scan.skipped;
        #[cfg(test)]
        {
            *write_recover(&self.load_warnings) = scan.warnings;
        }
        self.update(|s| {
            s.manifests = scan.manifests;
            s.effects = effects;
        });
    }

    pub fn skipped(&self) -> Vec<SkippedPlugin> {
        read_recover(&self.skipped).clone()
    }
}

/// Stable device id from a matched manifest + handle (suffix per transport).
fn device_id(
    manifest: &PluginManifest,
    spec: &manifest::DeviceSpec,
    handle: &DiscoveryHandle<'_>,
) -> String {
    let suffix = transport::descriptor_for(&spec.transport)
        .and_then(|d| d.id_suffix)
        .map(|f| f(handle))
        .unwrap_or_else(|| "0".to_owned());
    format!("{}-{}", manifest.id_prefix(), suffix)
}

impl Registry {
    /// Build the winning device for `handle` (a plugin shadows a native driver),
    /// else the native descriptor's device. The unified entry point over both the
    /// runtime plugin registry and the compile-time native registry.
    pub fn make_device(
        &self,
        app: &Arc<crate::state::AppState>,
        handle: DiscoveryHandle<'_>,
    ) -> Option<Arc<dyn Device>> {
        let identity = crate::registry::identity::identity_from_handle(&handle);
        let device = self
            .match_handle(app, &handle)
            .or_else(|| crate::registry::discovery::make_device_native_only(handle))?;
        let origin = device.conflict_origin();
        Some(Arc::new(crate::registry::identity::IdentifiedDevice::new(
            device, identity, origin,
        )))
    }

    /// Build a device from a matched manifest, the spec that matched, and the
    /// handle. Device-only plugins need no runtime/transport; capability plugins
    /// open their transport and spawn a worker. Returns `None` if the transport
    /// can't be opened (so a native driver can still claim the hardware).
    fn build_device(
        &self,
        app: &Arc<crate::state::AppState>,
        manifest: &PluginManifest,
        spec: &manifest::DeviceSpec,
        handle: &DiscoveryHandle<'_>,
        ready: ReadyPlugin,
    ) -> Option<Arc<dyn Device>> {
        let id = device_id(manifest, spec, handle);
        let notify = Arc::downgrade(app);
        if !manifest.needs_worker() {
            return Some(Arc::new(LuaDevice::device_only(id, manifest, spec, notify)));
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            log::warn!(
                "plugin '{}' needs a worker but no runtime is available",
                manifest.plugin_id
            );
            return None;
        };
        // The grants + resolved config were validated by `activation_status` when it
        // produced this `ReadyPlugin` — reuse them rather than re-resolving.
        let ReadyPlugin { granted, config } = ready;
        let write_rate_limit = declared_write_rate_limit(spec.max_bytes_per_sec);
        let runtime_state = Arc::new(std::sync::Mutex::new(
            device::RuntimeState::OpeningTransport,
        ));
        let transport = match transport::descriptor_for(&spec.transport)
            .map(|d| (d.open)(manifest, handle, &config, &granted, write_rate_limit))
        {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                *runtime_state.lock().unwrap() = device::RuntimeState::Closed;
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
            vid: match handle {
                DiscoveryHandle::Hid { vid, .. } => Some(*vid),
                DiscoveryHandle::UsbNonHid { vid, .. } => Some(*vid),
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
            let mut dev = LuaDevice::with_transport(
                id,
                manifest,
                spec,
                dev_match,
                transport,
                runtime,
                granted,
                config,
                notify,
                runtime_state,
            );
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

    /// Match a handle against a given manifest slice (consent checked against
    /// this registry's granted/acknowledged state). Used by tests.
    #[cfg(test)]
    pub fn match_in(
        &self,
        app: &Arc<crate::state::AppState>,
        manifests: &[PluginManifest],
        handle: &DiscoveryHandle<'_>,
    ) -> Option<Arc<dyn Device>> {
        for manifest in manifests {
            let Some(spec) = manifest.device_for(handle) else {
                continue;
            };
            if let ActivationState::Ready(ready) =
                self.activation_status(app.secret_store.as_ref(), manifest)
            {
                return self.build_device(app, manifest, spec, handle, ready);
            }
        }
        None
    }

    /// Match a discovery handle against every loaded plugin. Consulted by
    /// [`Registry::make_device`] before the native descriptors so a plugin shadows native.
    pub fn match_handle(
        &self,
        app: &Arc<crate::state::AppState>,
        handle: &DiscoveryHandle<'_>,
    ) -> Option<Arc<dyn Device>> {
        // The snapshot is a frozen `Arc`, not a lock guard, so `build_device` can
        // freely take its own snapshots with no risk of a recursive-read deadlock.
        let state = self.snapshot();
        for manifest in state.manifests.iter() {
            let Some(spec) = manifest.device_for(handle) else {
                continue;
            };
            // Only a `Ready` plugin (enabled + consent-satisfied) shadows a native
            // driver; a disabled/ungranted one falls through to the native path.
            if let ActivationState::Ready(ready) =
                self.activation_status(app.secret_store.as_ref(), manifest)
            {
                return self.build_device(app, manifest, spec, handle, ready);
            }
        }
        None
    }

    pub fn has_match(&self, handle: &DiscoveryHandle<'_>) -> bool {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| !state.disabled.contains(&m.plugin_id) && self.consent_satisfied(m))
            .any(|m| m.device_for(handle).is_some())
    }

    /// Collect every [`DeviceSpec`] declared by the named plugins' manifests
    /// (current registry snapshot). Used to build a scoped [`DiscoveryFilter`].
    pub fn device_specs_for(&self, plugin_ids: &[String]) -> Vec<DeviceSpec> {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| plugin_ids.contains(&m.plugin_id))
            .flat_map(|m| m.devices.clone())
            .collect()
    }
}

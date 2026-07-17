// SPDX-License-Identifier: GPL-3.0-or-later
//! Device plugins: add a device without recompiling the daemon.
//!
//! A plugin is a directory package whose `plugin.yaml` declares hardware,
//! identity, permissions, transports, and capabilities. Its Lua entry contains
//! callbacks that turn capability calls into transport bytes and is not
//! evaluated during manifest loading. Plugins expose
//! only *existing* capability kinds; Halo owns the capability taxonomy.
//!
//! Registration is at runtime: `load_all` reads the plugins directory into the
//! registry snapshot, and `make_device` consults `match_handle` before built-in
//! host descriptors, so a plugin shadows a built-in device for the same hardware.

mod host_scan;
pub(crate) mod integration_monitor;
pub(crate) mod integration_scan;
pub(crate) mod manifest;
#[cfg(feature = "plugin-test")]
pub mod plugin_test;
pub(crate) mod recommend;
pub mod repo;
pub(crate) mod runtime;
pub mod usecases;

pub use manifest::{parse_manifest_from_dir, DeviceSpec, EffectKind, PluginManifest, ProbeMode};
pub use runtime::device::LuaDevice;
use runtime::device::{LuaDeviceParts, LuaDeviceSpawnParts, LuaDeviceWorker};
pub use runtime::effect_worker::{LedCoord, PluginEffectHandle};
pub use runtime::widget_worker::{
    PluginWidgetHandle, WidgetImageInput, WidgetMediaInput, WidgetRenderInput, WidgetSensorInput,
};
pub use runtime::worker::run_pre_scan;

use manifest::{requirements, udev};
use runtime::{command_resolve, device, transport, worker};

/// Lua host/plugin ABI implemented by this daemon. Repository compatibility
/// and [`manifest::contract::PLUGIN_API_CONTRACT`] deliberately share this one value.
pub const PLUGIN_API: u32 = 1;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use halod_shared::types::{
    Animation, EffectParamValue, HealthState, HealthStatus, Permission, PluginInfo, PluginIssue,
    PluginIssueContext, PluginIssueKind, PluginKind, SkippedPlugin, WriteRateLimit,
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
    pub module_sources: std::collections::BTreeMap<String, String>,
    pub kind: EffectKind,
    pub catalog_id: String,
    pub descriptor: Animation,
}

#[derive(Clone)]
pub struct PluginWidgetEntry {
    pub plugin_id: String,
    pub script_source: String,
    pub module_sources: std::collections::BTreeMap<String, String>,
    pub catalog_id: String,
    pub descriptor: halod_shared::types::LcdWidgetDescriptor,
    pub plugin_dir: std::path::PathBuf,
}

#[derive(Clone)]
pub struct PluginPresetEntry {
    pub plugin_id: String,
    pub catalog_id: String,
    pub descriptor: halod_shared::types::LcdPresetDescriptor,
    pub plugin_dir: std::path::PathBuf,
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
    /// A blocking host requirement is unmet (e.g. a declared command is not on
    /// PATH). The plugin stays enabled but cannot execute until it is satisfied.
    MissingRequirements(Vec<halod_shared::types::PluginRequirementStatus>),
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
    widgets: Vec<PluginWidgetEntry>,
    presets: Vec<PluginPresetEntry>,
    /// Plugin ids the user disabled. `match_handle` skips these, so a disabled
    /// plugin no longer shadows its built-in host device.
    disabled: HashSet<String>,
    /// Integration ids disabled *as an integration* — independent of `disabled`
    /// (which governs whether the Lua may run at all). Only meaningful for
    /// `PluginKind::Integration` plugins.
    integrations_disabled: HashSet<String>,
    /// Permissions the user granted per plugin (see [`granted_for`]). Every
    /// plugin — built-in or not — must be granted its permissions through
    /// consent; nothing is auto-granted.
    granted: HashMap<String, Vec<Permission>>,
    /// Last complete authority snapshot accepted through the enable flow.
    accepted_authorities: HashMap<String, halod_shared::types::PluginAuthority>,
    /// Loader-established provenance.  This cannot be inferred from a package
    /// path for an explicit development repository because it intentionally
    /// lives outside the managed repository directory.
    provenance: HashMap<String, halod_shared::types::PluginProvenance>,
    /// Installed package hashes are update metadata, not an execution gate.
    installed_hashes: HashMap<String, String>,
    /// Non-secure config values the user set per plugin. Secure values never
    /// live here — see the secret store.
    config_values: HashMap<String, HashMap<String, String>>,
}

/// The device-plugin registry: every piece of runtime-mutable plugin state the
/// daemon owns. Held as [`crate::state::AppState::registry`] (one per process in
/// production, one per `AppState` in tests — so tests are isolated without a
/// shared-globals lock).
#[derive(Default)]
pub struct Registry {
    /// The read-mostly plugin snapshot (manifests/effects/consent/config).
    plugins: RwLock<Arc<PluginState>>,
    /// Health episodes keyed by a plugin id or by `plugin_id::device_id`.
    /// Keeping device failures separate makes recovery and one-shot
    /// notifications precise while `health_for` derives the plugin aggregate.
    health: RwLock<HashMap<String, HealthState>>,
    invalid_manifests: RwLock<Vec<(PluginManifest, PluginIssue)>>,
    skipped: RwLock<Vec<SkippedPlugin>>,
    /// Hardware recommendations, computed once at startup (and on plugin reload)
    /// from connected HID devices — not recomputed per state poll.
    recommendations: RwLock<Vec<halod_shared::types::PluginRecommendation>>,
    /// Cached requirement statuses per plugin id. Populated lazily and refreshed
    /// at event points (startup, reconcile) so probes — some of which spawn
    /// processes or open device nodes — never run on every state poll.
    requirement_cache: RwLock<HashMap<String, Vec<halod_shared::types::PluginRequirementStatus>>>,
    content_revision: std::sync::atomic::AtomicU64,
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
    pub fn content_revision(&self) -> u64 {
        self.content_revision
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Render the daemon baseline plus every loaded Linux plugin's declarative
    /// HID/USB/SMBus access rules. Disabled plugins remain included so permissions
    /// are already available when the user enables one.
    pub fn udev_rules(&self) -> String {
        udev::assemble(&self.snapshot().manifests)
    }

    pub fn udev_rules_status(&self) -> halod_shared::types::UdevRulesStatus {
        let snapshot = self.snapshot();
        let manifests = &snapshot.manifests;
        udev::status(&udev::assemble(manifests), manifests)
    }

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

/// Assemble release rules from the exact signed repository bundle embedded in
/// the daemon binary, without reading or modifying the user's plugin config.
pub fn udev_rules_from_bundle(bytes: &[u8]) -> anyhow::Result<String> {
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let root = std::env::temp_dir().join(format!("halod-udev-{}", uuid::Uuid::new_v4()));
    let cleanup = Cleanup(root.clone());
    halod_plugin_signing::extract_bundle(bytes, &root)?;
    let registry = Registry::default();
    registry.load_all_with_priority_repo(&root.join("__local__"), Some(&root), &[]);
    let rules = registry.udev_rules();
    drop(cleanup);
    Ok(rules)
}

fn consent_satisfied_in(state: &PluginState, manifest: &PluginManifest) -> bool {
    let requested = authority_for_manifest(manifest);
    if requested.permissions.is_empty() && requested.transport_scopes.is_empty() {
        return true;
    }
    state
        .accepted_authorities
        .get(&manifest.plugin_id)
        .is_some_and(|accepted| requested.is_subset_of(accepted))
}

/// Convert the statically declared transport surface into stable scope labels
/// suitable for consent comparison and UI display. Device transport names are
/// included even when their configuration is implicit (for example HID), while
/// configured endpoints contribute their own names. Future scoped transports
/// can extend this function without changing persisted authority semantics.
fn authority_for_manifest(manifest: &PluginManifest) -> halod_shared::types::PluginAuthority {
    let mut scopes: Vec<String> = manifest
        .devices
        .iter()
        .map(|device| device.transport.clone())
        .collect();
    if manifest.transports.hid.is_some() {
        scopes.push("hid".to_owned());
    }
    if manifest.transports.tcp.is_some() {
        scopes.push("tcp".to_owned());
    }
    if manifest.transports.usb.is_some() {
        scopes.push("usb".to_owned());
    }
    if let Some(command) = &manifest.transports.command {
        scopes.extend(
            command
                .commands
                .iter()
                .map(|name| format!("command:{name}")),
        );
    }
    halod_shared::types::PluginAuthority {
        permissions: manifest.permissions.clone(),
        transport_scopes: scopes,
    }
    .normalized()
}

fn effect_entries_for(manifest: &PluginManifest) -> Vec<PluginEffectEntry> {
    manifest
        .effects
        .iter()
        .map(|e| PluginEffectEntry {
            plugin_id: manifest.plugin_id.clone(),
            script_source: manifest.script_source.clone(),
            module_sources: manifest.module_sources.clone(),
            kind: e.kind,
            catalog_id: e.catalog_id(&manifest.plugin_id),
            descriptor: e.descriptor(&manifest.plugin_id),
        })
        .collect()
}

fn widget_entries_for(manifest: &PluginManifest) -> Vec<PluginWidgetEntry> {
    manifest
        .widgets
        .iter()
        .map(|widget| PluginWidgetEntry {
            plugin_id: manifest.plugin_id.clone(),
            script_source: manifest.script_source.clone(),
            module_sources: manifest.module_sources.clone(),
            catalog_id: widget.catalog_id(&manifest.plugin_id),
            descriptor: widget.descriptor(&manifest.plugin_id),
            plugin_dir: manifest.plugin_dir.clone(),
        })
        .collect()
}

fn preset_entries_for(manifest: &PluginManifest) -> Vec<PluginPresetEntry> {
    manifest
        .presets
        .iter()
        .map(|preset| {
            let descriptor = preset.descriptor(&manifest.plugin_id);
            PluginPresetEntry {
                plugin_id: manifest.plugin_id.clone(),
                catalog_id: descriptor.id.clone(),
                descriptor,
                plugin_dir: manifest.plugin_dir.clone(),
            }
        })
        .collect()
}

impl Registry {
    /// Whether the plugin owning an effect remains within its accepted authority.
    /// Installed content hashes intentionally do not gate code.
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

    fn lcd_entry_active(state: &PluginState, plugin_id: &str) -> bool {
        state
            .manifests
            .iter()
            .find(|manifest| manifest.plugin_id == plugin_id)
            .is_some_and(|manifest| {
                !state.disabled.contains(plugin_id) && consent_satisfied_in(state, manifest)
            })
    }

    pub fn widget_descriptors(&self) -> Vec<halod_shared::types::LcdWidgetDescriptor> {
        let state = self.snapshot();
        state
            .widgets
            .iter()
            .filter(|entry| Self::lcd_entry_active(&state, &entry.plugin_id))
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    pub fn widget_descriptor(
        &self,
        catalog_id: &str,
    ) -> Option<halod_shared::types::LcdWidgetDescriptor> {
        self.widget_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id == catalog_id)
    }

    pub fn preset_descriptors(&self) -> Vec<halod_shared::types::LcdPresetDescriptor> {
        let state = self.snapshot();
        state
            .presets
            .iter()
            .filter(|entry| Self::lcd_entry_active(&state, &entry.plugin_id))
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    pub fn widget_entry(&self, catalog_id: &str) -> Option<PluginWidgetEntry> {
        let state = self.snapshot();
        state
            .widgets
            .iter()
            .find(|entry| {
                entry.catalog_id == catalog_id && Self::lcd_entry_active(&state, &entry.plugin_id)
            })
            .cloned()
    }

    pub fn build_widget_handle(
        &self,
        secrets: &dyn crate::secrets::SecretStore,
        catalog_id: &str,
    ) -> Option<PluginWidgetHandle> {
        let entry = self.widget_entry(catalog_id)?;
        let state = self.snapshot();
        let widget_ids = state
            .widgets
            .iter()
            .filter(|candidate| candidate.plugin_id == entry.plugin_id)
            .filter_map(|candidate| {
                candidate
                    .catalog_id
                    .strip_prefix(&format!("{}:", entry.plugin_id))
                    .map(str::to_owned)
            })
            .collect();
        let granted = self.granted_for(&entry.plugin_id);
        let config = self.resolved_config_for(secrets, &entry.plugin_id, &granted);
        Some(PluginWidgetHandle::spawn(
            entry.script_source,
            entry.module_sources,
            widget_ids,
            granted,
            config,
        ))
    }

    pub fn preset_entry(&self, catalog_id: &str) -> Option<PluginPresetEntry> {
        let state = self.snapshot();
        state
            .presets
            .iter()
            .find(|entry| {
                entry.catalog_id == catalog_id && Self::lcd_entry_active(&state, &entry.plugin_id)
            })
            .cloned()
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
            entry.module_sources,
            effect_id,
            params.clone(),
            granted,
            config,
        ))
    }

    /// Atomically replace every persisted plugin-policy field in one snapshot.
    pub fn replace_policy(&self, policy: &crate::config::PluginPolicy) {
        self.update(|s| {
            s.disabled = s
                .manifests
                .iter()
                .filter(|manifest| !policy.enabled.contains(&manifest.plugin_id))
                .map(|manifest| manifest.plugin_id.clone())
                .collect();
            s.integrations_disabled = s
                .manifests
                .iter()
                .filter(|manifest| {
                    manifest.plugin_type == PluginKind::Integration
                        && !policy.integrations_enabled.contains(&manifest.plugin_id)
                })
                .map(|manifest| manifest.plugin_id.clone())
                .collect();
            s.granted = policy
                .accepted_authorities
                .iter()
                .map(|(id, authority)| (id.clone(), authority.permissions.clone()))
                .collect();
            s.accepted_authorities = policy.accepted_authorities.clone();
            s.installed_hashes = policy.installed_hashes.clone();
            s.config_values = policy.config.clone();
        });
        self.content_revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn is_disabled(&self, plugin_id: &str) -> bool {
        self.snapshot().disabled.contains(plugin_id)
    }

    fn is_integration_disabled(&self, plugin_id: &str) -> bool {
        self.snapshot().integrations_disabled.contains(plugin_id)
    }

    /// Current authority from inert manifest data. This must stay independent
    /// of Lua so the user can inspect exactly what will run before enabling it.
    pub(crate) fn authority_for(
        &self,
        plugin_id: &str,
    ) -> Option<halod_shared::types::PluginAuthority> {
        self.snapshot()
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .map(authority_for_manifest)
    }

    /// Whether `plugin_id`'s manifest may execute on the current host. Unknown
    /// plugins report `false`.
    pub(crate) fn supports_current_platform(&self, plugin_id: &str) -> bool {
        self.snapshot()
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .is_some_and(|m| m.supports_current_platform())
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
    fn device_health_scope(plugin_id: &str, device_id: &str) -> String {
        format!("{plugin_id}::{device_id}")
    }

    /// Update a scoped failure episode. Equivalent repeat failures update the
    /// visible detail but preserve `notification_sent`; callers notify only on
    /// the returned `true` transition.
    fn set_health(&self, scope: &str, kind: PluginIssueKind, detail: String) -> bool {
        let status = match kind {
            PluginIssueKind::ConnectFailed
            | PluginIssueKind::InitFailed
            | PluginIssueKind::RuntimeError => HealthStatus::Failed,
            PluginIssueKind::LoadFailed | PluginIssueKind::LoadWarning => HealthStatus::Degraded,
        };
        let mut health = write_recover(&self.health);
        let notification_sent = health
            .get(scope)
            .is_some_and(|state| state.notification_sent);
        health.insert(
            scope.to_owned(),
            HealthState {
                status,
                issue: Some(PluginIssue {
                    kind,
                    detail,
                    context: None,
                    timestamp_ms: crate::util::time::now_ms(),
                }),
                notification_sent: true,
            },
        );
        !notification_sent
    }

    fn clear_health(&self, scope: &str) {
        write_recover(&self.health).remove(scope);
    }

    /// Aggregate direct plugin and per-device health records. Failed wins over
    /// degraded; the newest issue at that severity is retained for display.
    fn health_for(&self, plugin_id: &str) -> HealthState {
        let prefix = format!("{plugin_id}::");
        let mut aggregate = HealthState::default();
        let severity = |status| match status {
            HealthStatus::Healthy => 0,
            HealthStatus::Degraded => 1,
            HealthStatus::Failed => 2,
        };
        for (scope, state) in read_recover(&self.health).iter() {
            if scope != plugin_id && !scope.starts_with(&prefix) {
                continue;
            }
            if severity(state.status) > severity(aggregate.status)
                || (state.status == aggregate.status
                    && state.issue.as_ref().is_some_and(|issue| {
                        aggregate
                            .issue
                            .as_ref()
                            .is_none_or(|old| issue.timestamp_ms > old.timestamp_ms)
                    }))
            {
                aggregate = state.clone();
            }
        }
        aggregate
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
        let scope = Self::device_health_scope(plugin_id, device_id);
        let notify = self.set_health(&scope, PluginIssueKind::RuntimeError, detail.clone());
        // Close the check/write race: if disable landed between the first
        // policy read and `set_issue`, remove the stale write before anything
        // can observe or broadcast it. If disable starts after this check, its
        // own cleanup runs later and wins instead.
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_health(&scope);
            return;
        }
        if !notify {
            return; // already reported this episode
        }
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_health(&scope);
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
        self.clear_health(&Self::device_health_scope(plugin_id, device_id));
    }

    /// Persist a physical plugin device's initialize failure on the owning
    /// plugin card. The generic registration path sends the DeviceInitFailed
    /// toast; this record keeps the failure visible after that toast expires.
    pub(crate) fn report_init_error(&self, plugin_id: &str, device_id: &str, detail: String) {
        let scope = Self::device_health_scope(plugin_id, device_id);
        self.set_health(&scope, PluginIssueKind::InitFailed, detail);
    }

    /// A later successful initialization ends the per-device failure episode.
    pub(crate) fn clear_init_error(&self, plugin_id: &str, device_id: &str) {
        let scope = Self::device_health_scope(plugin_id, device_id);
        let should_clear = read_recover(&self.health)
            .get(&scope)
            .and_then(|state| state.issue.as_ref())
            .is_some_and(|issue| issue.kind == PluginIssueKind::InitFailed);
        if should_clear {
            self.clear_health(&scope);
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
        let notify = self.set_health(plugin_id, PluginIssueKind::ConnectFailed, detail.clone());
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_health(plugin_id);
            return;
        }
        if !notify {
            return; // already reported this episode
        }
        if self.is_disabled(plugin_id) || self.is_integration_disabled(plugin_id) {
            self.clear_health(plugin_id);
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
        self.clear_health(plugin_id);
    }

    /// Clear all operational error state when a plugin or integration is
    /// deliberately stopped. Removing episode-dedup keys lets a later re-enable
    /// report a genuinely fresh failure, while clearing the persisted issue
    /// immediately removes its card/sidebar warning.
    pub(crate) fn clear_operational_errors(&self, plugin_id: &str, device_ids: &[String]) {
        let prefix = format!("{plugin_id}::");
        let devices: HashSet<_> = device_ids.iter().collect();
        write_recover(&self.health).retain(|scope, _| {
            scope != plugin_id
                && !(scope.starts_with(&prefix)
                    && devices
                        .iter()
                        .any(|device| scope == &Self::device_health_scope(plugin_id, device)))
        });
    }

    /// End every connection/device failure episode from an integration's old
    /// runtime. A freshly opened integration may expose a different set of
    /// device ids, so waiting for successful callbacks from the old ids would
    /// leave their aggregated sidebar issue stuck forever. Load diagnostics are
    /// retained because reconnecting cannot resolve package-content problems.
    pub(crate) fn clear_integration_operational_errors(&self, plugin_id: &str) {
        let prefix = format!("{plugin_id}::");
        write_recover(&self.health).retain(|scope, state| {
            let belongs_to_integration = scope == plugin_id || scope.starts_with(&prefix);
            let operational = state.issue.as_ref().is_some_and(|issue| {
                matches!(
                    issue.kind,
                    PluginIssueKind::ConnectFailed
                        | PluginIssueKind::InitFailed
                        | PluginIssueKind::RuntimeError
                )
            });
            !belongs_to_integration || !operational
        });
    }

    /// Record a non-fatal load warning (bad logo, id collision) as a persisted
    /// plugin issue, surfaced on the plugin page / sidebar without a toast.
    fn set_load_warning(&self, plugin_id: &str, reason: String) {
        self.set_health(plugin_id, PluginIssueKind::LoadWarning, reason);
    }

    /// Drop any stale load-warning issues before a reload re-derives them, so a
    /// warning the user has since fixed doesn't linger. Runtime/connect issues
    /// (a different `kind`) are untouched.
    fn clear_load_warnings(&self) {
        write_recover(&self.health).retain(|_, state| {
            state
                .issue
                .as_ref()
                .is_none_or(|issue| issue.kind != PluginIssueKind::LoadWarning)
        });
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

    /// True when the plugin's complete requested authority remains within the
    /// accepted snapshot. Being consent-satisfied is
    /// necessary but not sufficient to activate — see [`Registry::activation_status`],
    /// which also accounts for the disabled set.
    fn consent_satisfied(&self, manifest: &PluginManifest) -> bool {
        consent_satisfied_in(&self.snapshot(), manifest)
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
        if !manifest.supports_current_platform() {
            // Catalog-visible but inert: it must neither request consent nor
            // shadow an otherwise applicable built-in host device.
            ActivationState::Disabled
        } else if self.is_disabled(&manifest.plugin_id) {
            ActivationState::Disabled
        } else {
            // Requirements are evaluated before consent: a plugin that cannot run
            // shows its missing requirement instead of prompting for consent it
            // could not use. Requirement probing is inert host access, never Lua.
            let missing = requirements::blocking_missing(&self.requirement_statuses(manifest));
            if !missing.is_empty() {
                ActivationState::MissingRequirements(missing)
            } else if !self.consent_satisfied(manifest) {
                ActivationState::AwaitingConsent
            } else {
                let granted = self.granted_for(&manifest.plugin_id);
                let config = self.resolved_config_for(secrets, &manifest.plugin_id, &granted);
                ActivationState::Ready(ReadyPlugin { granted, config })
            }
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
                    && m.supports_current_platform()
                    && !state.integrations_disabled.contains(&m.plugin_id)
                    && !state.disabled.contains(&m.plugin_id)
                    && consent_satisfied_in(&state, m)
                    && self.blocking_requirements_met(m)
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

impl Registry {
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
            info.active = false;
            info.activation_blocker = None;
            info.health = HealthState {
                status: HealthStatus::Degraded,
                issue: Some(issue.clone()),
                notification_sent: false,
            };
            infos.push(info);
        }
        infos
    }

    /// Blocking host requirements currently unmet for `plugin_id` (empty when
    /// satisfied or the plugin is unknown). Freshly evaluated — the daemon-
    /// authoritative check the enable flow force-refreshes so a stale GUI cannot
    /// bypass the gate.
    pub fn missing_blocking_requirements(
        &self,
        plugin_id: &str,
    ) -> Vec<halod_shared::types::PluginRequirementStatus> {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .find(|m| m.plugin_id == plugin_id)
            .map(|m| requirements::blocking_missing(&requirements::evaluate(m)))
            .unwrap_or_default()
    }

    /// Cached requirement statuses for `m`, evaluating and caching on first use.
    /// The gate and UI read this so probes don't run on every state poll; event
    /// points (startup, reconcile) call [`refresh_requirements`] to recompute.
    fn requirement_statuses(
        &self,
        m: &PluginManifest,
    ) -> Vec<halod_shared::types::PluginRequirementStatus> {
        if let Some(cached) = read_recover(&self.requirement_cache).get(&m.plugin_id) {
            return cached.clone();
        }
        let statuses = requirements::evaluate(m);
        write_recover(&self.requirement_cache).insert(m.plugin_id.clone(), statuses.clone());
        statuses
    }

    /// Recompute and cache every loaded plugin's requirement statuses. Called at
    /// startup and after reconcile so satisfied↔missing transitions are picked up
    /// without any continuous polling.
    pub fn refresh_requirements(&self) {
        let state = self.snapshot();
        let fresh: HashMap<String, Vec<halod_shared::types::PluginRequirementStatus>> = state
            .manifests
            .iter()
            .map(|m| (m.plugin_id.clone(), requirements::evaluate(m)))
            .collect();
        *write_recover(&self.requirement_cache) = fresh;
    }

    /// Whether every blocking host requirement for `m` is currently satisfied.
    /// The single predicate every activation path shares, so device plugins and
    /// integrations gate on requirements identically. Reads the cache.
    fn blocking_requirements_met(&self, m: &PluginManifest) -> bool {
        requirements::blocking_missing(&self.requirement_statuses(m)).is_empty()
    }

    /// Recompute host/plugin recommendations and cache them. Called at startup
    /// and on plugin reload — never per state poll. Independent of the activation
    /// matcher, which excludes disabled plugins.
    pub fn refresh_recommendations(
        &self,
        hid: &[halod_shared::debug_info::HidEntryDebugInfo],
        usb: &[recommend::UsbEntry],
        gpu_smbus: &[crate::drivers::transports::smbus::BusInfo],
    ) {
        let state = self.snapshot();
        let recs = recommend::recommendations(
            &state.manifests,
            &|id| !state.disabled.contains(id),
            hid,
            usb,
            gpu_smbus,
            &|executable| command_resolve::resolve(executable).is_some(),
        );
        *write_recover(&self.recommendations) = recs;
    }

    /// The cached host recommendations (see [`refresh_recommendations`]).
    pub fn recommendations(&self) -> Vec<halod_shared::types::PluginRecommendation> {
        read_recover(&self.recommendations).clone()
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
        let health = self.health_for(&m.plugin_id);
        // Requirement status drives the effective activation and the "Enabled,
        // but inactive" blocker. A disabled/unsupported plugin has no blocker —
        // it is simply off.
        let requirements = self.requirement_statuses(m);
        let blocking = requirements::blocking_missing(&requirements);
        let intent = m.supports_current_platform() && !self.is_disabled(&m.plugin_id);
        let integration_enabled = !self.is_integration_disabled(&m.plugin_id);
        let active = intent
            && consented
            && blocking.is_empty()
            && (m.plugin_type != PluginKind::Integration || integration_enabled);
        let activation_blocker = (intent && !blocking.is_empty()).then_some(
            halod_shared::types::PluginActivationBlocker::MissingRequirements {
                requirements: blocking,
            },
        );
        PluginInfo {
            id: m.plugin_id.clone(),
            name: m.display_name(),
            path: m.source_path.display().to_string(),
            plugin_type: m.plugin_type,
            capabilities: m.capability_labels(),
            platforms: m.platforms.clone(),
            platform_supported: m.supports_current_platform(),
            effect_names: m.effects.iter().map(|e| e.name.clone()).collect(),
            enabled: m.supports_current_platform() && !self.is_disabled(&m.plugin_id) && consented,
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
            provenance: state
                .provenance
                .get(&m.plugin_id)
                .copied()
                .unwrap_or(halod_shared::types::PluginProvenance::LocalUnsigned),
            declared_permissions: m.permissions.clone(),
            authority: authority_for_manifest(m),
            accepted_authority: state.accepted_authorities.get(&m.plugin_id).cloned(),
            config_fields: m.config_fields().iter().map(Into::into).collect(),
            config_values: config_values_for(state, m),
            secret_set,
            integration_enabled,
            consented,
            active,
            requirements,
            activation_blocker,
            health,
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

    fn read_declared_file(
        plugin_dir: &std::path::Path,
        name: &str,
        max_bytes: u64,
    ) -> anyhow::Result<Vec<u8>> {
        manifest::validate_asset_filename(name)?;
        let path = plugin_dir.join("assets").join(name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            anyhow::bail!("asset '{name}' must be a regular file");
        }
        if meta.len() > max_bytes {
            anyhow::bail!("asset '{name}' exceeds the plugin asset size limit");
        }
        std::fs::read(&path).map_err(Into::into)
    }

    /// Validate and rasterize a widget's mandatory SVG before it reaches the GUI.
    pub fn read_widget_icon(&self, catalog_id: &str) -> anyhow::Result<Vec<u8>> {
        use image::ImageEncoder as _;
        let image = self.read_widget_icon_rgba_at(catalog_id, 64)?;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            &image.rgba,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(png)
    }

    /// Rasterize a widget SVG at the resolution needed by the LCD renderer.
    /// The GUI thumbnail intentionally remains 64 px, but render assets must
    /// never be enlarged from that thumbnail.
    pub fn read_widget_icon_rgba_at(
        &self,
        catalog_id: &str,
        target_edge: u32,
    ) -> anyhow::Result<WidgetImageInput> {
        use resvg::{tiny_skia, usvg};

        let entry = self
            .widget_entry(catalog_id)
            .ok_or_else(|| anyhow::anyhow!("unknown LCD widget '{catalog_id}'"))?;
        let data = Self::read_declared_file(
            &entry.plugin_dir,
            &entry.descriptor.icon,
            halod_shared::types::MAX_PLUGIN_ASSET_BYTES,
        )?;
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
            .map_err(|error| anyhow::anyhow!("invalid widget SVG: {error}"))?;
        let size = tree.size().to_int_size();
        let source_edge = size.width().max(size.height()) as f32;
        anyhow::ensure!(source_edge > 0.0, "widget SVG has no drawable size");
        let target_edge = target_edge.clamp(1, 1024);
        let mut pixmap = tiny_skia::Pixmap::new(target_edge, target_edge)
            .ok_or_else(|| anyhow::anyhow!("allocating widget SVG raster"))?;
        let scale = target_edge as f32 / source_edge;
        let tx = (target_edge as f32 - size.width() as f32 * scale) / 2.0;
        let ty = (target_edge as f32 - size.height() as f32 * scale) / 2.0;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, tx, ty),
            &mut pixmap.as_mut(),
        );
        let mut rgba = Vec::with_capacity(target_edge as usize * target_edge as usize * 4);
        for pixel in pixmap.pixels() {
            let pixel = pixel.demultiply();
            rgba.extend_from_slice(&[pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]);
        }
        Ok(WidgetImageInput {
            width: target_edge,
            height: target_edge,
            rgba,
        })
    }

    pub fn read_lcd_preset(
        &self,
        catalog_id: &str,
    ) -> anyhow::Result<halod_shared::lcd_custom::CustomTemplateDef> {
        let entry = self
            .preset_entry(catalog_id)
            .ok_or_else(|| anyhow::anyhow!("unknown LCD preset '{catalog_id}'"))?;
        let data = Self::read_declared_file(&entry.plugin_dir, &entry.descriptor.file, 512 * 1024)?;
        let def = serde_json::from_slice(&data)?;
        halod_shared::lcd_custom::validate_widgets(&def).map_err(anyhow::Error::msg)?;
        Ok(def)
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
    invalid: Vec<(PluginManifest, String, Option<PluginIssueContext>)>,
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
        scan.invalid.push((manifest, reason, None));
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

fn record_invalid_plugin_dir(
    dir: &Path,
    package_id: String,
    package_version: String,
    reason: &str,
    context: Option<&PluginIssueContext>,
    scan: &mut LoadScan,
) {
    match manifest::build_manifest_from_dir(dir) {
        Ok(manifest) => scan
            .invalid
            .push((manifest, reason.to_owned(), context.cloned())),
        Err(error) => scan.invalid.push((
            PluginManifest {
                plugin_id: package_id.clone(),
                source_path: dir.join("main.lua"),
                script_source: String::new(),
                module_sources: Default::default(),
                plugin_dir: dir.to_path_buf(),
                devices: vec![],
                identity: manifest::Identity {
                    name: Some(package_id.clone()),
                    id: Some(package_id),
                    version: Some(package_version),
                    ..Default::default()
                },
                logo: None,
                effect_thumbnails: vec![],
                plugin_type: PluginKind::Device,
                dynamic_children: false,
                effects: vec![],
                widgets: vec![],
                presets: vec![],
                transports: Default::default(),
                requirements: vec![],
                permissions: vec![],
                platforms: vec![],
                capabilities: vec![],
                config: None,
            },
            format!("{reason}; package manifest also failed to parse: {error:#}"),
            context.cloned(),
        )),
    }
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
    match repo::read_repository_manifest(repo_dir) {
        Ok(repository) => {
            for package in repository.packages {
                try_load_plugin_dir(&repo_dir.join(package.path), scan);
            }
            return;
        }
        Err(error) if repo_dir.join("repository.yaml").is_file() => {
            log::warn!(
                "Ignoring invalid plugin repository {}: {error:#}",
                repo_dir.display()
            );
            if let Ok(repository) = repo::read_repository_index(repo_dir) {
                let reason = format!("repository package failed integrity validation: {error:#}");
                let context = error
                    .downcast_ref::<halod_plugin_signing::PackageHashMismatch>()
                    .map(|mismatch| PluginIssueContext::RepositoryHashMismatch {
                        package: mismatch.package.clone(),
                        expected: mismatch.expected.clone(),
                        actual: mismatch.actual.clone(),
                    });
                for package in repository.packages {
                    record_invalid_plugin_dir(
                        &repo_dir.join(package.path),
                        package.id,
                        package.version,
                        &reason,
                        context.as_ref(),
                        scan,
                    );
                }
            }
            return;
        }
        Err(_) => {}
    }
    try_load_plugin_dir(repo_dir, scan);
    scan_plugin_subdirs(repo_dir, scan);
    scan_plugin_subdirs(&repo_dir.join("plugins"), scan);
}

/// Scan a repo without integrity verification — for the local development repo
/// (`--dev-plugin-repo`), whose working tree is edited in place and whose hashes
/// intentionally won't match the generated `repository.yaml`.
fn scan_repo_trusted(repo_dir: &Path, scan: &mut LoadScan) {
    if let Ok(repository) = repo::read_repository_index(repo_dir) {
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
    repos.iter().map(repo::active_revision_dir).collect()
}

impl Registry {
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
    #[cfg(test)]
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
            scan_repo_trusted(repo_dir, &mut scan);
        } else {
            for repo_dir in repo_dirs.iter().filter(|d| is_official(d)) {
                scan_repo(repo_dir, &mut scan);
            }
            scan_plugin_subdirs(dir, &mut scan);
            for repo_dir in repo_dirs.iter().filter(|d| !is_official(d)) {
                scan_repo(repo_dir, &mut scan);
            }
        }
        let effects: Vec<PluginEffectEntry> =
            scan.manifests.iter().flat_map(effect_entries_for).collect();
        let widgets: Vec<PluginWidgetEntry> =
            scan.manifests.iter().flat_map(widget_entries_for).collect();
        let presets: Vec<PluginPresetEntry> =
            scan.manifests.iter().flat_map(preset_entries_for).collect();
        let provenance = scan
            .manifests
            .iter()
            .map(|manifest| {
                let provenance =
                    if priority_repo.is_some_and(|root| manifest.plugin_dir.starts_with(root)) {
                        halod_shared::types::PluginProvenance::LocalDevelopment
                    } else {
                        match plugin_source_for(&manifest.plugin_dir) {
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
                        }
                    };
                (manifest.plugin_id.clone(), provenance)
            })
            .collect();
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
            .map(|(m, reason, context)| {
                (
                    m,
                    PluginIssue {
                        kind: PluginIssueKind::LoadFailed,
                        detail: reason,
                        context,
                        timestamp_ms: crate::util::time::now_ms(),
                    },
                )
            })
            .collect();
        *write_recover(&self.invalid_manifests) = invalid;
        *write_recover(&self.skipped) = scan.skipped;
        self.update(|s| {
            s.manifests = scan.manifests;
            s.effects = effects;
            s.widgets = widgets;
            s.presets = presets;
            s.provenance = provenance;
        });
        self.content_revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
    // SMN describes the single physical CPU package, not one bus endpoint.
    // Its native predecessor used a fixed stable identity; retain an explicit
    // plugin identity verbatim instead of adding CPUID-derived churn.
    #[cfg(target_os = "windows")]
    if matches!(handle, DiscoveryHandle::AmdSmn { .. }) && manifest.identity.id.is_some() {
        return manifest.id_prefix().to_owned();
    }
    let suffix = transport::descriptor_for(&spec.transport)
        .and_then(|d| d.id_suffix)
        .map(|f| f(handle))
        .unwrap_or_else(|| "0".to_owned());
    format!("{}-{}", manifest.id_prefix(), suffix)
}

impl Registry {
    /// Build the plugin device matching `handle`.
    pub fn make_device(
        &self,
        app: &Arc<crate::state::AppState>,
        handle: DiscoveryHandle<'_>,
    ) -> Option<Arc<dyn Device>> {
        let identity = crate::registry::identity::identity_from_handle(&handle);
        let device = self.match_handle(app, &handle)?;
        let origin = device.conflict_origin();
        Some(Arc::new(crate::registry::identity::IdentifiedDevice::new(
            device, identity, origin,
        )))
    }

    /// Build a device from a matched manifest, the spec that matched, and the
    /// handle. Device-only plugins need no runtime/transport; capability plugins
    /// open their transport and spawn a worker. Returns `None` if the transport
    /// can't be opened.
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
            return Some(Arc::new(LuaDevice::new(LuaDeviceParts {
                id,
                manifest,
                spec: Some(spec),
                notify,
                runtime: None,
                worker: LuaDeviceWorker::None,
            })));
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
            key: None,
            name: None,
            extra: match handle {
                #[cfg(target_os = "windows")]
                DiscoveryHandle::AmdSmn { family, model } => HashMap::from([
                    ("family".to_owned(), u64::from(*family)),
                    ("model".to_owned(), u64::from(*model)),
                ]),
                #[cfg(target_os = "windows")]
                DiscoveryHandle::Lpcio {
                    slot,
                    chip_id,
                    revision,
                    hwm_base,
                } => HashMap::from([
                    ("slot".to_owned(), u64::from(*slot)),
                    ("chip_id".to_owned(), u64::from(*chip_id)),
                    ("revision".to_owned(), u64::from(*revision)),
                    ("hwm_base".to_owned(), u64::from(*hwm_base)),
                ]),
                _ => HashMap::new(),
            },
        };

        // `new_cyclic` so the device can hand its children a `FanHub` back-reference
        // for the chain machinery (e.g. an NZXT Kraken/Control Hub accessory fan).
        let device = Arc::new_cyclic(|weak| {
            let mut dev = LuaDevice::new(LuaDeviceParts {
                id,
                manifest,
                spec: Some(spec),
                notify,
                runtime: Some(runtime_state),
                worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
                    dev_match,
                    transport,
                    handle: runtime,
                    granted,
                    config,
                })),
            });
            dev.set_self_ref(weak.clone());
            dev
        });
        if manifest
            .capabilities
            .iter()
            .any(|capability| capability == "chain")
        {
            let adapter: Arc<dyn crate::drivers::chain::ChainAdapter> = device.clone();
            let host = crate::drivers::chain::ChainHost::new(adapter);
            device.install_chain_host(host);
        }
        Some(device as Arc<dyn Device>)
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
            // Only a `Ready` plugin (enabled + consent-satisfied + requirements
            // met) shadows a native driver; a disabled/ungranted/blocked one
            // falls through to the native path.
            match self.activation_status(app.secret_store.as_ref(), manifest) {
                ActivationState::Ready(ready) => {
                    return self.build_device(app, manifest, spec, handle, ready);
                }
                ActivationState::MissingRequirements(missing) => {
                    log::debug!(
                        "plugin '{}' matches this device but is inactive: {} unmet blocking requirement(s)",
                        manifest.plugin_id,
                        missing.len()
                    );
                }
                ActivationState::Disabled | ActivationState::AwaitingConsent => {}
            }
        }
        None
    }

    pub fn has_match(&self, handle: &DiscoveryHandle<'_>) -> bool {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| {
                m.supports_current_platform()
                    && !state.disabled.contains(&m.plugin_id)
                    && self.consent_satisfied(m)
            })
            .any(|m| m.device_for(handle).is_some())
    }

    /// Collect every [`DeviceSpec`] declared by the named plugins' manifests
    /// (current registry snapshot). Used to build a scoped [`DiscoveryFilter`].
    pub fn device_specs_for(&self, plugin_ids: &[String]) -> Vec<DeviceSpec> {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| m.supports_current_platform() && plugin_ids.contains(&m.plugin_id))
            .flat_map(|m| m.devices.clone())
            .collect()
    }

    /// Device declarations belonging to plugins that may execute now. Host
    /// scanners use this to enumerate generic identities without embedding
    /// plugin-specific executable names or chip allowlists in Rust.
    pub(super) fn active_device_specs(&self) -> Vec<DeviceSpec> {
        let state = self.snapshot();
        state
            .manifests
            .iter()
            .filter(|m| {
                m.supports_current_platform()
                    && !state.disabled.contains(&m.plugin_id)
                    && consent_satisfied_in(&state, m)
            })
            .flat_map(|m| m.devices.clone())
            .collect()
    }
}

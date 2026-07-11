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
mod integration_leaf;
pub(crate) mod integration_scan;
mod manifest;
mod sandbox;
mod transport;
mod transport_api;
mod worker;

pub use device::LuaDevice;
pub use effect_worker::{LedCoord, PluginEffectHandle};
pub use manifest::{parse_manifest, EffectKind, PluginManifest, ProbeMode};
pub use scan::plugin_smbus_scan_entries;
pub use worker::run_pre_scan;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

use halod_shared::types::{Animation, EffectParamValue, Permission, PluginInfo, PluginKind};

use crate::drivers::Device;
use crate::registry::discovery::DiscoveryHandle;

mod scan;

#[cfg(test)]
mod corsair_test;
#[cfg(test)]
mod ene_test;
#[cfg(test)]
mod lcd_test;
#[cfg(test)]
mod openrgb_test;

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
    /// Permissions the user granted per plugin. Built-ins are additionally
    /// auto-granted their own declared permissions in [`granted_for`].
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

/// The current registry snapshot. The lock is held only for the `Arc` clone,
/// never across the caller's use of the data — so re-entrant reads can't deadlock.
fn snapshot() -> Arc<PluginState> {
    STATE.read().expect("plugin state poisoned").clone()
}

/// Swap in a new snapshot by applying `f` to a clone of the current one. Held
/// under the write lock so concurrent mutators can't lose each other's edits;
/// `f` only mutates fields (never re-locks), so it cannot deadlock.
fn update(f: impl FnOnce(&mut PluginState)) {
    let mut guard = STATE.write().expect("plugin state poisoned");
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

/// Permissions actually granted to `plugin_id`'s Lua sandbox: the user-set
/// grants, plus — for a built-in — its own declared permissions. Built-ins
/// ship inside the trusted daemon binary, so (as with `consent_satisfied`)
/// no separate consent step applies to them; without this, a built-in
/// declaring e.g. `os` would still run with the sandbox's `os.clock()`
/// stripped, since nothing else populates the granted map on its behalf.
pub(crate) fn granted_for(plugin_id: &str) -> Vec<Permission> {
    let mut granted = snapshot()
        .granted
        .get(plugin_id)
        .cloned()
        .unwrap_or_default();
    if is_builtin(plugin_id) {
        // Looked up from the built-in sources directly (not the snapshot's
        // manifests, which may not have been populated yet in this process —
        // e.g. in a test building a `LuaDevice` straight from a parsed manifest
        // without going through `load_all`) so a built-in's own permissions are
        // reliably granted regardless of registry state.
        if let Some(m) = builtin_manifests()
            .iter()
            .find(|m| m.plugin_id == plugin_id)
        {
            for p in &m.permissions {
                if !granted.contains(p) {
                    granted.push(*p);
                }
            }
        }
    }
    granted
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
    *SECRET_STORE.write().expect("secret store poisoned") = Some(store);
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
    let Some(store) = SECRET_STORE.read().ok().and_then(|g| g.clone()) else {
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

/// True when the plugin may activate. Built-ins ship in the trusted binary; a
/// plugin declaring no permissions runs freely (it can only talk to its matched
/// device — the base trust every plugin has). A plugin that *declares*
/// permissions must have every one granted **and** the grant pinned to the
/// exact script content the user consented to: editing the script after a grant
/// revokes consent until it is granted again (trust-on-first-use).
fn consent_satisfied(manifest: &PluginManifest) -> bool {
    if is_builtin(&manifest.plugin_id) || manifest.permissions.is_empty() {
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
    NOTIFIED
        .write()
        .expect("plugin notified set poisoned")
        .get_or_insert_with(HashSet::new)
        .insert(plugin_id.to_owned());
}

/// Pure filter behind [`take_newly_ungranted_plugins`]: which manifests are
/// ungranted and not yet in `notified` (inserting each as it's returned).
fn ungranted_in(manifests: &[PluginManifest], notified: &mut HashSet<String>) -> Vec<String> {
    manifests
        .iter()
        .filter(|m| !consent_satisfied(m) && notified.insert(m.plugin_id.clone()))
        .map(|m| m.display_name().to_owned())
        .collect()
}

/// Display names of plugins that need a permission grant and haven't been
/// announced yet (auto-discovered, not manually imported — those are marked
/// via [`suppress_permission_notice`] before this is ever called). Marks
/// every returned plugin as notified so a later rescan won't repeat it.
pub fn take_newly_ungranted_plugins() -> Vec<String> {
    let state = snapshot();
    let mut guard = NOTIFIED.write().expect("plugin notified set poisoned");
    let notified = guard.get_or_insert_with(HashSet::new);
    ungranted_in(&state.manifests, notified)
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
            PluginInfo {
                id: m.plugin_id.clone(),
                name: m.display_name().to_owned(),
                path: m.source_path.display().to_string(),
                plugin_type: m.plugin_type,
                capabilities: m.capability_labels(),
                effect_names: m.effects.iter().map(|e| e.name.clone()).collect(),
                enabled: !is_disabled(&m.plugin_id),
                author: m.author().to_owned(),
                version: m.version().to_owned(),
                description: m.description().to_owned(),
                targets: m.target_labels(),
                builtin: is_builtin(&m.plugin_id),
                declared_permissions: m.permissions.clone(),
                granted_permissions: granted_for(&m.plugin_id),
                config_fields: m.config_fields().iter().map(Into::into).collect(),
                config_values: config_values_for(&state, m),
                secret_set,
                integration_enabled: !is_integration_disabled(&m.plugin_id),
                consented: consent_satisfied(m),
                content_changed: !is_builtin(&m.plugin_id)
                    && acknowledged_hash_for(&m.plugin_id).is_some_and(|h| h != m.content_hash()),
            }
        })
        .collect()
}

/// A plugin compiled into the daemon binary (built-in), keyed by its stem.
/// Built-ins have no on-disk script in the plugins directory, so they can be
/// disabled but never deleted through the GUI.
pub fn is_builtin(plugin_id: &str) -> bool {
    BUILTIN_PLUGINS
        .iter()
        .any(|(name, _)| Path::new(name).file_stem().and_then(|s| s.to_str()) == Some(plugin_id))
}

/// Plugins shipped inside the daemon binary, loaded before directory plugins.
/// Most replace what used to be native Rust drivers (e.g. ENE SMBus RGB);
/// `halo_effects.lua` is the reference effect-plugin implementation
/// instead of a separate example file, so it's always available to inspect
/// and to exercise the RGB effect plugin API without dropping anything into
/// the plugins directory. The user can disable any of these like any other
/// plugin.
const BUILTIN_PLUGINS: &[(&str, &str)] = &[
    ("ene_smbus.lua", include_str!("builtins/ene_smbus.lua")),
    (
        "corsair_dram.lua",
        include_str!("builtins/corsair_dram.lua"),
    ),
    ("nzxt_kraken.lua", include_str!("builtins/nzxt_kraken.lua")),
    (
        "nzxt_kraken_x3.lua",
        include_str!("builtins/nzxt_kraken_x3.lua"),
    ),
    (
        "nzxt_control_hub.lua",
        include_str!("builtins/nzxt_control_hub.lua"),
    ),
    (
        "halo_effects.lua",
        include_str!("builtins/halo_effects.lua"),
    ),
    ("openrgb.lua", include_str!("builtins/openrgb.lua")),
];

/// Parsed once and cached. `granted_for` needs a built-in's declared
/// permissions even before `load_all` has populated the snapshot (see its
/// built-in branch), and it is a discovery hot path — so the built-in sources
/// are parsed a single time here rather than on every call.
fn builtin_manifests() -> &'static [PluginManifest] {
    static CACHE: std::sync::OnceLock<Vec<PluginManifest>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        BUILTIN_PLUGINS
            .iter()
            .filter_map(|(name, src)| match parse_manifest(src, Path::new(name)) {
                Ok(m) => Some(m),
                Err(e) => {
                    log::error!("built-in plugin '{name}' failed to parse: {e:#}");
                    None
                }
            })
            .collect()
    })
}

/// (Re)load the built-in plugins plus every `*.lua` in `dir` into the registry,
/// replacing prior contents. A malformed plugin is logged and skipped — it never
/// aborts loading or the daemon. Missing directory is normal (no plugins installed).
pub fn load_all(dir: &Path) {
    let mut manifests = builtin_manifests().to_vec();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                    continue;
                }
                match load_one(&path) {
                    Ok(m) if is_builtin(&m.plugin_id) => {
                        // A disk plugin whose id collides with a built-in would be
                        // treated as built-in by id (consent-exempt, auto-granted the
                        // built-in's declared permissions). Refuse it so a file drop
                        // can never impersonate a trusted compiled-in plugin.
                        log::warn!(
                            "Ignoring plugin {}: id '{}' collides with a built-in",
                            path.display(),
                            m.plugin_id
                        );
                    }
                    Ok(m) => {
                        log::info!(
                            "Loaded device plugin '{}' from {}",
                            m.plugin_id,
                            path.display()
                        );
                        manifests.push(m);
                    }
                    Err(e) => log::warn!("Skipping plugin {}: {e:#}", path.display()),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!("No plugins directory at {}", dir.display());
        }
        Err(e) => log::warn!("Cannot read plugins directory {}: {e}", dir.display()),
    }
    let effects: Vec<PluginEffectEntry> = manifests.iter().flat_map(effect_entries_for).collect();
    update(|s| {
        s.manifests = manifests;
        s.effects = effects;
    });
}

fn load_one(path: &Path) -> anyhow::Result<PluginManifest> {
    let source = std::fs::read_to_string(path)?;
    parse_manifest(&source, path)
}

/// Stable device id from a matched manifest + handle (suffix per transport).
fn device_id(
    manifest: &PluginManifest,
    spec: &manifest::MatchSpec,
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
    spec: &manifest::MatchSpec,
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
        .map(|d| (d.open)(manifest, handle, &config))
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
            _ => None,
        },
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
        let host = crate::drivers::chain::ChainHost::new(
            adapter,
            crate::drivers::CHAIN_LINK_KIND_NZXT_ARGB,
        );
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
        .find_map(|m| m.match_spec_for(handle).map(|spec| (m, spec)))
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
        .find_map(|m| m.match_spec_for(handle).map(|spec| (m, spec)))?;
    build_device(manifest, spec, handle)
}

pub fn has_match(handle: &DiscoveryHandle<'_>) -> bool {
    let state = snapshot();
    state
        .manifests
        .iter()
        .filter(|m| !state.disabled.contains(&m.plugin_id) && consent_satisfied(m))
        .any(|m| m.match_spec_for(handle).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretStore as _;
    use halod_shared::types::DeviceType;
    use std::path::Path;

    use super::TEST_GLOBALS_LOCK as GLOBALS_LOCK;

    fn manifest() -> PluginManifest {
        let src = r#"
            return {
              match = { transport = "hid", vid = 0x1234, pid = 0x5678 },
              identity = { vendor = "Acme", model = "K1", name = "Acme K1" },
            }
        "#;
        parse_manifest(src, Path::new("acme_k1.lua")).unwrap()
    }

    fn hid<'a>(vid: u16, pid: u16, serial: Option<&'a str>, idx: usize) -> DiscoveryHandle<'a> {
        DiscoveryHandle::Hid {
            vid,
            pid,
            path: "p",
            serial,
            idx,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        }
    }

    /// Replace the registry snapshot's manifests (and their derived effect
    /// entries) for a test — the snapshot equivalent of the old direct
    /// `*PLUGIN_REGISTRY.write()`. Callers hold `GLOBALS_LOCK`.
    fn set_registry(manifests: Vec<PluginManifest>) {
        let effects: Vec<PluginEffectEntry> =
            manifests.iter().flat_map(effect_entries_for).collect();
        update(|s| {
            s.manifests = manifests;
            s.effects = effects;
        });
    }

    /// Acknowledge every manifest's current content, as user consent would, so
    /// `consent_satisfied` treats them as consented. Callers hold `GLOBALS_LOCK`.
    fn acknowledge(manifests: &[PluginManifest]) {
        let map = manifests
            .iter()
            .map(|m| (m.plugin_id.clone(), m.content_hash()))
            .collect();
        set_acknowledged(&map);
    }

    #[test]
    fn matching_handle_builds_device_with_identity() {
        let manifests = vec![manifest()];
        let dev = match_in(&manifests, &hid(0x1234, 0x5678, Some("SER"), 0)).expect("matches");
        assert_eq!(dev.vendor(), "Acme");
        assert_eq!(dev.name(), "Acme K1");
        assert_eq!(dev.id(), "acme_k1-SER");
    }

    #[test]
    fn device_id_falls_back_to_index_without_serial() {
        let manifests = vec![manifest()];
        let dev = match_in(&manifests, &hid(0x1234, 0x5678, None, 3)).expect("matches");
        assert_eq!(dev.id(), "acme_k1-3");
    }

    #[test]
    fn granted_permission_is_pinned_to_script_content() {
        // A permissioned plugin activates only while its granted content pin
        // matches the current script; editing the script revokes consent.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"
            return {
              match = { transport = "hid", vid = 0xCCCC, pid = 0xDDDD },
              identity = { vendor = "Acme", model = "K2" },
              permissions = { "network" },
            }
        "#;
        let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
        let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
        set_disabled(&[]);
        set_granted(&HashMap::from([(
            "needs_network".to_string(),
            vec![Permission::Network],
        )]));

        // Granted but not pinned to content → inert.
        set_acknowledged(&HashMap::new());
        assert!(
            match_in(&manifests, &handle).is_none(),
            "a grant with no content pin must not activate"
        );

        // Pinned to the current content → active.
        acknowledge(&manifests);
        assert!(
            match_in(&manifests, &handle).is_some(),
            "grant pinned to the current script activates"
        );

        // Content changed (stale pin) → inert again.
        set_acknowledged(&HashMap::from([(
            "needs_network".to_string(),
            "deadbeef".to_string(),
        )]));
        assert!(
            match_in(&manifests, &handle).is_none(),
            "a since-modified script must revert to needing consent"
        );

        set_granted(&HashMap::new());
        set_acknowledged(&HashMap::new());
    }

    #[test]
    fn non_matching_handle_returns_none() {
        let manifests = vec![manifest()];
        assert!(match_in(&manifests, &hid(0x9999, 0x0000, None, 0)).is_none());
    }

    #[test]
    fn disabled_plugin_does_not_match() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"
            return {
              match = { transport = "hid", vid = 0xAAAA, pid = 0xBBBB },
              identity = { vendor = "Acme", model = "K1" },
            }
        "#;
        let manifests = vec![parse_manifest(src, Path::new("disabled_only_plugin.lua")).unwrap()];
        let handle = hid(0xAAAA, 0xBBBB, Some("S"), 0);
        set_disabled(&["disabled_only_plugin".to_string()]);
        assert!(
            match_in(&manifests, &handle).is_none(),
            "disabled plugin must not shadow native"
        );
        set_disabled(&[]);
        assert!(match_in(&manifests, &handle).is_some());
    }

    #[test]
    fn plugin_with_ungranted_permission_does_not_match() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"
            return {
              match = { transport = "hid", vid = 0xCCCC, pid = 0xDDDD },
              identity = { vendor = "Acme", model = "K2" },
              permissions = { "network" },
            }
        "#;
        let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
        let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
        set_disabled(&[]);
        acknowledge(&manifests);
        set_granted(&HashMap::new());
        assert!(
            match_in(&manifests, &handle).is_none(),
            "declared-but-ungranted permission must keep the plugin inert"
        );

        let mut granted = HashMap::new();
        granted.insert("needs_network".to_string(), vec![Permission::Network]);
        set_granted(&granted);
        assert!(
            match_in(&manifests, &handle).is_some(),
            "fully granted plugin activates"
        );
        set_granted(&HashMap::new());
        set_acknowledged(&HashMap::new());
    }

    #[test]
    fn consent_satisfied_true_when_no_permissions_declared() {
        // A plugin that declares no permissions runs freely — it can only talk
        // to its matched device, the base trust every plugin has.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "x", model = "y" },
            }
        "#;
        let m = parse_manifest(src, Path::new("no_perms.lua")).unwrap();
        set_acknowledged(&HashMap::new());
        assert!(consent_satisfied(&m));
    }

    #[test]
    fn load_all_never_runs_a_dropped_in_scripts_side_effects() {
        // A malicious file dropped into the plugins dir tries to write a
        // sentinel at top level. `load_all` evaluates its manifest, but the
        // sandbox strips `io`/`os`, so the write never happens and the plugin
        // is skipped — dropping a file can't run code before consent.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("pwned.txt");
        let evil = format!(
            r#"local f = io.open([[{}]], "w"); f:write("x"); f:close()
               return {{ match = {{ transport = "hid", vid = 1, pid = 2 }},
                         identity = {{ vendor = "x", model = "y" }} }}"#,
            sentinel.display()
        );
        std::fs::write(dir.path().join("evil.lua"), evil).unwrap();

        load_all(dir.path());

        assert!(
            !sentinel.exists(),
            "a dropped-in script's filesystem write must never execute at load time"
        );
        assert!(
            !snapshot().manifests.iter().any(|m| m.plugin_id == "evil"),
            "a script that errors under the sandbox must be skipped, not registered"
        );
        load_all(Path::new("/nonexistent"));
    }

    #[test]
    fn ungranted_in_reports_once_then_stays_silent() {
        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "x", model = "y", name = "Needs Net" },
              permissions = { "network" },
            }
        "#;
        let m = parse_manifest(src, Path::new("needs_net2.lua")).unwrap();
        let manifests = vec![m];
        let mut notified = HashSet::new();

        let first = ungranted_in(&manifests, &mut notified);
        assert_eq!(first, vec!["Needs Net".to_string()]);

        // Same manifest, same notified set: already announced, not repeated.
        let second = ungranted_in(&manifests, &mut notified);
        assert!(
            second.is_empty(),
            "must not repeat an already-notified plugin"
        );
    }

    #[test]
    fn ungranted_in_skips_satisfied_manifests() {
        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "x", model = "y" },
            }
        "#;
        let m = parse_manifest(src, Path::new("no_perms2.lua")).unwrap();
        let mut notified = HashSet::new();
        assert!(ungranted_in(&[m], &mut notified).is_empty());
    }

    #[test]
    fn ene_smbus_is_builtin_others_are_not() {
        assert!(is_builtin("ene_smbus"));
        assert!(!is_builtin("wled_udp"));
        assert!(!is_builtin("ene_smbus.lua")); // stem only, not the file name
    }

    #[test]
    fn disk_plugin_cannot_impersonate_a_builtin_id() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A file dropped as `openrgb.lua` claims a built-in id; if loaded it
        // would be consent-exempt and auto-granted the built-in's permissions.
        let evil = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "EVIL", model = "y" },
              permissions = { "network", "os" },
            }
        "#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("openrgb.lua"), evil).unwrap();
        load_all(dir.path());

        let state = snapshot();
        let openrgb: Vec<_> = state
            .manifests
            .iter()
            .filter(|m| m.plugin_id == "openrgb")
            .collect();
        assert_eq!(openrgb.len(), 1, "disk shadow must not join the built-in");
        assert_ne!(
            openrgb[0].identity.vendor, "EVIL",
            "the surviving 'openrgb' must be the compiled-in built-in, not the disk file"
        );
        drop(state);
        load_all(Path::new("/nonexistent"));
    }

    #[test]
    fn shipped_example_plugin_parses() {
        // Guards the documented example against drift with the manifest schema.
        let src = include_str!("../../../../../plugins/examples/example_device.lua");
        let m = parse_manifest(src, Path::new("example_device.lua")).unwrap();
        assert_eq!(m.identity.vendor, "Example");
        assert_eq!(m.capability_labels(), vec!["RGB", "Fan", "Sensor"]);
        assert!(m.needs_worker());
        assert_eq!(m.poll.as_ref().map(|p| p.interval_ms), Some(500));
    }

    #[test]
    fn shipped_nzxt_kraken_plugin_parses_per_pid_identity() {
        // Guards the Z/Elite family plugin's per-PID name/device_type fix
        // (regression: every matched PID used to show as "Kraken Z" and
        // categorize as "unknown" instead of AIO).
        let src = include_str!("builtins/nzxt_kraken.lua");
        let m = parse_manifest(src, Path::new("nzxt_kraken.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 5);
        for spec in &m.match_specs {
            assert_eq!(spec.device_type, Some(DeviceType::AIO));
            assert!(spec.name.is_some(), "every PID needs its own display name");
        }
        let elite_v2 = m
            .match_specs
            .iter()
            .find(|s| s.pid == Some(0x3012))
            .expect("0x3012 (Elite V2) must be matched");
        assert_eq!(elite_v2.name.as_deref(), Some("Kraken Elite V2"));
        assert!(m.fan.is_some());
        assert!(m.lcd.is_some());
    }

    #[test]
    fn shipped_nzxt_kraken_x3_plugin_parses() {
        // X53/X63/X73: distinct wire family — ring+logo RGB only, no
        // software pump/fan control, no LCD.
        let src = include_str!("builtins/nzxt_kraken_x3.lua");
        let m = parse_manifest(src, Path::new("nzxt_kraken_x3.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 2);
        for spec in &m.match_specs {
            assert_eq!(spec.device_type, Some(DeviceType::AIO));
            assert_eq!(spec.name.as_deref(), Some("Kraken X53/X63/X73"));
        }
        assert!(m.match_specs.iter().any(|s| s.pid == Some(0x2007)));
        assert!(m.match_specs.iter().any(|s| s.pid == Some(0x2014)));
        assert!(m.fan.is_none(), "X3 has no software pump/fan control");
        assert!(m.lcd.is_none(), "X3 has no LCD");
        let zones = &m.rgb.as_ref().unwrap().zones;
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].leds.len(), 8);
        assert_eq!(zones[1].leds.len(), 1);
    }

    #[test]
    fn shipped_nzxt_control_hub_plugin_parses() {
        let src = include_str!("builtins/nzxt_control_hub.lua");
        let m = parse_manifest(src, Path::new("nzxt_control_hub.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 1);
        assert_eq!(m.match_specs[0].pid, Some(0x2022));
        assert_eq!(m.match_specs[0].device_type, Some(DeviceType::Hub));
        assert!(m.rgb.is_none(), "hub has no LEDs of its own");
        assert!(m.fan.is_none(), "hub has no fan of its own");
        assert!(m.sensor.is_none());
        let chain = m.chain.as_ref().unwrap();
        assert_eq!(chain.channels.len(), 5);
        assert!(chain.accessories.iter().all(|a| a.fan));
    }

    #[test]
    fn shipped_example_effects_plugin_parses() {
        // Guards the documented effects example against drift with the schema.
        let src = include_str!("builtins/halo_effects.lua");
        let m = parse_manifest(src, Path::new("halo_effects.lua")).unwrap();
        assert!(
            m.match_specs.is_empty(),
            "effect-only plugin needs no match"
        );
        assert_eq!(m.plugin_type, PluginKind::Effect);
        assert!(!m.needs_worker());
        assert!(
            m.capability_labels().is_empty(),
            "effects aren't a capability"
        );
        assert_eq!(m.effects.len(), 10);
        let entries = effect_entries_for(&m);
        assert_eq!(entries[0].catalog_id, "halo_effects:plasma");
        assert_eq!(entries[0].kind, EffectKind::Pixmap);
        assert!(entries
            .iter()
            .any(|e| e.catalog_id == "halo_effects:comet" && e.kind == EffectKind::Direct));
    }

    #[test]
    fn shipped_openrgb_plugin_parses() {
        // Guards the built-in OpenRGB integration against drift with the schema.
        let src = include_str!("builtins/openrgb.lua");
        let m = parse_manifest(src, Path::new("openrgb.lua")).unwrap();
        assert!(
            m.match_specs.is_empty(),
            "integration plugin needs no match"
        );
        assert_eq!(m.plugin_type, PluginKind::Integration);
        assert!(m.needs_worker());
        assert_eq!(m.permissions, vec![Permission::Network, Permission::Os]);
        let tcp = m.transports.tcp.as_ref().expect("declares a tcp transport");
        assert_eq!(tcp.host_key, "host");
        assert_eq!(tcp.port_key, "port");
        let fields = m.config_fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].key, "host");
        assert_eq!(fields[0].default, "127.0.0.1");
        assert_eq!(fields[1].key, "port");
        assert_eq!(fields[1].default, "6742");
    }

    #[test]
    fn builtin_plugins_are_permission_satisfied_without_a_grant() {
        // openrgb.lua declares `network`; being built-in (shipped with the
        // trusted daemon binary) must be enough — no manual consent step.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_granted(&HashMap::new());
        let src = include_str!("builtins/openrgb.lua");
        let m = parse_manifest(src, Path::new("openrgb.lua")).unwrap();
        assert!(consent_satisfied(&m));
    }

    #[test]
    fn granted_for_auto_grants_a_builtins_own_declared_permissions() {
        // Regression: `permissions_satisfied` (discovery gating) bypassing
        // consent for built-ins isn't enough on its own — `granted_for` feeds
        // the *sandbox's* actual permission list (e.g. whether `os.clock()`
        // is reinjected), and previously had no built-in bypass at all, so a
        // built-in's own declared permissions were never actually granted at
        // the Lua level despite `permissions_satisfied` letting it through.
        // Uses the real `openrgb.lua` (declares `network` + `os`) since the
        // lookup is against the built-in sources, not `PLUGIN_REGISTRY` —
        // this must hold even before `load_all` has ever populated it.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_granted(&HashMap::new());
        set_registry(Vec::new());

        let granted = granted_for("openrgb");
        assert!(granted.contains(&Permission::Network));
        assert!(granted.contains(&Permission::Os));

        // A non-builtin with the same declared permissions must NOT be
        // auto-granted — only an explicit user grant satisfies it.
        let src = r#"return {
            identity = { vendor = "x", model = "y" },
            type = "integration",
            permissions = { "network", "os" },
        }"#;
        set_registry(vec![parse_manifest(
            src,
            Path::new("some_other_plugin.lua"),
        )
        .unwrap()]);
        assert!(granted_for("some_other_plugin").is_empty());

        set_registry(Vec::new());
    }

    #[test]
    fn effect_entries_for_namespaces_ids_and_carries_kind() {
        let src = r#"return {
            identity = { vendor = "x", model = "Effects" },
            effects = {
              { kind = "pixmap", id = "plasma", name = "Plasma" },
              { kind = "direct", id = "comet", name = "Comet" },
            },
        }"#;
        let m = parse_manifest(src, Path::new("fx.lua")).unwrap();
        let entries = effect_entries_for(&m);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].catalog_id, "fx:plasma");
        assert_eq!(entries[0].kind, EffectKind::Pixmap);
        assert_eq!(entries[0].descriptor.id, "fx:plasma");
        assert_eq!(entries[1].catalog_id, "fx:comet");
        assert_eq!(entries[1].kind, EffectKind::Direct);
    }

    #[test]
    fn effect_entries_for_empty_for_a_device_only_plugin() {
        assert!(effect_entries_for(&manifest()).is_empty());
    }

    #[test]
    fn shipped_permission_demo_plugin_parses_and_is_inert_until_granted() {
        // Guards the permission-demo example against drift, and demonstrates
        // the gate: declared-but-ungranted is unsatisfied; fully granted is
        // satisfied. (Not routed through `match_in` — this plugin declares a
        // `sensor` capability, so `needs_worker()` is true and full device
        // construction would need a real HID transport, not just a runtime.)
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = include_str!("../../../../../plugins/examples/permission_demo.lua");
        let m = parse_manifest(src, Path::new("permission_demo.lua")).unwrap();
        assert_eq!(m.permissions, vec![Permission::Os]);
        assert!(m.needs_worker());

        // Acknowledged, so this isolates the permission gate (not the consent gate).
        acknowledge(std::slice::from_ref(&m));
        set_granted(&HashMap::new());
        assert!(
            !consent_satisfied(&m),
            "ungranted os permission keeps it inert"
        );

        let mut granted = HashMap::new();
        granted.insert("permission_demo".to_string(), vec![Permission::Os]);
        set_granted(&granted);
        assert!(consent_satisfied(&m));
        set_granted(&HashMap::new());
        set_acknowledged(&HashMap::new());
    }

    #[test]
    fn capability_labels_reflect_manifest_sections() {
        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "V", model = "M" },
              rgb = { zones = {} },
              sensor = {},
            }
        "#;
        let m = parse_manifest(src, Path::new("caps.lua")).unwrap();
        assert_eq!(m.capability_labels(), vec!["RGB", "Sensor"]);
    }

    /// An in-memory `SecretStore` for tests, so `list()` tests don't need a
    /// real keyring or the encrypted-file backend.
    #[derive(Default)]
    struct FakeSecretStore {
        values: std::sync::Mutex<HashMap<(String, String), String>>,
    }

    impl crate::secrets::SecretStore for FakeSecretStore {
        fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> anyhow::Result<()> {
            self.values
                .lock()
                .unwrap()
                .insert((plugin_id.to_owned(), key.to_owned()), plaintext.to_owned());
            Ok(())
        }
        fn get(&self, plugin_id: &str, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(&(plugin_id.to_owned(), key.to_owned()))
                .cloned())
        }
        fn delete(&self, plugin_id: &str, key: &str) -> anyhow::Result<()> {
            self.values
                .lock()
                .unwrap()
                .remove(&(plugin_id.to_owned(), key.to_owned()));
            Ok(())
        }
        fn backend_name(&self) -> &'static str {
            "fake"
        }
    }

    #[test]
    fn config_for_defaults_unset_fields_and_overrides_set_ones() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            config = { fields = {
              { key = "host", label = "Host", default = "127.0.0.1" },
              { key = "port", label = "Port", default = "6742" },
              { key = "token", label = "Token", secure = true, default = "unused" },
            } },
        }"#;
        set_registry(vec![parse_manifest(src, Path::new("cfgfor.lua")).unwrap()]);
        let mut stored = HashMap::new();
        stored.insert(
            "cfgfor".to_string(),
            HashMap::from([("port".to_string(), "9999".to_string())]),
        );
        set_config_values(&stored);

        let resolved = config_for("cfgfor");
        assert_eq!(resolved.get("host"), Some(&"127.0.0.1".to_string()));
        assert_eq!(resolved.get("port"), Some(&"9999".to_string()));
        assert!(
            !resolved.contains_key("token"),
            "secure fields must never appear in the non-secure config map"
        );

        set_config_values(&HashMap::new());
        set_registry(Vec::new());
    }

    #[test]
    fn config_for_unknown_plugin_is_empty() {
        assert!(config_for("does-not-exist").is_empty());
    }

    #[test]
    fn integration_manifests_filters_by_type_disabled_and_permissions() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let integ_src = r#"return {
            identity = { vendor = "x", model = "y" },
            type = "integration",
        }"#;
        let device_src = r#"return {
            identity = { vendor = "x", model = "y" },
            match = { transport = "hid", vid = 1, pid = 2 },
        }"#;
        let needs_perm_src = r#"return {
            identity = { vendor = "x", model = "y" },
            type = "integration",
            permissions = { "network" },
        }"#;
        set_registry(vec![
            parse_manifest(integ_src, Path::new("integ_ok.lua")).unwrap(),
            parse_manifest(device_src, Path::new("device_only.lua")).unwrap(),
            parse_manifest(needs_perm_src, Path::new("integ_needs_perm.lua")).unwrap(),
        ]);
        set_disabled(&[]);
        set_granted(&HashMap::new());

        let ids: Vec<String> = integration_manifests()
            .into_iter()
            .map(|m| m.plugin_id)
            .collect();
        assert_eq!(ids, vec!["integ_ok"]);

        set_disabled(&["integ_ok".to_string()]);
        assert!(integration_manifests().is_empty());
        set_disabled(&[]);

        // `integrations_disabled` is a second, independent gate: it must
        // exclude the integration even though the plugin itself is enabled.
        set_integrations_disabled(&["integ_ok".to_string()]);
        assert!(integration_manifests().is_empty());
        assert!(integration_manifest("integ_ok").is_none());
        set_integrations_disabled(&[]);

        assert_eq!(
            integration_manifest("integ_ok").map(|m| m.plugin_id),
            Some("integ_ok".to_string())
        );

        set_registry(Vec::new());
    }

    #[test]
    fn secure_config_keys_for_returns_declared_secure_keys() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            config = { fields = {
              { key = "host", label = "Host" },
              { key = "token", label = "Token", secure = true },
            } },
        }"#;
        set_registry(vec![
            parse_manifest(src, Path::new("securekeys.lua")).unwrap()
        ]);

        assert_eq!(secure_config_keys_for("securekeys"), vec!["token"]);
        assert!(secure_config_keys_for("does-not-exist").is_empty());

        set_registry(Vec::new());
    }

    #[test]
    fn list_reports_config_fields_values_and_secret_set() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            config = { fields = {
              { key = "host", label = "Host", default = "127.0.0.1" },
              { key = "token", label = "Token", secure = true },
            } },
        }"#;
        set_registry(vec![parse_manifest(src, Path::new("listcfg.lua")).unwrap()]);

        let secrets = FakeSecretStore::default();
        secrets.set("listcfg", "token", "s3cr3t").unwrap();

        let infos = list(&secrets);
        let info = infos.iter().find(|p| p.id == "listcfg").expect("present");
        assert_eq!(info.config_fields.len(), 2);
        assert_eq!(
            info.config_values.get("host"),
            Some(&"127.0.0.1".to_string())
        );
        assert!(
            !info.config_values.contains_key("token"),
            "secret value must never appear in config_values"
        );
        assert_eq!(info.secret_set.get("token"), Some(&true));

        set_registry(Vec::new());
    }
}

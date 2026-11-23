// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// Bounds a hostile plugin from bloating the socket and the GUI's texture memory.
pub const MAX_PLUGIN_ASSET_BYTES: u64 = 256 * 1024;

/// Painted into a small square tile, so anything larger is wasted.
pub const MAX_PLUGIN_LOGO_DIM: u32 = 512;

/// Long:short side ratio; an extreme banner would letterbox to an unreadable sliver.
pub const MAX_PLUGIN_LOGO_ASPECT: u32 = 2;

pub fn validate_logo_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("logo has a zero dimension".to_owned());
    }
    if width > MAX_PLUGIN_LOGO_DIM || height > MAX_PLUGIN_LOGO_DIM {
        return Err(format!(
            "logo {width}x{height} exceeds the {MAX_PLUGIN_LOGO_DIM}px maximum"
        ));
    }
    let (long, short) = if width >= height {
        (width, height)
    } else {
        (height, width)
    };
    if long > short * MAX_PLUGIN_LOGO_ASPECT {
        return Err(format!(
            "logo aspect {width}x{height} exceeds the {MAX_PLUGIN_LOGO_ASPECT}:1 maximum"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    #[serde(default)]
    pub plugins: Vec<PluginInfo>,
    #[serde(default)]
    pub repos: Vec<PluginRepoInfo>,
    /// The best entry per id and remote repo: the newest compatible version,
    /// else the newest one, flagged incompatible.
    #[serde(default)]
    pub available: Vec<AvailablePlugin>,
    /// Manifests too malformed to load, shown as a "skipped" notice.
    #[serde(default)]
    pub skipped: Vec<SkippedPlugin>,
    /// Computed once at startup.
    #[serde(default)]
    pub recommendations: Vec<PluginRecommendation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPlugin {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PluginRepoLocation {
    Url(String),
    Local(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoTrust {
    Official,
    Signed,
    #[default]
    Unsigned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRepoInfo {
    pub slug: String,
    pub location: PluginRepoLocation,
    /// The GUI hides the remove control for the seeded official repo.
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub trust: RepoTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// RFC 3339.
    #[serde(default)]
    pub last_sync: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

/// Sent as an IPC error `code` so the GUI can localize repository failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogErrorCode {
    Malformed,
    Unsigned,
    UntrustedKey,
    SignatureMismatch,
    Stale,
    HashMismatch,
    ApiIncompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailablePlugin {
    pub repo: String,
    pub id: crate::ids::PluginId,
    pub name: String,
    pub version: String,
    pub api: String,
    pub compatible: bool,
    pub uses: PackageUses,
    #[serde(default)]
    pub matches: PackageMatches,
}

/// The boundary between "trusted to talk to its matched device" and "trusted to reach outside it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Hid,
    /// Only allowlisted bulk/interrupt endpoints or bounded control-transfer policy.
    Usb,
    Hwmon,
    Lpcio,
    /// Read-only.
    AmdSmn,
    Command,
    Serial,
    Network,
    /// Non-secure config values need no permission.
    SecureStorage,
    /// A raw bus grants access to every device on it, hence a separate grant.
    Smbus,
    AudioRouting,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginProvenance {
    Official,
    Signed,
    #[default]
    Unsigned,
    Local,
}

/// What a plugin's manifest declares it uses; an update that widens it needs the user's permission again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageUses {
    pub permissions: Vec<Permission>,
    pub commands: Vec<String>,
    /// Exact keys, except for the host-managed `host.sensors.*` scope.
    pub data_reads: Vec<String>,
}

impl PackageUses {
    pub fn widens(&self, previous: &PackageUses) -> bool {
        fn beyond<T: PartialEq>(next: &[T], previous: &[T]) -> bool {
            next.iter().any(|item| !previous.contains(item))
        }
        beyond(&self.permissions, &previous.permissions)
            || beyond(&self.commands, &previous.commands)
            || beyond(&self.data_reads, &previous.data_reads)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HidIdentity {
    pub vid: u16,
    pub pids: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_page: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbIdentity {
    pub vid: u16,
    pub pids: Vec<u16>,
    pub interface: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PciIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_vendor: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_device: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageMatches {
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hid_any: bool,
    pub hid: Vec<HidIdentity>,
    pub usb: Vec<UsbIdentity>,
    pub smbus_gpu: Vec<PciIdentity>,
    pub commands: Vec<String>,
}

impl PackageMatches {
    pub fn supports_platform(&self, os: &str) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|platform| platform == os)
    }

    pub fn device_keys(&self) -> Vec<DeviceKey> {
        let hid = self.hid.iter().flat_map(|identity| {
            identity.pids.iter().map(|&pid| DeviceKey::Hid {
                vid: identity.vid,
                pid,
                usage_page: identity.usage_page,
                usage: identity.usage,
                interface: identity.interface,
            })
        });
        let usb = self.usb.iter().flat_map(|identity| {
            identity.pids.iter().map(|&pid| DeviceKey::Usb {
                vid: identity.vid,
                pid,
                interface: identity.interface,
            })
        });
        let pci = self.smbus_gpu.iter().cloned().map(DeviceKey::SmbusGpu);
        hid.chain(usb).chain(pci).collect()
    }

    fn widens_access(&self, previous: &PackageMatches) -> bool {
        (self.hid_any && !previous.hid_any)
            || self
                .commands
                .iter()
                .any(|command| !previous.commands.contains(command))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceKey {
    Hid {
        vid: u16,
        pid: u16,
        usage_page: Option<u16>,
        usage: Option<u16>,
        interface: Option<i32>,
    },
    Usb {
        vid: u16,
        pid: u16,
        interface: u8,
    },
    SmbusGpu(PciIdentity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateScope {
    Unchanged,
    NewDevices(usize),
    MorePermissions,
}

pub fn update_scope(
    previous: Option<(&PackageUses, &PackageMatches)>,
    next: (&PackageUses, &PackageMatches),
) -> UpdateScope {
    let Some((previous_uses, previous_matches)) = previous else {
        return UpdateScope::MorePermissions;
    };
    let (next_uses, next_matches) = next;
    if next_uses.widens(previous_uses) || next_matches.widens_access(previous_matches) {
        return UpdateScope::MorePermissions;
    }
    let known = previous_matches.device_keys();
    let mut added: Vec<DeviceKey> = Vec::new();
    for key in next_matches.device_keys() {
        if !known.contains(&key) && !added.contains(&key) {
            added.push(key);
        }
    }
    match added.len() {
        0 => UpdateScope::Unchanged,
        count => UpdateScope::NewDevices(count),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDataRecordStatus {
    pub key: String,
    pub status: String,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    #[default]
    Device,
    /// Never opens a transport.
    Effect,
    /// Instantiated from its own config values rather than a discovery handle.
    Integration,
    Lcd,
}

/// Display-only; the GUI fetches the bytes via `GetPluginAsset`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEffectAsset {
    pub id: String,
    pub thumbnail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginIssueKind {
    ConnectFailed,
    InitFailed,
    /// Background path only.
    RuntimeError,
    /// e.g. bad logo, id collision.
    LoadWarning,
    LoadFailed,
}

/// Unstructured failures continue to use `PluginIssue::detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginIssueContext {
    ApiIncompatible { required: String, supported: String },
}

/// Persisted so it survives a transient toast the user may have missed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIssue {
    pub kind: PluginIssueKind,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PluginIssueContext>,
    pub timestamp_ms: u64,
}

/// Identical failures update an existing episode without notifying again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    #[default]
    Healthy,
    Degraded,
    Failed,
}

/// A success resets it to `Healthy` and clears `notification_sent`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthState {
    #[serde(default)]
    pub status: HealthStatus,
    #[serde(default)]
    pub issue: Option<PluginIssue>,
    #[serde(default)]
    pub notification_sent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementImpact {
    #[default]
    Block,
    Degrade,
}

/// The UI owns the localized remediation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementFailureReason {
    NotInstalled,
    NotLoaded,
    NotFound,
    PermissionDenied,
    Unavailable,
    Incompatible,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequirement {
    /// Must resolve on the daemon's `PATH`.
    Command { executable: String },
    /// Explicitly required by an integration.
    KernelModule { name: String },
    /// Inferred for Windows transports backed by the bundled modules.
    PawnIo,
    /// Inferred from an SMBus device match.
    LinuxI2c,
    /// `access` is `read` for sensor discovery or `pwm` for writable fan control.
    LinuxHwmon { access: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRequirementStatus {
    pub requirement: PluginRequirement,
    #[serde(default)]
    pub impact: RequirementImpact,
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RequirementFailureReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

/// Derived from enable intent, setup completeness, and the live root device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationLifecycleState {
    #[default]
    Disabled,
    Unconfigured,
    Configured,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationSetupMode {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationSetupPhase {
    #[default]
    Init,
    Discovering,
    Pairing,
    Done,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationAuthKind {
    #[default]
    None,
    Button,
    Oauth2Pkce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSetupCandidate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub values: HashMap<String, String>,
}

/// Lua supplies only candidates and hook results; it never controls screens or transitions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationSetupStatus {
    #[serde(default)]
    pub phase: IntegrationSetupPhase,
    #[serde(default)]
    pub modes: Vec<IntegrationSetupMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_mode: Option<IntegrationSetupMode>,
    #[serde(default)]
    pub auth: IntegrationAuthKind,
    #[serde(default)]
    pub candidates: Vec<IntegrationSetupCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_candidate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub instructions: Vec<String>,
    /// GUI i18n key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginActivationBlocker {
    MissingRequirements {
        requirements: Vec<PluginRequirementStatus>,
    },
}

/// Never includes device paths or serial numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum PluginRecommendationMatch {
    /// Currently reserved for the stock Halo Effects library.
    Always,
    Command {
        executable: String,
    },
    Hid {
        vid: u16,
        pid: u16,
    },
    Usb {
        vid: u16,
        pid: u16,
    },
    /// Chipset SMBus controllers alone would be false positives on nearly every desktop.
    SmbusGpu {
        pci_vendor: u16,
        pci_device: u16,
        pci_sub_vendor: u16,
        pci_sub_device: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecommendation {
    pub plugin_id: crate::ids::PluginId,
    pub plugin_name: String,
    pub hardware: PluginRecommendationMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_from: Option<String>,
    /// Always true until the access probe lands.
    #[serde(default = "default_true")]
    pub accessible: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdevRulesStatus {
    /// False on hosts where udev is not used.
    pub supported: bool,
    /// Highest-precedence installed `60-halod.rules` is byte-equal to the daemon's output.
    pub current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
    pub generated_rule_count: usize,
    /// Empty when the rules are current or only the daemon baseline drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins_requiring_update: Vec<crate::ids::PluginId>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub missing_rules_by_plugin: std::collections::BTreeMap<crate::ids::PluginId, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributing_plugin_ids: Vec<crate::ids::PluginId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: crate::ids::PluginId,
    pub name: String,
    #[serde(default)]
    pub translations: BTreeMap<String, BTreeMap<String, String>>,
    pub path: String,
    #[serde(default)]
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub experimental: bool,
    /// e.g. "RGB", "Fan".
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Empty means all platforms.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// False when the catalog is visible on this host but must stay inert.
    #[serde(default = "default_true")]
    pub platform_supported: bool,
    #[serde(default)]
    pub effect_names: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub targets: Vec<String>,
    /// SPDX identifier or free-text name.
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub devices: Vec<PluginDeviceInfo>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub effect_thumbnails: Vec<PluginEffectAsset>,
    /// Slug of the repository the plugin was loaded from.
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub provenance: PluginProvenance,
    #[serde(default)]
    pub uses: PackageUses,
    #[serde(default)]
    pub matches: PackageMatches,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub data_records: Vec<PluginDataRecordStatus>,
    #[serde(default)]
    pub config_fields: Vec<PluginConfigField>,
    /// Secure fields never appear here; see `secret_set`.
    #[serde(default)]
    pub config_values: HashMap<String, String>,
    /// The secret's plaintext never crosses the IPC boundary.
    #[serde(default)]
    pub secret_set: HashMap<String, bool>,
    /// Independent of `enabled`, which only governs whether its Lua may run; `true` for non-integrations.
    #[serde(default = "default_true")]
    pub integration_enabled: bool,
    #[serde(default)]
    pub integration_configured: bool,
    #[serde(default)]
    pub integration_requires_setup: bool,
    #[serde(default)]
    pub integration_state: IntegrationLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_setup: Option<IntegrationSetupStatus>,
    /// An enabled plugin is inactive while a blocking host requirement is missing.
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub requirements: Vec<PluginRequirementStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_blocker: Option<PluginActivationBlocker>,
    /// Device-specific records remain scoped in the daemon registry.
    #[serde(default)]
    pub health: HealthState,
}

impl PluginInfo {
    fn translated_text<'a>(&'a self, locale: &str, key: &str, fallback: &'a str) -> &'a str {
        let locale = locale.to_ascii_lowercase();
        let base = locale.split(['-', '_']).next().unwrap_or(&locale);
        self.translations
            .get(&locale)
            .and_then(|catalog| catalog.get(key))
            .or_else(|| {
                self.translations
                    .get(base)
                    .and_then(|catalog| catalog.get(key))
            })
            .or_else(|| {
                self.translations
                    .get("en")
                    .and_then(|catalog| catalog.get(key))
            })
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn localized_name(&self, locale: &str) -> &str {
        self.translated_text(locale, "plugin.name", &self.name)
    }

    pub fn localized_description(&self, locale: &str) -> &str {
        self.translated_text(locale, "plugin.description", &self.description)
    }
}

pub(super) fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDeviceInfo {
    pub vendor: String,
    pub model: String,
    /// The device's `name` override, or `model`.
    pub name: String,
    #[serde(default)]
    pub device_type: Option<DeviceType>,
}

/// `value` is a replug-stable `/dev/serial/by-id` path when available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialPortInfo {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginConfigFieldKind {
    #[default]
    Text,
    Number,
    Boolean,
    Enum,
    Host,
    Port,
    /// Offered as a dropdown of host ports, with a free-text fallback.
    SerialPort,
    Url,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigVisibility {
    pub field: String,
    pub equals: String,
}

/// No `default`: the GUI only sees resolved values via `PluginInfo::config_values`/`secret_set`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfigField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub kind: PluginConfigFieldKind,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<PluginConfigVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const PERMISSIONS: [Permission; 4] = [
        Permission::Hid,
        Permission::Usb,
        Permission::Network,
        Permission::Command,
    ];

    fn uses(mask: u8, commands: &[&str]) -> PackageUses {
        PackageUses {
            permissions: PERMISSIONS
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, permission)| *permission)
                .collect(),
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
            data_reads: Vec::new(),
        }
    }

    proptest! {
        #[test]
        fn widens_exactly_when_the_next_set_is_not_a_subset(previous in 0u8..16, next in 0u8..16) {
            prop_assert_eq!(
                uses(next, &[]).widens(&uses(previous, &[])),
                next & !previous != 0
            );
        }
    }

    #[test]
    fn a_new_program_or_data_read_widens() {
        assert!(uses(0, &["nvidia-smi"]).widens(&uses(0, &[])));
        assert!(!uses(0, &[]).widens(&uses(0, &["nvidia-smi"])));
        let mut reads = uses(0, &[]);
        reads.data_reads.push("host.sensors.*".into());
        assert!(reads.widens(&uses(0, &[])));
    }

    fn hid(vid: u16, pids: &[u16]) -> HidIdentity {
        HidIdentity {
            vid,
            pids: pids.to_vec(),
            usage_page: None,
            usage: None,
            interface: None,
        }
    }

    fn matching(identities: Vec<HidIdentity>) -> PackageMatches {
        PackageMatches {
            hid: identities,
            ..PackageMatches::default()
        }
    }

    fn scope(previous: &PackageMatches, next: &PackageMatches) -> UpdateScope {
        let base = uses(1, &[]);
        update_scope(Some((&base, previous)), (&base, next))
    }

    #[test]
    fn a_fresh_install_needs_permissions() {
        let base = uses(1, &[]);
        let matches = PackageMatches::default();
        assert_eq!(
            update_scope(None, (&base, &matches)),
            UpdateScope::MorePermissions
        );
    }

    #[test]
    fn new_pids_and_identities_count_as_new_devices() {
        let base = matching(vec![hid(1, &[10])]);
        assert_eq!(
            scope(&base, &matching(vec![hid(1, &[10, 11])])),
            UpdateScope::NewDevices(1)
        );
        assert_eq!(
            scope(&base, &matching(vec![hid(1, &[10]), hid(2, &[20, 21])])),
            UpdateScope::NewDevices(2)
        );
        let mut usb = base.clone();
        usb.usb.push(UsbIdentity {
            vid: 3,
            pids: vec![30],
            interface: 0,
        });
        usb.smbus_gpu.push(PciIdentity::default());
        assert_eq!(scope(&base, &usb), UpdateScope::NewDevices(2));
    }

    #[test]
    fn regrouping_or_dropping_devices_is_unchanged() {
        let base = matching(vec![hid(1, &[10, 11])]);
        assert_eq!(
            scope(&base, &matching(vec![hid(1, &[11]), hid(1, &[10])])),
            UpdateScope::Unchanged
        );
        assert_eq!(
            scope(&base, &matching(vec![hid(1, &[10])])),
            UpdateScope::Unchanged
        );
        assert_eq!(
            scope(&base, &matching(vec![hid(1, &[10, 10, 11])])),
            UpdateScope::Unchanged
        );
    }

    #[test]
    fn new_access_outranks_new_devices() {
        let base = matching(vec![hid(1, &[10])]);
        let mut any = matching(vec![hid(1, &[10, 11])]);
        any.hid_any = true;
        assert_eq!(scope(&base, &any), UpdateScope::MorePermissions);
        let mut command = base.clone();
        command.commands.push("tool".into());
        assert_eq!(scope(&base, &command), UpdateScope::MorePermissions);
        let wider = uses(3, &[]);
        assert_eq!(
            update_scope(Some((&uses(1, &[]), &base)), (&wider, &base)),
            UpdateScope::MorePermissions
        );
    }

    proptest! {
        #[test]
        fn new_devices_counts_exactly_the_unseen_pids(
            previous in proptest::collection::vec(0u16..32, 0..8),
            next in proptest::collection::vec(0u16..32, 0..8),
        ) {
            let mut unseen: Vec<u16> = next.iter().copied().filter(|pid| !previous.contains(pid)).collect();
            unseen.sort_unstable();
            unseen.dedup();
            let expected = match unseen.len() {
                0 => UpdateScope::Unchanged,
                count => UpdateScope::NewDevices(count),
            };
            prop_assert_eq!(
                scope(&matching(vec![hid(1, &previous)]), &matching(vec![hid(1, &next)])),
                expected
            );
        }
    }
}

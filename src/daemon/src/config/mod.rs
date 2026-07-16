// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use halod_shared::types::{AppRule, PluginAuthority, VisibilityState, DEFAULT_PROFILE_NAME};
// Types shared with wire protocol; re-exported for backward-compat.
pub use halod_shared::types::{
    CanvasState, CoolingConfig, GuiConfig, LcdConfig, PlacedZone, RgbConfig,
};
use halod_shared::zone_transform::ZoneContentTransform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::profiles::config::Profile;
use crate::registry::config::{DeviceLayout, DeviceRecord};
use halod_shared::keyboard::KeyboardLayoutSelection;

// ── On-disk layout ───────────────────────────────────────────────────────────
//
// The single in-memory `Config` is split across several files by concern, each
// independently atomic (tmp+rename) and independently defaultable:
//   config.yaml          - active_profile + cooling/rgb/lcd/gui config
//   devices.yaml          - known_devices, device_layouts, device_transforms, sensor_visibility
//   app_rules.yaml        - app_rules
//   profiles/<slug>.yaml  - one Profile per file, named for a human to read

pub fn load() -> Result<Config> {
    let main: MainFile = load_file(&main_config_path(), "config.yaml")?;
    let devices: DevicesFile = load_file(&devices_config_path(), "devices.yaml")?;
    let app_rules: AppRulesFile = load_file(&app_rules_config_path(), "app_rules.yaml")?;
    let plugins: PluginPolicy = load_file(&plugins_config_path(), "plugins.yaml")?;
    let mut profiles = load_profiles()?;
    if profiles.is_empty() {
        profiles.insert(DEFAULT_PROFILE_NAME.to_string(), Profile::default());
    }

    let mut cooling = main.cooling;
    cooling.fan_failsafe_duty = cooling.fan_failsafe_duty.min(100);

    Ok(Config {
        active_profile: main.active_profile,
        profiles,
        known_devices: devices.known_devices,
        cooling,
        rgb: main.rgb,
        lcd: main.lcd,
        gui: main.gui,
        device_layouts: devices.device_layouts,
        sensor_visibility: devices.sensor_visibility,
        device_transforms: devices.device_transforms,
        keyboard_layouts: devices.keyboard_layouts,
        app_rules: app_rules.app_rules,
        plugins,
    })
}

pub fn save(cfg: &Config) -> Result<()> {
    atomic_write(
        &main_config_path(),
        &serde_yaml::to_string(&MainFile {
            active_profile: cfg.active_profile.clone(),
            cooling: cfg.cooling.clone(),
            rgb: cfg.rgb.clone(),
            lcd: cfg.lcd.clone(),
            gui: cfg.gui.clone(),
        })?,
    )?;
    atomic_write(
        &devices_config_path(),
        &serde_yaml::to_string(&DevicesFile {
            known_devices: cfg.known_devices.clone(),
            device_layouts: cfg.device_layouts.clone(),
            sensor_visibility: cfg.sensor_visibility.clone(),
            device_transforms: cfg.device_transforms.clone(),
            keyboard_layouts: cfg.keyboard_layouts.clone(),
        })?,
    )?;
    atomic_write(
        &app_rules_config_path(),
        &serde_yaml::to_string(&AppRulesFile {
            app_rules: cfg.app_rules.clone(),
        })?,
    )?;
    atomic_write(
        &plugins_config_path(),
        &serde_yaml::to_string(&cfg.plugins)?,
    )?;
    save_profiles(&cfg.profiles)?;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    crate::util::fs::atomic_write_str(path, contents)
}

fn load_file<T: Default + DeserializeOwned>(path: &Path, label: &str) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    let raw = read_bounded(path, label)?;
    serde_yaml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse {label} at {}: {e}", path.display()))
}

pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_PROFILES: usize = 1024;

/// Read a config file, rejecting anything past the 1 MiB ceiling using file
/// metadata so a huge file is never fully allocated first.
/// TODO: probably belongs to utils
pub(crate) fn read_bounded(path: &Path, label: &str) -> Result<String> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_CONFIG_BYTES {
            anyhow::bail!(
                "{label} at {} is too large ({} bytes)",
                path.display(),
                meta.len()
            );
        }
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "{label} at {} is too large ({} bytes)",
            path.display(),
            raw.len()
        );
    }
    Ok(raw)
}

fn load_profiles() -> Result<HashMap<String, Profile>> {
    let dir = profiles_dir();
    let mut out = HashMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    anyhow::ensure!(
        paths.len() <= MAX_PROFILES,
        "too many profile files ({} > {MAX_PROFILES})",
        paths.len()
    );
    paths.sort();
    for path in paths {
        let raw = read_bounded(&path, "profile")?;
        let mut file: ProfileFile = serde_yaml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("Failed to parse profile at {}: {e}", path.display()))?;
        crate::profiles::validate::validate_profile(&file.name, &file.profile)?;
        if let Some(canvas) = file.profile.lighting.canvas.as_mut() {
            canvas.sanitize();
        }
        out.insert(file.name, file.profile);
    }
    Ok(out)
}

/// Write every profile to its expected filename, then delete any other
/// `*.yaml` left in `profiles/` so a rename/remove doesn't orphan a stale file.
fn save_profiles(profiles: &HashMap<String, Profile>) -> Result<()> {
    let dir = profiles_dir();
    std::fs::create_dir_all(&dir)?;

    let mut expected: HashSet<String> = HashSet::new();
    for (name, profile) in profiles {
        let filename = profile_filename(name);
        let yaml = serde_yaml::to_string(&ProfileFile {
            name: name.clone(),
            profile: profile.clone(),
        })?;
        atomic_write(&dir.join(&filename), &yaml)?;
        expected.insert(filename);
    }

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().into_owned();
            if !expected.contains(&filename) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// Derive a filesystem-safe filename (with `.yaml` extension) for a profile
/// name. Names that are already a safe slug map to themselves; any lossy
/// transformation (disallowed chars, leading dot, overlong) gets an FNV-1a
/// hash of the full name appended so distinct names can't collide.
fn profile_filename(name: &str) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while slug.starts_with('.') {
        slug.remove(0);
    }
    slug.truncate(64);
    if slug.is_empty() {
        slug = "profile".to_string();
    }
    if slug == name {
        format!("{slug}.yaml")
    } else {
        format!("{slug}-{:016x}.yaml", fnv1a64(name))
    }
}

fn fnv1a64(s: &str) -> u64 {
    use std::hash::Hasher;
    let mut h = fnv::FnvHasher::default();
    h.write(s.as_bytes());
    h.finish()
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HALOD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(appdata).join(halod_shared::app::APP_NAME)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join(halod_shared::app::APP_NAME)
    }
}

fn main_config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

fn devices_config_path() -> PathBuf {
    config_dir().join("devices.yaml")
}

fn app_rules_config_path() -> PathBuf {
    config_dir().join("app_rules.yaml")
}

fn plugins_config_path() -> PathBuf {
    config_dir().join("plugins.yaml")
}

fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

/// Directory where uploaded LCD images are stored persistently.
/// All uploaded images accumulate here; profiles reference them by filename only.
pub fn lcd_images_dir() -> PathBuf {
    config_dir().join(halod_shared::types::LCD_IMAGES_SUBDIR)
}

/// Directory holding standalone plugin packages (`plugin.yaml` plus entry
/// source), read at startup.
pub fn plugins_dir() -> PathBuf {
    config_dir().join("plugins")
}

/// Directory holding checked-out git-repo plugin sources, one subdirectory per repo (see `PluginRepoRecord`).
pub fn plugin_repos_dir() -> PathBuf {
    config_dir().join("plugin_repos")
}

/// Immutable revisions materialized from the daemon's release-embedded plugin
/// bundle. Kept outside Git checkout roots so a later official clone can use
/// the normal mutable repository location unchanged.
pub fn embedded_plugin_revisions_dir() -> PathBuf {
    config_dir().join("embedded_plugin_revisions")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MainFile {
    #[serde(default = "default_profile_name")]
    active_profile: String,
    // The daemon's persisted RGB config key is `rgb`; the wire side names the
    // same struct `lighting.config`.
    #[serde(default)]
    cooling: CoolingConfig,
    #[serde(default)]
    rgb: RgbConfig,
    #[serde(default)]
    lcd: LcdConfig,
    #[serde(default)]
    gui: GuiConfig,
}

impl Default for MainFile {
    fn default() -> Self {
        Self {
            active_profile: default_profile_name(),
            cooling: CoolingConfig::default(),
            rgb: RgbConfig::default(),
            lcd: LcdConfig::default(),
            gui: GuiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    known_devices: HashMap<String, DeviceRecord>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    device_layouts: HashMap<String, DeviceLayout>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    sensor_visibility: HashMap<String, VisibilityState>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    device_transforms: HashMap<String, HashMap<String, ZoneContentTransform>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    keyboard_layouts: HashMap<String, KeyboardLayoutSelection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppRulesFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    app_rules: Vec<AppRule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPolicy {
    /// Authority snapshots accepted through the enable modal.  They are kept
    /// separately from the runtime permission projection so repository updates
    /// can decide whether a changed manifest expands authority.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub accepted_authorities: HashMap<String, PluginAuthority>,
    /// Plugin ids explicitly enabled through the authority confirmation flow.
    /// An absent id is inert, which makes a fresh consent decision required.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled: Vec<String>,
    /// Non-secure user-editable config values per plugin id (key -> value).
    /// Fields declared `secure = true` never appear here — see the encrypted
    /// secret store instead.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config: HashMap<String, HashMap<String, String>>,
    /// Integration ids explicitly enabled independently of package activation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub integrations_enabled: Vec<String>,
    /// Content hash (hex SHA-256) of the installed plugin package, keyed by
    /// plugin id. This identifies updates and modified checkouts; it is never
    /// used as a consent gate.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub installed_hashes: HashMap<String, String>,
    /// Registered git-repo plugin sources. See `PluginRepoRecord`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<PluginRepoRecord>,
}

/// A registered git-repo plugin source, pinned to a commit SHA that only an explicit "update" advances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRepoRecord {
    pub url: String,
    /// Directory name under `plugin_repos_dir()`, derived from the URL at `add_repo` time
    /// (fixed to `constants::OFFICIAL_PLUGIN_REPO_SLUG` for the seeded official repo).
    pub slug: String,
    /// Stable identity declared by `repository.yaml`.  Pinning this prevents a
    /// URL or branch from silently becoming a different repository.
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Commit SHA the checked-out working tree is pinned to.
    pub locked_sha: String,
    /// Immutable revision directory currently selected for execution. `None`
    /// means the source is registered but has not installed a revision yet.
    #[serde(default)]
    pub active_revision: Option<String>,
    /// Storage backing the active revision. Older configurations predate
    /// embedded bundles and therefore resolve to the managed Git revision.
    #[serde(default)]
    pub active_source: PluginRevisionSource,
    /// Last verified official revision retained for rollback diagnostics.
    #[serde(default)]
    pub previous_verified_sha: Option<String>,
    /// When this repo's clone directory was last cloned/fetched/checked out
    /// (RFC 3339), for the GUI's repo detail panel. `None` until the first sync.
    #[serde(default)]
    pub last_sync: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRevisionSource {
    Embedded,
    #[default]
    Managed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileFile {
    name: String,
    #[serde(flatten)]
    profile: Profile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile_name")]
    pub active_profile: String,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default)]
    pub known_devices: HashMap<String, DeviceRecord>,
    #[serde(default)]
    pub cooling: CoolingConfig,
    #[serde(default)]
    pub rgb: RgbConfig,
    #[serde(default)]
    pub lcd: LcdConfig,
    #[serde(default)]
    pub gui: GuiConfig,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub device_layouts: HashMap<String, DeviceLayout>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sensor_visibility: HashMap<String, VisibilityState>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub device_transforms: HashMap<String, HashMap<String, ZoneContentTransform>>,
    /// device_id → keyboard layout selection. Absent = Auto/Auto (both axes
    /// resolve from the firmware-detected language).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub keyboard_layouts: HashMap<String, KeyboardLayoutSelection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub app_rules: Vec<AppRule>,
    pub plugins: PluginPolicy,
}

fn default_profile_name() -> String {
    DEFAULT_PROFILE_NAME.to_string()
}

impl Default for Config {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(DEFAULT_PROFILE_NAME.to_string(), Profile::default());
        Self {
            active_profile: DEFAULT_PROFILE_NAME.to_string(),
            profiles,
            known_devices: HashMap::new(),
            cooling: CoolingConfig::default(),
            rgb: RgbConfig::default(),
            lcd: LcdConfig::default(),
            gui: GuiConfig::default(),
            device_layouts: HashMap::new(),
            sensor_visibility: HashMap::new(),
            device_transforms: HashMap::new(),
            keyboard_layouts: HashMap::new(),
            app_rules: Vec::new(),
            plugins: PluginPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_config_close_to_tray_defaults_to_true_when_field_absent() {
        let yaml = "log_level: info";
        let cfg: GuiConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.close_to_tray);
    }

    #[test]
    fn load_handles_missing_valid_and_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        // Missing files -> defaults.
        let cfg = load().unwrap();
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
        assert!(cfg.profiles.contains_key(DEFAULT_PROFILE_NAME));

        // Valid main file -> parsed values.
        std::fs::write(dir.path().join("config.yaml"), "active_profile: gaming\n").unwrap();
        let cfg = load().unwrap();
        assert_eq!(cfg.active_profile, "gaming");

        // Malformed YAML in a section file -> error naming that file, not silent defaults.
        std::fs::write(
            dir.path().join("devices.yaml"),
            "known_devices: [unterminated\n",
        )
        .unwrap();
        let err = load().unwrap_err();
        assert!(err.to_string().contains("devices.yaml"));

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[test]
    fn save_then_load_round_trips_every_section() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let mut cfg = Config {
            active_profile: "Gäming Setup".to_string(),
            ..Config::default()
        };
        let mut gaming = Profile::default();
        gaming
            .device_states
            .insert("dev1".into(), serde_json::json!({ "fan_curve": {"a": 1} }));
        gaming.lighting.canvas = Some(CanvasState {
            sample_radius: 9.0,
            ..Default::default()
        });
        cfg.profiles.insert("Gäming Setup".to_string(), gaming);
        cfg.known_devices.insert(
            "dev1".into(),
            DeviceRecord {
                name: "Dev One".into(),
                vendor: "Acme".into(),
                model: "X1".into(),
                active_state: Default::default(),
            },
        );
        cfg.device_layouts.insert(
            "hub1".into(),
            DeviceLayout {
                channels: HashMap::new(),
            },
        );
        cfg.sensor_visibility
            .insert("sensor1".into(), Default::default());
        cfg.keyboard_layouts.insert(
            "kbd1".into(),
            KeyboardLayoutSelection {
                variant: Some(halod_shared::keyboard::KeyVariant::Iso),
                language: Some(halod_shared::types::KeyboardLayout::CH),
            },
        );
        cfg.app_rules.push(AppRule {
            process_names: vec!["game.exe".into()],
            profile: "Gäming Setup".into(),
            enabled: true,
        });
        cfg.gui.seen_tours.insert("page:home".into());
        cfg.cooling.fan_failsafe_duty = 60;
        cfg.rgb.canvas_fps = 45;
        cfg.lcd.enabled = false;
        cfg.plugins.enabled.push("wled_udp".into());
        cfg.plugins.accepted_authorities.insert(
            "wled_udp".into(),
            PluginAuthority {
                permissions: vec![halod_shared::types::Permission::Network],
                transport_scopes: vec![],
            },
        );
        cfg.plugins.config.insert(
            "openrgb".into(),
            HashMap::from([("host".to_string(), "127.0.0.1".to_string())]),
        );
        cfg.plugins
            .installed_hashes
            .insert("wled_udp".into(), "abc123".into());

        save(&cfg).unwrap();
        let reloaded = load().unwrap();

        assert_eq!(reloaded.plugins.enabled, vec!["wled_udp".to_string()]);
        assert_eq!(
            reloaded
                .plugins
                .accepted_authorities
                .get("wled_udp")
                .map(|authority| &authority.permissions),
            Some(&vec![halod_shared::types::Permission::Network])
        );
        assert_eq!(
            reloaded.plugins.installed_hashes.get("wled_udp"),
            Some(&"abc123".to_string())
        );
        assert_eq!(
            reloaded
                .plugins
                .config
                .get("openrgb")
                .and_then(|m| m.get("host")),
            Some(&"127.0.0.1".to_string())
        );

        assert_eq!(reloaded.active_profile, cfg.active_profile);
        assert_eq!(reloaded.profiles.len(), cfg.profiles.len());
        assert_eq!(
            reloaded.profiles["Gäming Setup"].device_states["dev1"]["fan_curve"]["a"],
            1
        );
        assert_eq!(
            reloaded.profiles["Gäming Setup"]
                .lighting
                .canvas
                .as_ref()
                .unwrap()
                .sample_radius,
            9.0
        );
        assert_eq!(reloaded.known_devices["dev1"].name, "Dev One");
        assert!(reloaded.device_layouts.contains_key("hub1"));
        assert!(reloaded.sensor_visibility.contains_key("sensor1"));
        assert_eq!(
            reloaded.keyboard_layouts["kbd1"].language,
            Some(halod_shared::types::KeyboardLayout::CH)
        );
        assert_eq!(reloaded.app_rules.len(), 1);
        assert!(reloaded.gui.seen_tours.contains("page:home"));
        assert_eq!(reloaded.cooling.fan_failsafe_duty, 60);
        assert_eq!(reloaded.rgb.canvas_fps, 45);
        assert!(!reloaded.lcd.enabled);

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[test]
    fn save_prunes_a_removed_profiles_file() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let mut cfg = Config::default();
        cfg.profiles.insert("Gaming".into(), Profile::default());
        save(&cfg).unwrap();
        assert!(dir.path().join("profiles/Gaming.yaml").exists());

        cfg.profiles.remove("Gaming");
        save(&cfg).unwrap();
        assert!(!dir.path().join("profiles/Gaming.yaml").exists());
        assert!(dir
            .path()
            .join("profiles")
            .join(profile_filename(DEFAULT_PROFILE_NAME))
            .exists());

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[test]
    fn profile_filename_is_readable_for_a_clean_name() {
        assert_eq!(profile_filename("Gaming"), "Gaming.yaml");
    }

    #[test]
    fn profile_filename_sanitizes_unsafe_chars() {
        let f = profile_filename("../etc/passwd");
        assert!(!f.contains('/'));
        assert!(f.ends_with(".yaml"));
        assert!(!f.starts_with('.'));
    }

    proptest::proptest! {
        #[test]
        fn profile_filename_property_all_invariants_hold(name in ".*") {
            let f = profile_filename(&name);
            assert!(!f.is_empty());
            assert!(f.ends_with(".yaml"));
            assert!(!f.starts_with('.'));
            assert!(
                f.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                "filename {f} has unsafe characters"
            );
            assert!(f.len() <= 64 + 1 + 16 + 5, "filename {f} unexpectedly long");
        }

        #[test]
        fn profile_filename_property_distinct_names_produce_distinct_files(
            a in ".{1,80}", b in ".{1,80}"
        ) {
            if a != b {
                assert_ne!(profile_filename(&a), profile_filename(&b));
            }
        }
    }

    #[test]
    fn placed_zone_effect_defaults_to_none() {
        let yaml = "device_id: d\nzone_id: z\nx: 0.0\ny: 0.0\n";
        let z: PlacedZone = serde_yaml::from_str(yaml).unwrap();
        assert!(z.effect.is_none());
    }

    #[test]
    fn plugin_policy_rejects_retired_fields() {
        let current: PluginPolicy = serde_yaml::from_str("enabled: [foo]\n").unwrap();
        assert_eq!(current.enabled, ["foo"]);
        assert!(serde_yaml::from_str::<PluginPolicy>("disabled: [foo]\n").is_err());
        assert!(serde_yaml::from_str::<PluginPolicy>("acknowledged: {}\n").is_err());
    }

    #[test]
    fn plugin_repos_round_trip_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let mut cfg = Config::default();
        cfg.plugins.repos.push(PluginRepoRecord {
            url: "https://example.com/foo.git".into(),
            slug: "foo".into(),
            repository_id: None,
            branch: Some("main".into()),
            locked_sha: "deadbeef".into(),
            active_revision: Some("deadbeef".into()),
            active_source: PluginRevisionSource::Managed,
            previous_verified_sha: None,
            last_sync: None,
        });
        save(&cfg).unwrap();
        let reloaded = load().unwrap();

        assert_eq!(reloaded.plugins.repos.len(), 1);
        assert_eq!(reloaded.plugins.repos[0].slug, "foo");
        assert_eq!(reloaded.plugins.repos[0].locked_sha, "deadbeef");
        assert_eq!(reloaded.plugins.repos[0].branch.as_deref(), Some("main"));

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }
}

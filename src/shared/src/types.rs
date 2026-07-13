use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

pub use crate::commands::ScreenRotation;
use crate::zone_transform::ZoneContentTransform;

pub const DEFAULT_PROFILE_NAME: &str = "default";

/// Subdirectory (relative to the daemon's config dir) where uploaded LCD
/// images live. Shared by the daemon (writes/serves them) and the GUI (reads
/// them straight off disk using the daemon-reported `config_dir`).
pub const LCD_IMAGES_SUBDIR: &str = "media/lcd_images";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdEngineTemplateDescriptor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireLcdEngineState {
    pub available_templates: Vec<LcdEngineTemplateDescriptor>,
    /// device_id → template_id
    pub device_templates: HashMap<String, String>,
    /// device_id → (param id → value) for the device's active template, so
    /// the editor can seed itself from a running custom template.
    #[serde(default)]
    pub device_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
}

/// Progress of an in-flight LCD image upload, pushed to the uploading client
/// so its spinner can show the work is alive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdUploadProgress {
    pub device_id: String,
    pub stage: LcdUploadStage,
    /// 0–100 within the stage; `None` when the stage has no measurable extent.
    #[serde(default)]
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdUploadStage {
    /// Decoding / resizing the uploaded file (GIFs report per-frame percent).
    Processing,
    /// Writing the processed image to the device.
    Applying,
    /// The image finished uploading and was applied to the device (terminal).
    Done,
    /// The upload or device write failed and was aborted (terminal).
    Failed,
}

/// A rendered LCD engine frame broadcast to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdEngineFrame {
    pub device_id: String,
    pub frame_id: u64,
    /// Base64-encoded PNG preview image.
    pub preview_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedFrameEntry {
    pub device_id: String,
    pub zone_id: String,
    pub led_id: u32,
    pub color: RgbColor,
}

/// A rendered canvas frame broadcast to subscribed clients.
///
/// frame_id is monotonically increasing; gaps indicate frames the daemon dropped
/// (broadcast channel Lagged). Frontend derives:
///   FPS  = frames_received / elapsed_seconds (rolling 1s window using timestamp_ms)
///   Lost = Σ (frame_id[n] - frame_id[n-1] - 1) across received frames
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasFrame {
    pub frame_id: u64,
    pub timestamp_ms: u64,
    pub canvas_srgb_b64: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub led_colors: Vec<LedFrameEntry>,
}

// Shared config/wire defaults
// Single source of truth for values that both the daemon's persisted config
// and the wire messages default to.

/// Default width/height of a placed canvas zone, as a fraction of the canvas.
pub const DEFAULT_ZONE_SIZE: f32 = 0.15;
/// Default canvas sampling radius, in pixels.
pub const DEFAULT_SAMPLE_RADIUS: f32 = 3.0;
/// Default for "minimize to tray instead of quitting".
pub const DEFAULT_CLOSE_TO_TRAY: bool = true;
pub const DEFAULT_SUPPRESS_DEPENDENCY_WARNING: bool = false;
/// Default for "hide the custom window controls" (off — controls shown).
pub const DEFAULT_HIDE_WINDOW_CONTROLS: bool = false;
/// Milliseconds between fan-curve engine ticks.
pub const DEFAULT_FAN_CURVE_TICK_MS: u64 = 2000;
/// Canvas engine frame rate.
pub const DEFAULT_CANVAS_FPS: u32 = 20;
/// LCD engine frame rate.
pub const DEFAULT_LCD_FPS: u32 = 20;
/// Duty (0-100) applied when a fan's assigned sensor is absent.
pub const DEFAULT_FAN_FAILSAFE_DUTY: u8 = 75;
/// Default log level.
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// Default UI language (BCP-47-ish code; the GUI owns the catalog).
pub const DEFAULT_LANGUAGE: &str = "en";
/// UI language codes both sides agree on. The GUI has a catalog for each and
/// the daemon accepts only these in `SetLanguage`; add a code here when adding
/// a set of `locales/*.<code>.yaml` catalogs.
pub const SUPPORTED_LANGUAGES: &[&str] = &["en", "it"];
/// Default for the per-engine enable toggles.
pub const DEFAULT_ENGINE_ENABLED: bool = true;
/// Keepalive interval for LCD preview; kept well under `LCD_PREVIEW_LEASE_SECS`.
pub const LCD_PREVIEW_KEEPALIVE_SECS: f64 = 1.0;
/// Lease timeout for LCD preview; daemon pauses streaming after this.
pub const LCD_PREVIEW_LEASE_SECS: u64 = 3;

fn default_zone_size() -> f32 {
    DEFAULT_ZONE_SIZE
}

/// A write-rate ceiling, enforced generically at the transport layer via
/// `Metered`/`WriteRateLimiter` regardless of caller. There is no default
/// ceiling — a transport is constructed with `None` unless a device
/// explicitly opts in; everything else is unthrottled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRateLimit {
    pub max_bytes_per_sec: u32,
}

/// Live write-rate limit and throughput for a device, surfaced to the GUI.
/// `limit` is `None` when the device hasn't declared a ceiling (the default
/// for every device today) — live throughput is still measured and shown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct WriteRateStatus {
    pub limit: Option<WriteRateLimit>,
    pub current_writes_per_sec: f32,
    pub current_bytes_per_sec: f32,
    pub rejected_total: u64,
}

/// Clamp a float to `fallback` when it is non-finite (NaN/Inf). Keeps a
/// hand-edited config's stray NaN from panicking the JSON broadcast, which
/// rejects non-finite floats.
pub fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

/// Validate a user-supplied LCD image filename before joining it onto the
/// image library directory. Allowlist-based (not a traversal denylist): only a
/// bare `[A-Za-z0-9._-]` name with a known image extension and no leading dot
/// is accepted, ruling out path separators, `..`, absolute/drive-relative
/// paths, and NTFS alternate data streams. Shared so the GUI can pre-validate
/// before sending and the daemon can enforce on receipt.
pub fn validate_image_filename(name: &str) -> Result<(), &'static str> {
    let ext_ok = matches!(
        std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg" | "gif")
    );
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && ext_ok
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err("invalid filename")
    }
}

/// Sniff an image's format from its magic bytes, returning a file extension.
pub fn sniff_ext(data: &[u8]) -> &'static str {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "gif"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else {
        "unknown"
    }
}

/// Max encoded size of any plugin display asset (logo/effect thumbnail) served
/// over IPC. Bounds a careless or hostile plugin from bloating the socket and
/// the GUI's texture memory. Enforced by the daemon on read.
pub const MAX_PLUGIN_ASSET_BYTES: u64 = 256 * 1024;

/// Max pixel width/height of a plugin logo. It's painted into a small square
/// tile, so anything larger is wasted decode/memory.
pub const MAX_PLUGIN_LOGO_DIM: u32 = 512;

/// Max long:short side ratio of a plugin logo. The tile is square and the GUI
/// letterboxes to fit, but an extreme banner would shrink to an unreadable
/// sliver — reject it outright rather than display it.
pub const MAX_PLUGIN_LOGO_ASPECT: u32 = 2;

/// Validate a decoded plugin logo's dimensions: non-zero, within
/// [`MAX_PLUGIN_LOGO_DIM`], and no more lopsided than [`MAX_PLUGIN_LOGO_ASPECT`].
/// Pure so both the daemon (on load) and the GUI (on import) can enforce it.
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

/// How the canvas sampler maps LEDs to pixmap positions for a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SamplingMode {
    /// Sample at each LED's real spatial position on the canvas (today).
    #[default]
    Spatial,
    /// Ring-topology zones: LEDs are laid along the zone rect by chain index.
    Unrolled,
}

/// A canvas zone placement — persisted in the daemon's config and sent to the
/// GUI verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlacedZone {
    pub device_id: String,
    pub zone_id: String,
    pub x: f32,
    pub y: f32,
    #[serde(default = "default_zone_size")]
    pub w: f32,
    #[serde(default = "default_zone_size")]
    pub h: f32,
    #[serde(default)]
    pub rotation: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(default)]
    pub sampling_mode: SamplingMode,
}

/// Canvas lighting state. Persisted in the daemon's config (the `available_*`
/// fields are skipped when empty so they don't pollute the YAML) and sent to
/// the GUI with the `available_*` catalogs filled in at serialization time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasState {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available_effects: Vec<Animation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available_direct_effects: Vec<Animation>,
    /// Saved custom "Designer" effects (see `effect_designer`), for display in
    /// the RGB Lighting effect grid alongside the built-ins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub custom_direct_effects: Vec<EffectDef>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub effects: HashMap<String, EffectDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effect: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub placed_zones: Vec<PlacedZone>,
    pub sample_radius: f32,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            available_effects: Vec::new(),
            available_direct_effects: Vec::new(),
            custom_direct_effects: Vec::new(),
            effects: HashMap::new(),
            default_effect: None,
            placed_zones: Vec::new(),
            sample_radius: DEFAULT_SAMPLE_RADIUS,
        }
    }
}

/// Cooling engine config, nested into [`CoolingState`]. Persisted on the daemon
/// under `config.yaml`'s `cooling` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoolingConfig {
    pub fan_curve_enabled: bool,
    /// Milliseconds between fan-curve ticks.
    pub fan_curve_tick_ms: u64,
    /// Duty (0-100) applied when a fan's assigned sensor is absent.
    pub fan_failsafe_duty: u8,
}

impl Default for CoolingConfig {
    fn default() -> Self {
        Self {
            fan_curve_enabled: DEFAULT_ENGINE_ENABLED,
            fan_curve_tick_ms: DEFAULT_FAN_CURVE_TICK_MS,
            fan_failsafe_duty: DEFAULT_FAN_FAILSAFE_DUTY,
        }
    }
}

/// RGB engine config, nested into [`LightingState`]. Persisted on the daemon
/// under `config.yaml`'s `rgb` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RgbConfig {
    pub canvas_enabled: bool,
    pub canvas_fps: u32,
}

impl Default for RgbConfig {
    fn default() -> Self {
        Self {
            canvas_enabled: DEFAULT_ENGINE_ENABLED,
            canvas_fps: DEFAULT_CANVAS_FPS,
        }
    }
}

/// LCD engine config, nested into [`LcdState`]. Persisted on the daemon under
/// `config.yaml`'s `lcd` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LcdConfig {
    pub enabled: bool,
    pub fps: u32,
}

impl Default for LcdConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENGINE_ENABLED,
            fps: DEFAULT_LCD_FPS,
        }
    }
}

/// Whether the daemon may contact GitHub to download official plugins and
/// check for updates automatically. `Unset` until the user is first asked, so
/// the GUI knows to show the first-run consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginDownloadConsent {
    #[default]
    Unset,
    Allowed,
    Denied,
}

/// GUI-facing preferences and daemon log level, sent as `AppState.gui` and
/// persisted on the daemon under `config.yaml`'s `gui` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    /// UI language code (e.g. "en", "it"); the GUI translates against it.
    pub language: String,
    pub close_to_tray: bool,
    pub suppress_dependency_warning: bool,
    /// Hide the custom title-bar window controls (minimize/maximize/close),
    /// for tiling window managers that drive those actions themselves.
    pub hide_window_controls: bool,
    /// Persistence keys (e.g. "page:home", "tab:cooling") of tutorials the
    /// user has completed or skipped.
    pub seen_tours: BTreeSet<String>,
    pub log_level: String,
    pub plugin_downloads: PluginDownloadConsent,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            language: DEFAULT_LANGUAGE.to_string(),
            close_to_tray: DEFAULT_CLOSE_TO_TRAY,
            suppress_dependency_warning: DEFAULT_SUPPRESS_DEPENDENCY_WARNING,
            hide_window_controls: DEFAULT_HIDE_WINDOW_CONTROLS,
            seen_tours: BTreeSet::new(),
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            plugin_downloads: PluginDownloadConsent::Unset,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub target: String,
    pub message: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

/// A user-visible notification identified by a stable code plus structured,
/// runtime-supplied parameters (device ids, error text, …). The daemon emits
/// these; the GUI owns all human-readable copy and translation. `severity()`
/// is derived from the variant so it is not carried separately on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum NotificationCode {
    EngineStopped {
        detail: String,
    },
    KeyRemapUnavailable {
        detail: String,
    },
    WirelessReinitFailed {
        device: String,
    },
    DeviceReconnectFailed {
        device: String,
    },
    ProfileSwitched {
        profile: String,
    },
    ChainLinkRestoreFailed {
        name: String,
        detail: String,
    },
    DeviceInitFailed {
        device: String,
        detail: String,
    },
    FanStalled {
        fan: String,
    },
    /// An auto-discovered plugin (found by a directory scan, not a manual
    /// "Add plugin" import — those get a blocking consent modal instead)
    /// declares permissions the user hasn't granted yet, so it stays inert.
    PluginNeedsPermission {
        plugin: String,
    },
    /// A plugin's on-disk content hash changed since it was last acknowledged,
    /// without going through the update flow (e.g. a manual edit). Emitted for
    /// plugins that declare no permissions, so the change is still surfaced —
    /// permission-declaring plugins are covered by `PluginNeedsPermission`.
    PluginContentChanged {
        plugin: String,
    },
    /// A plugin device's Lua callback failed at runtime on a background path
    /// (engine tick, sensor poll) where the error would otherwise only be
    /// logged. Deduplicated daemon-side so a persistently-failing plugin alerts
    /// once, not every frame. `detail` is the error text, shown verbatim.
    PluginRuntimeError {
        plugin: String,
        detail: String,
    },
    /// A generic error surfaced as free text (e.g. a failed command's error).
    /// The GUI translates only the title and shows `message` verbatim.
    Generic {
        message: String,
    },
}

impl NotificationCode {
    pub fn severity(&self) -> NotificationSeverity {
        use NotificationCode::*;
        match self {
            EngineStopped { .. }
            | ChainLinkRestoreFailed { .. }
            | DeviceInitFailed { .. }
            | Generic { .. } => NotificationSeverity::Error,
            KeyRemapUnavailable { .. }
            | WirelessReinitFailed { .. }
            | DeviceReconnectFailed { .. }
            | FanStalled { .. }
            | PluginNeedsPermission { .. }
            | PluginContentChanged { .. }
            | PluginRuntimeError { .. } => NotificationSeverity::Warning,
            ProfileSwitched { .. } => NotificationSeverity::Info,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(flatten)]
    pub code: NotificationCode,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningApp {
    pub process_name: String,
    pub display_name: String,
    pub icon_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRule {
    pub process_names: Vec<String>,
    pub profile: String,
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn bool_true() -> bool {
    true
}

/// Per-profile device/zone selection for the global RGB Lighting view.
/// Empty `device_ids` = nothing selected. A device absent from `zones`
/// targets all of its zones.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LightingTargets {
    #[serde(default)]
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub zones: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileOverrides {
    /// device_id -> overridden capability state_keys. `BTreeMap` for
    /// deterministic serialization order, which UI change-detection hashes.
    #[serde(default)]
    pub device_capabilities: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub canvas: bool,
    /// True when the active profile is `default` (nothing is overridable).
    #[serde(default)]
    pub active_is_default: bool,
}

/// Active profile, the available profile names, app-focus rules, and the
/// active profile's per-device capability overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileState {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub available: Vec<String>,
    #[serde(default)]
    pub app_rules: Vec<AppRule>,
    #[serde(default)]
    pub overrides: ProfileOverrides,
}

/// Results of the daemon's host-capability probes, gating optional UI features.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthCheckState {
    /// False when no supported compositor/platform backend was found.
    /// Default false so the UI doesn't flash "supported" before the first broadcast.
    #[serde(default)]
    pub focus_watcher_supported: bool,
    /// Whether `ffmpeg` is available on the daemon's host, gating LCD video mode.
    #[serde(default)]
    pub ffmpeg_available: bool,
}

/// Device plugins and their pending-apply state, for the Plugins screen.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    /// Device plugins discovered in the plugins directory, with their
    /// enable/disable state, for the Plugins management screen.
    #[serde(default)]
    pub plugins: Vec<PluginInfo>,
    /// True when a plugin enable/disable/grant/import/delete has been staged
    /// but not yet applied to live devices — the Plugins screen invites the
    /// user to apply it explicitly rather than rediscovering on every edit.
    #[serde(default)]
    pub rediscover_pending: bool,
    /// Registered git-repo plugin sources, for the "Plugin Repositories" section of the Plugins screen.
    #[serde(default)]
    pub repos: Vec<PluginRepoInfo>,
}

/// A registered git-repo plugin source, as shown in the GUI. Mirrors the daemon's `PluginRepoRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRepoInfo {
    pub url: String,
    pub slug: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub locked_sha: String,
    /// When this repo was last cloned/fetched/checked out (RFC 3339), if ever.
    #[serde(default)]
    pub last_sync: Option<String>,
    /// True for the seeded official repo — the GUI hides its remove control.
    #[serde(default)]
    pub official: bool,
}

/// One repo's update check result, reported in reply to `DaemonCommand::CheckPluginRepoUpdates`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoUpdateStatus {
    pub slug: String,
    pub locked_sha: String,
    pub remote_sha: String,
    pub behind: bool,
}

/// One plugin's update-availability, reported in reply to
/// `DaemonCommand::CheckPluginUpdates` — finer-grained than [`RepoUpdateStatus`]:
/// a repo can be behind while a given plugin's own content is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUpdateStatus {
    pub plugin_id: String,
    pub slug: String,
    /// The repo's checked-out (`locked_sha`) content differs from the remote
    /// tip — a genuine upstream update the user can pull.
    pub update_available: bool,
    /// The on-disk content differs from what the repo checked out — a local
    /// edit (or tampering), not an upstream change. Distinct from
    /// `update_available` so the GUI can say "modified on disk" rather than
    /// "update available".
    #[serde(default)]
    pub on_disk_changed: bool,
    pub current_version: String,
    pub available_version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub discovery: DiscoveryStatus,
    #[serde(default)]
    pub devices: Vec<WireDevice>,
    #[serde(default)]
    pub profiles: ProfileState,
    #[serde(default)]
    pub cooling: CoolingState,
    #[serde(default)]
    pub lighting: LightingState,
    #[serde(default)]
    pub lcd: LcdState,
    #[serde(default)]
    pub gui: GuiConfig,
    #[serde(default)]
    pub log_entries: Vec<LogEntry>,
    /// Filesystem path to the daemon's config directory; the UI displays and
    /// opens it without recomputing client-side.
    #[serde(default)]
    pub config_dir: String,
    #[serde(default)]
    pub health: HealthCheckState,
    /// Resolved `process_name -> icon` for every process referenced by an app
    /// rule, so the UI can show app icons on rule badges without re-resolving.
    /// On Linux the icon is a theme name or absolute path from the matching
    /// `.desktop` file; on Windows an absolute path to a cached PNG.
    #[serde(default)]
    pub process_icons: HashMap<String, String>,
    #[serde(default)]
    pub plugins: PluginsState,
}

/// A privileged capability a plugin must declare before the daemon grants it —
/// the enforcement boundary between "trusted to talk to its matched device"
/// (every plugin) and "trusted to reach outside it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Open network connections (e.g. a TCP client to a local SDK server).
    Network,
    /// Reach OS-level primitives beyond pure computation (currently: clock
    /// reads via `os.time`/`os.clock`).
    Os,
    /// Read this plugin's own decrypted secret config values (`secure = true`
    /// fields) via `halod.config`. Non-secure config values need no permission.
    SecureStorage,
    /// Scan, read, or write SMBus/I2C devices (the `smbus` transport backend and
    /// `pre_scan`). A raw bus grants access to every device on it, so it is an
    /// explicit grant separate from a plugin's matched-hardware access.
    Smbus,
    /// Create/route host audio sinks (`dev.audio`) via the `pactl`-backed sink
    /// registry. Gates `AudioApi` so plugins cannot spawn host audio modules
    /// without consent.
    AudioRouting,
}

/// Which discovery path a plugin registers into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Declares hardware via `match` and (optionally) device capabilities.
    #[default]
    Device,
    /// Declares RGB effects only; never opens a transport.
    Effect,
    /// Instantiated from its own config values (e.g. a server host/port)
    /// rather than a hardware discovery handle; its children are the
    /// individual things the remote service reports.
    Integration,
}

/// One effect's thumbnail; display-only, the GUI fetches the bytes via `GetPluginAsset`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEffectAsset {
    pub id: String,
    /// Asset name to pass as `GetPluginAsset { name, .. }`.
    pub thumbnail: String,
}

/// Where a plugin came from: local disk, or a registered git repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    #[default]
    Local,
    Repo {
        slug: String,
    },
}

/// One plugin as shown in the GUI's Plugins screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    /// Filesystem path of the script.
    pub path: String,
    #[serde(default)]
    pub plugin_type: PluginKind,
    /// Human-readable capability labels the plugin declares (e.g. "RGB", "Fan").
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Display names of the RGB effects the plugin declares (device plugins
    /// may bundle effects alongside their hardware capabilities).
    #[serde(default)]
    pub effect_names: Vec<String>,
    pub enabled: bool,
    /// Plugin author, as declared in the manifest (empty when unset).
    #[serde(default)]
    pub author: String,
    /// Plugin version string, as declared in the manifest (empty when unset).
    #[serde(default)]
    pub version: String,
    /// Free-text description from the manifest (empty when unset).
    #[serde(default)]
    pub description: String,
    /// Device labels the plugin targets, derived from its declared devices.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Declared license, as an SPDX identifier or free-text name (empty when unset).
    #[serde(default)]
    pub license: String,
    /// Every device this plugin declares (empty for an `effect`/`integration` plugin).
    #[serde(default)]
    pub devices: Vec<PluginDeviceInfo>,
    /// Display-only logo asset name, if declared. Fetch via `GetPluginAsset`.
    #[serde(default)]
    pub logo: Option<String>,
    /// Per-effect thumbnail asset names, if declared.
    #[serde(default)]
    pub effect_thumbnails: Vec<PluginEffectAsset>,
    /// Where this plugin came from (local disk vs. a registered git repo).
    #[serde(default)]
    pub source: PluginSource,
    /// Privileged capabilities the manifest declares.
    #[serde(default)]
    pub declared_permissions: Vec<Permission>,
    /// Subset of `declared_permissions` the user has granted. A plugin whose
    /// declared permissions aren't fully granted is inert (discovered but not
    /// activated) until the user accepts them.
    #[serde(default)]
    pub granted_permissions: Vec<Permission>,
    /// User-editable config fields the plugin declares (e.g. a server IP).
    #[serde(default)]
    pub config_fields: Vec<PluginConfigField>,
    /// Current values of the plugin's non-secure config fields, keyed by
    /// field key. Secure fields never appear here — see `secret_set`.
    #[serde(default)]
    pub config_values: HashMap<String, String>,
    /// Whether a secret is currently stored for each secure config field
    /// (keyed by field key). The GUI shows "set"/"not set"; the secret's
    /// plaintext never crosses the IPC boundary.
    #[serde(default)]
    pub secret_set: HashMap<String, bool>,
    /// For a `PluginKind::Integration` plugin, whether the *integration
    /// itself* is enabled — independent of `enabled` (which only governs
    /// whether its Lua may run at all). Always `true` for a non-integration
    /// plugin, where the field is meaningless.
    #[serde(default = "default_true")]
    pub integration_enabled: bool,
    /// Whether the user has consented to running this exact script: its content
    /// hash matches the acknowledged one and every declared permission is
    /// granted. `false` for a never-acknowledged or since-modified disk plugin
    /// (which stays inert until re-consented). Always `true` for built-ins.
    #[serde(default = "default_true")]
    pub consented: bool,
    /// Whether the script on disk differs from the version the user last
    /// consented to (a grant existed but the content hash no longer matches).
    /// Drives the "this plugin was modified since you allowed it" prompt.
    #[serde(default)]
    pub content_changed: bool,
}

fn default_true() -> bool {
    true
}

/// One device a `device`-type plugin declares (see [`PluginKind`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDeviceInfo {
    pub vendor: String,
    pub model: String,
    /// Display name (the device's `name` override, or `model`).
    pub name: String,
    #[serde(default)]
    pub device_type: Option<DeviceType>,
}

/// Interpretation hint for a [`PluginConfigField`] value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginConfigFieldKind {
    #[default]
    Text,
    Number,
}

/// One user-editable setting a plugin declares (mirrors the manifest's
/// `ConfigFieldDef`, without the `default` — the GUI is only ever shown the
/// resolved current value via `PluginInfo::config_values`/`secret_set`).
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoolingState {
    #[serde(default)]
    pub fan_curves: Vec<WireFanCurve>,
    #[serde(default)]
    pub preset_curves: Vec<WirePresetCurve>,
    #[serde(default)]
    pub config: CoolingConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LightingState {
    #[serde(default)]
    pub canvas: CanvasState,
    /// Active profile's saved RGB Lighting selection.
    #[serde(default)]
    pub targets: LightingTargets,
    #[serde(default)]
    pub config: RgbConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LcdState {
    #[serde(default)]
    pub engine: WireLcdEngineState,
    /// Names of saved custom LCD templates (`lcd/<name>.yaml`), sorted.
    #[serde(default)]
    pub templates: Vec<String>,
    #[serde(default)]
    pub config: LcdConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryPhase {
    #[default]
    Discovering,
    Idle,
    Complete,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryStatus {
    #[serde(default)]
    pub phase: DiscoveryPhase,
    /// Free-form, human-readable description of the current discovery step
    /// (e.g. which transport is being scanned), pushed by the daemon so the UI
    /// can show live progress. Empty when idle or complete.
    #[serde(default)]
    pub detail: String,
    /// True while a background plugin-update check is in flight, so the radar
    /// can show a "checking for updates" step. Independent of `phase`: it may
    /// still be true after device discovery completes.
    #[serde(default)]
    pub checking_updates: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    #[default]
    Other,
    Fan,
    Hub,
    Dongle,
    Keyboard,
    Mouse,
    Headset,
    Monitor,
    Gpu,
    LedStrip,
    Motherboard,
    Ram,
    Sensor,
    AIO,
    Speaker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Wired,
    Wireless,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VisibilityState {
    #[default]
    Visible,
    Hidden,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireDevice {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub device_type: DeviceType,
    pub connected: bool,
    pub capabilities: Vec<DeviceCapability>,
    #[serde(default)]
    pub active_state: VisibilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<ConnectionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    /// Byte-movement transport the device is reachable over (`hid`, `smbus`,
    /// `usb`, …). `None` for devices whose transport is unknown/internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Declared write-rate ceiling and live throughput, enforced at the
    /// transport layer. `None` when the device hasn't wired up live stats
    /// (e.g. a chain accessory sharing its parent hub's transport, or a
    /// transport that doesn't enforce a ceiling yet, like SMBus) — the GUI
    /// hides the row rather than showing a misleading zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_rate: Option<WriteRateStatus>,
    /// Responsive grid layout for the generic Controls tab's category cards.
    /// Empty ⇒ every category gets its own full-width row, alphabetically —
    /// today's behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_layout: Vec<CategoryLayout>,
    /// Set to the owning plugin id when this device *is* an integration's
    /// root (e.g. the OpenRGB SDK client) rather than a real device — the GUI
    /// hides it from Home/sidebar and shows it on the Integrations page
    /// instead. `None` for every other device, including the devices an
    /// integration exposes as children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
}

fn span_default() -> u8 {
    1
}

/// Grid placement for one Controls-tab category card. `column`/`span` are in
/// grid columns (0-based start, width in columns); `order` breaks ties for
/// row placement across categories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CategoryLayout {
    /// Matches `Choice`/`Range`/`Boolean`/`Action` `category` (`""` ⇒ `"Settings"`).
    pub category: String,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub column: u8,
    #[serde(default = "span_default")]
    pub span: u8,
}

/// Where a receiver's pairing process currently stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    #[default]
    Idle,
    /// The pairing lock is open and the receiver is listening for a new device.
    Listening,
    /// A device was paired during the most recent listening window.
    Paired,
    /// The most recent pairing attempt ended in an error (see `error`).
    Error,
}

/// One device occupying a receiver pairing slot.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingSlot {
    pub slot: u8,
    pub device_id: String,
    pub name: String,
    pub connected: bool,
}

/// A receiver's pairing capability: current state plus the occupied slots that
/// can be unpaired.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingStatus {
    pub state: PairingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub max_slots: u8,
    pub slots: Vec<PairingSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DeviceCapability {
    Children(Vec<WireDevice>),
    Pairing(PairingStatus),
    Choice(Vec<Choice>),
    Range(Vec<Range>),
    Boolean(Vec<Boolean>),
    Action(Vec<Action>),
    Battery(Vec<Battery>),
    Connection(ConnectionStatus),
    Equalizer(Equalizer),
    Sensors(Vec<Sensor>),
    Fan(FanStatus),
    Pump(PumpStatus),
    Rgb(RgbStatus),
    Dpi(DpiStatus),
    OnboardProfiles(OnboardProfiles),
    Lcd(LcdStatus),
    KeyRemap(KeyRemapStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenShape {
    Circle,
    Square,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdDescriptor {
    pub shape: ScreenShape,
    pub width: u32,
    pub height: u32,
    pub supported_rotations: Vec<crate::commands::ScreenRotation>,
    pub supported_image_types: Vec<String>,
    #[serde(default)]
    pub latches_last_frame: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LcdMode {
    #[default]
    Default,
    Image,
    Gif,
    Engine,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdStatus {
    pub descriptor: LcdDescriptor,
    pub brightness: u8,
    pub rotation: ScreenRotation,
    pub mode: LcdMode,
    /// Filename only (relative to lcd_images_dir). None when in default/built-in mode.
    pub active_image: Option<String>,
    #[serde(default)]
    pub video_path: Option<String>,
    #[serde(default)]
    pub raw_streaming: bool,
}

/// Which layer the device's DPI is currently managed by.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DpiMode {
    /// DPI steps are stored in the device's active onboard profile (flash).
    Onboard,
    /// DPI is managed by the host (software step list, host mode).
    Host,
}

/// Unified DPI state for a pointing device. In `Onboard` mode the steps come
/// from the device's active onboard profile; in `Host` mode they come from the
/// software-managed step list.
///
/// # Invariant
/// `current_index < steps.len()` should hold, but it is not structurally
/// enforced. Always use `steps.get(current_index)` rather than direct indexing
/// to avoid a panic on a malformed broadcast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpiStatus {
    /// Ordered DPI steps (onboard profile slots, or the software step list).
    pub steps: Vec<u16>,
    /// Index of the currently active step within `steps`.
    /// See the struct-level invariant note before indexing directly.
    pub current_index: usize,
    /// Currently active DPI value reported by / applied to the device.
    pub current_dpi: u16,
    /// Full list of DPI values the hardware accepts (used to validate edits).
    pub available_dpis: Vec<u16>,
    /// Whether DPI is currently managed onboard or by the host.
    pub mode: DpiMode,
}

/// One onboard-profile slot stored in the device's flash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfileSlot {
    /// 1-based slot index (matches the writable RAM sector address).
    pub index: u8,
    /// Whether the slot is enabled in the device's profile directory.
    pub enabled: bool,
    /// Whether this slot is the device's currently active profile.
    pub active: bool,
    /// Whether the slot is backed by a factory ROM profile (restorable).
    pub has_rom_default: bool,
}

/// Onboard (on-device) profile management for HID++ mice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfiles {
    /// 1-based index of the active slot; 0 when no profile is active (host mode).
    pub active_slot: u8,
    pub slots: Vec<OnboardProfileSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub type LedId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedPosition {
    pub id: LedId,
    pub x: f32,
    pub y: f32,
}

/// Tells the frontend how to render the zone widget.
///
/// This is a UI rendering hint embedded in `ZoneTopology::Keyboard`. It is
/// unrelated to [`StandardLayout`](crate::keyboard::StandardLayout) which
/// drives key-grid geometry for key remapping. A new keyboard device must
/// update both if the form factor is not already covered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardFormFactor {
    FullSize,
    TKL,
    Compact75,
    Compact65,
    Compact60,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardLayout {
    CH,
    IT,
    US,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ZoneTopology {
    Ring,
    Rings {
        count: u8,
    },
    Linear,
    Grid,
    Keyboard {
        form_factor: KeyboardFormFactor,
        layout: KeyboardLayout,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbZone {
    pub id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub leds: Vec<LedPosition>,
}

/// Widget type for one effect parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    Range {
        min: f64,
        max: f64,
        step: f64,
    },
    /// Free numeric entry rendered as an input field rather than a slider;
    /// `min`/`max` only clamp. Value is an `EffectParamValue::Float`.
    Number {
        min: f64,
        max: f64,
    },
    Enum {
        options: Vec<String>,
    },
    Color,
    Boolean,
    /// Free-text entry. Value is an `EffectParamValue::Str`.
    Text,
    /// Sensor picker. Value is an `EffectParamValue::Str` holding the sensor id.
    Sensor,
    /// Editable threshold→color list (add/remove rows). Value is an
    /// `EffectParamValue::Steps`.
    Steps,
    /// Image/GIF picker backed by the LCD image library. Value is an
    /// `EffectParamValue::Str` holding the filename (relative to the LCD images
    /// dir); the GUI offers upload + selection from the existing library.
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectParamDescriptor {
    pub id: String,
    pub label: String,
    pub kind: ParamKind,
    pub default: EffectParamValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeEffect {
    pub id: String,
    pub name: String,
    pub params: Vec<EffectParamDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Animation {
    pub id: String,
    pub name: String,
    pub params: Vec<EffectParamDescriptor>,
}

/// One threshold → color entry of a `ParamKind::Steps` list.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorStep {
    pub value: f64,
    pub color: RgbColor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectParamValue {
    Float(f64),
    Str(String),
    Color(RgbColor),
    Bool(bool),
    /// Serializes as a JSON array, so it stays unambiguous under `untagged`.
    Steps(Vec<ColorStep>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RgbState {
    Static {
        color: RgbColor,
    },
    // LED IDs use String keys: JSON object keys are always strings.
    PerLed {
        zones: HashMap<String, HashMap<String, RgbColor>>,
    },
    NativeEffect {
        id: String,
        params: HashMap<String, EffectParamValue>,
    },
    DirectEffect {
        id: String,
        params: HashMap<String, EffectParamValue>,
    },
    /// Device is under the RGB canvas engine; the frontend disables other
    /// controls and does not read or write state in this mode.
    Engine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectDef {
    pub effect_id: String,
    /// User-facing label; `None` falls back to the instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, EffectParamValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbDescriptor {
    pub zones: Vec<RgbZone>,
    pub native_effects: Vec<NativeEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbStatus {
    pub descriptor: RgbDescriptor,
    pub state: Option<RgbState>,
    /// Zones absent from the map use the identity transform.
    #[serde(default)]
    pub zone_transforms: HashMap<String, ZoneContentTransform>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chainable_channels: Vec<ChainableChannelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainableChannelInfo {
    pub channel_id: String,
    pub name: String,
    pub max_leds: u32,
    pub links: Vec<ChainLinkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLinkInfo {
    pub child_device_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    /// Hardware-detected links cannot be removed, reordered, or renamed
    /// through chain IPC commands; only the hardware controls them.
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceDisplay {
    /// Dropdown list.
    List,
    /// All options shown as inline toggle buttons (default).
    #[default]
    Inline,
    /// Two-option choice rendered as an on/off switch (index 0 = off, 1 = on).
    Toggle,
}

/// Makes a control conditionally visible depending on a sibling control's
/// current value in the same device. Sibling numeric value = a Choice's
/// `selected as i64`, a Range's `value`, or a Boolean's `0`/`1`. When the
/// sibling can't be found, the control is shown (fail-open).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VisibleWhen {
    pub key: String,
    pub equals: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub label: String,
    pub options: Vec<ChoiceOption>,
    pub selected: usize,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub display: ChoiceDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

/// Widget used to render a `Range` control.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RangeDisplay {
    /// Draggable slider (default).
    #[default]
    Slider,
    /// A `− value +` numeric stepper.
    Stepper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub key: String,
    pub label: String,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub value: i32,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub start_label: Option<String>,
    #[serde(default)]
    pub end_label: Option<String>,
    #[serde(default)]
    pub display: RangeDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boolean {
    pub key: String,
    pub value: bool,
    // `label`/`read_only`/`category` default so a plugin's `get_booleans`
    // callback can return just `{ key, value }`; the device layer backfills the
    // rest from the manifest's `BooleanDef`.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    Charging,
    #[default]
    Discharging,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Battery {
    pub key: String,
    pub label: String,
    pub level: u8,
    pub status: BatteryStatus,
}

/// Wired/wireless link state for a wireless-capable device. Present as a
/// capability only when the device can operate over a wireless link, so the GUI
/// shows a link indicator; wired-only devices omit it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionStatus {
    pub connection_type: ConnectionType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBand {
    pub index: usize,
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EqPreset {
    pub id: String,
    pub label: String,
    /// The single editable blank curve; rendered first and separated from the rest.
    #[serde(default)]
    pub is_custom: bool,
    /// Selecting a firmware preset switches the device (writes a device byte); any
    /// `bands` it carries are informational only. A non-firmware ("software") preset
    /// instead pushes its `bands` via the custom path.
    #[serde(default)]
    pub is_firmware: bool,
    /// Preselected band values, or `None` for no curve.
    #[serde(default)]
    pub bands: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Equalizer {
    pub presets: Vec<EqPreset>,
    pub selected_preset: usize,
    pub bands: Vec<EqBand>,
    /// Whether the selected preset's bands are user-editable; the UI shows the band
    /// sliders only when set.
    #[serde(default)]
    pub editable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensorUnit {
    #[default]
    Celsius,
    Fahrenheit,
    Percent,
    Megahertz,
    Hours,
    Rpm,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensorType {
    #[default]
    Temperature,
    Load,
    Memory,
    Frequency,
    Uptime,
    FanSpeed,
    FanDuty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub value: f64,
    pub unit: SensorUnit,
    #[serde(default)]
    pub sensor_type: SensorType,
    #[serde(default)]
    pub visibility: VisibilityState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FanStatus {
    pub channel: u8,
    pub rpm: u32,
    pub duty: u8,
    /// Whether the fan duty can be set (false when no fan is connected on this channel).
    pub controllable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PumpStatus {
    pub rpm: u32,
    pub duty: u8,
    pub controllable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanCurveStatus {
    #[default]
    Ok,
    /// sensor_id is None, or the sensor device could not be found.
    NoSensor,
    /// Sensor value has not changed for longer than the stale threshold.
    SensorMalfunction,
    /// set_duty syscall failed (e.g. permission denied on sysfs pwm file).
    WriteError(String),
    /// Fan RPM has been 0 while curve duty is >20% for more than 10 s.
    FanStalled,
    /// The fan has a saved curve but its device is not currently present
    /// (unplugged or not yet discovered). No write was attempted.
    NoDevice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFanCurve {
    pub fan_id: String,
    pub sensor_id: Option<String>,
    /// Points as [temp_celsius, duty_percent] pairs.
    pub points: Vec<[f32; 2]>,
    pub status: FanCurveStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePresetCurve {
    pub id: String,
    pub name: String,
    pub points: Vec<[f32; 2]>,
}

// ── Key Remapper types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseBtn {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModKey {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    VolumeUp,
    VolumeDown,
    Mute,
    Play,
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleDir {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MacroAtom {
    KeyDown { key: u32 },
    KeyUp { key: u32 },
    MouseDown { btn: MouseBtn },
    MouseUp { btn: MouseBtn },
    Delay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroStep {
    pub kind: MacroAtom,
    pub delay_after_ms: u32,
}

/// Upper bounds shared by the GUI editor clamps and the daemon guards.
pub const MACRO_MAX_STEPS: usize = 512;
pub const MACRO_MAX_DELAY_MS: u32 = 60_000;
/// Aggregate ceiling on a macro's total programmed delay, so a macro under the
/// per-step and step-count limits still can't schedule input for hours.
pub const MACRO_MAX_TOTAL_MS: u64 = 600_000;

/// Action to execute when a button event fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    /// Leave button as-is (no divert — firmware handles it normally).
    #[default]
    Native,
    /// Swallow the press, do nothing.
    Disable,
    /// Emulate a mouse button click.
    MouseButton { btn: MouseBtn },
    /// Emulate scroll wheel movement.
    Scroll { axis: ScrollAxis, clicks: i32 },
    /// Emulate a keyboard key chord (key + modifiers).
    KeyChord { key: u32, modifiers: Vec<ModKey> },
    /// Send a media/volume OS key.
    MediaKey { key: MediaAction },
    /// Cycle the software DPI step list up or down (host mode only).
    DpiCycle { direction: CycleDir },
    /// Cycle the onboard profile (non-host mode only).
    ProfileCycle { direction: CycleDir },
    /// Temporarily apply a specific DPI while the button is held; restore on release.
    MomentaryDpi { dpi: u16 },
    /// Marks this button as the global Layer Shift modifier key.
    LayerShift,
    /// Execute a sequence of input steps (runs asynchronously).
    Macro { steps: Vec<MacroStep> },
    /// Launch an application by path.
    OpenApp { path: String },
    /// Spawn a shell command.
    Command { cmd: String, args: Vec<String> },
}

/// Description of a physical remappable button, read from the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonDescriptor {
    /// Control ID as reported by the device (getCidInfo).
    pub cid: u16,
    /// Human-readable label derived from the task ID (e.g. "Left Button").
    pub label: String,
    /// Whether this button can be diverted to host control.
    pub divertable: bool,
    /// Mutual-exclusion group — buttons in the same group share a hardware slot.
    pub group: u8,
}

/// Configured mapping for one physical button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonMapping {
    pub cid: u16,
    #[serde(default)]
    pub base: ButtonAction,
    #[serde(default)]
    pub shifted: ButtonAction,
}

/// Full key-remap state for one device, sent to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRemapStatus {
    /// All remappable buttons the device reports (order from getCidInfo).
    pub buttons: Vec<ButtonDescriptor>,
    /// Only entries that differ from Native in at least one action.
    pub mappings: Vec<ButtonMapping>,
    /// True for Logitech — the UI shows a "requires host mode" notice when false.
    pub requires_host_mode: bool,
    pub host_mode_active: bool,
}

// Capability accessors

macro_rules! find_cap {
    ($name:ident, $variant:ident, $ty:ty) => {
        pub fn $name(&self) -> Option<&$ty> {
            self.capabilities.iter().find_map(|c| {
                if let DeviceCapability::$variant(s) = c {
                    Some(s)
                } else {
                    None
                }
            })
        }
    };
}

impl WireDevice {
    find_cap!(battery, Battery, Vec<Battery>);
    find_cap!(fan, Fan, FanStatus);
    find_cap!(pump, Pump, PumpStatus);
    find_cap!(rgb, Rgb, RgbStatus);
    find_cap!(sensors, Sensors, Vec<Sensor>);
    find_cap!(dpi, Dpi, DpiStatus);
    find_cap!(onboard_profiles, OnboardProfiles, OnboardProfiles);
    find_cap!(key_remap, KeyRemap, KeyRemapStatus);
    find_cap!(equalizer, Equalizer, Equalizer);
    find_cap!(children, Children, Vec<WireDevice>);
    find_cap!(pairing, Pairing, PairingStatus);
}

#[cfg(test)]
mod app_rule_tests {
    use super::*;

    #[test]
    fn app_rule_serde_round_trip() {
        let rule = AppRule {
            process_names: vec!["firefox".into(), "chrome".into()],
            profile: "Web".into(),
            enabled: true,
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: AppRule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.process_names, rule.process_names);
        assert_eq!(back.profile, rule.profile);
        assert!(back.enabled);
    }

    #[test]
    fn app_state_defaults_empty_app_rules() {
        let state: AppState = serde_json::from_str("{}").unwrap();
        assert!(state.profiles.app_rules.is_empty());
    }

    #[test]
    fn app_rule_enabled_defaults_to_true() {
        let back: AppRule =
            serde_json::from_str(r#"{"process_names":["foo"],"profile":"P"}"#).unwrap();
        assert!(back.enabled);
    }

    #[test]
    fn gui_config_seen_tours_round_trip() {
        let mut cfg = GuiConfig::default();
        cfg.seen_tours.insert("page:home".into());
        cfg.seen_tours.insert("tab:cooling".into());
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen_tours, cfg.seen_tours);
    }

    #[test]
    fn gui_config_seen_tours_defaults_empty_for_old_configs() {
        let cfg: GuiConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.seen_tours.is_empty());
    }

    #[test]
    fn cooling_config_defaults() {
        let c: CoolingConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.fan_curve_enabled, DEFAULT_ENGINE_ENABLED);
        assert_eq!(c.fan_curve_tick_ms, DEFAULT_FAN_CURVE_TICK_MS);
        assert_eq!(c.fan_failsafe_duty, DEFAULT_FAN_FAILSAFE_DUTY);
    }

    #[test]
    fn rgb_config_defaults() {
        let c: RgbConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.canvas_enabled, DEFAULT_ENGINE_ENABLED);
        assert_eq!(c.canvas_fps, DEFAULT_CANVAS_FPS);
    }

    #[test]
    fn lcd_config_defaults() {
        let c: LcdConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.enabled, DEFAULT_ENGINE_ENABLED);
        assert_eq!(c.fps, DEFAULT_LCD_FPS);
    }

    #[test]
    fn gui_config_defaults() {
        let c: GuiConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.language, DEFAULT_LANGUAGE);
        assert_eq!(c.close_to_tray, DEFAULT_CLOSE_TO_TRAY);
        assert_eq!(
            c.suppress_dependency_warning,
            DEFAULT_SUPPRESS_DEPENDENCY_WARNING
        );
        assert_eq!(c.hide_window_controls, DEFAULT_HIDE_WINDOW_CONTROLS);
        assert_eq!(c.log_level, DEFAULT_LOG_LEVEL);
    }

    /// An empty AppState JSON must materialize the manual config defaults through
    /// the nested `config` fields — the easiest silent regression to introduce.
    #[test]
    fn app_state_empty_json_config_defaults() {
        let state: AppState = serde_json::from_str("{}").unwrap();
        assert!(state.gui.close_to_tray);
        assert_eq!(
            state.cooling.config.fan_failsafe_duty,
            DEFAULT_FAN_FAILSAFE_DUTY
        );
        assert_eq!(state.lighting.config.canvas_fps, DEFAULT_CANVAS_FPS);
        assert_eq!(state.lcd.config.fps, DEFAULT_LCD_FPS);
    }

    #[test]
    fn app_state_regrouped_json_round_trips() {
        let mut state = AppState::default();
        state.profiles.active = "gaming".into();
        state.profiles.available = vec!["default".into(), "gaming".into()];
        state.gui.language = "it".into();
        state.gui.seen_tours.insert("page:home".into());
        state.cooling.config.fan_failsafe_duty = 42;
        state.lighting.config.canvas_fps = 33;
        state.lcd.config.enabled = false;
        state.health.ffmpeg_available = true;
        state.plugins.rediscover_pending = true;
        let value = serde_json::to_value(&state).unwrap();
        let back: AppState = serde_json::from_value(value).unwrap();
        assert_eq!(back.profiles.active, "gaming");
        assert_eq!(back.profiles.available, state.profiles.available);
        assert_eq!(back.gui.language, "it");
        assert_eq!(back.gui.seen_tours, state.gui.seen_tours);
        assert_eq!(back.cooling.config.fan_failsafe_duty, 42);
        assert_eq!(back.lighting.config.canvas_fps, 33);
        assert!(!back.lcd.config.enabled);
        assert!(back.health.ffmpeg_available);
        assert!(back.plugins.rediscover_pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_param_value_steps_round_trips_untagged() {
        let v = EffectParamValue::Steps(vec![
            ColorStep {
                value: 40.0,
                color: RgbColor { r: 0, g: 255, b: 0 },
            },
            ColorStep {
                value: 80.0,
                color: RgbColor { r: 255, g: 0, b: 0 },
            },
        ]);
        let json = serde_json::to_string(&v).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);

        // The array encoding must not shadow the object-encoded Color variant.
        let c = EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 });
        let json = serde_json::to_string(&c).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn lcd_upload_stage_serde_roundtrip() {
        assert_eq!(
            serde_json::to_string(&LcdUploadStage::Done).unwrap(),
            "\"done\""
        );
        assert_eq!(
            serde_json::to_string(&LcdUploadStage::Failed).unwrap(),
            "\"failed\""
        );
        let p = LcdUploadProgress {
            device_id: "lcd".into(),
            stage: LcdUploadStage::Failed,
            percent: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: LcdUploadProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn connection_type_roundtrip() {
        let d = WireDevice {
            id: "x".into(),
            name: "x".into(),
            vendor: "x".into(),
            model: "x".into(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![],
            connection_type: Some(ConnectionType::Wired),
            serial_number: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"connection_type\":\"wired\""));
        let back: WireDevice = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connection_type, Some(ConnectionType::Wired));
    }

    #[test]
    fn connection_type_absent_when_none() {
        let d = WireDevice {
            id: "x".into(),
            name: "x".into(),
            vendor: "x".into(),
            model: "x".into(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![],
            connection_type: None,
            serial_number: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("connection_type"));
    }

    #[test]
    fn connection_capability_roundtrips() {
        let cap = DeviceCapability::Connection(ConnectionStatus {
            connection_type: ConnectionType::Wireless,
        });
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("\"kind\":\"connection\""));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            DeviceCapability::Connection(ConnectionStatus {
                connection_type: ConnectionType::Wireless
            })
        ));
    }

    #[test]
    fn notification_severity_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Info).unwrap(),
            "\"info\""
        );
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn notification_roundtrip() {
        let n = Notification {
            code: NotificationCode::DeviceInitFailed {
                device: "Kraken".into(),
                detail: "thing exploded".into(),
            },
            timestamp_ms: 1234,
        };
        // The code tag and its params flatten alongside timestamp_ms.
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["code"], "device_init_failed");
        assert_eq!(json["device"], "Kraken");
        assert_eq!(json["detail"], "thing exploded");
        assert_eq!(json["timestamp_ms"], 1234);
        let back: Notification = serde_json::from_value(json).unwrap();
        assert_eq!(back.code, n.code);
        assert_eq!(back.code.severity(), NotificationSeverity::Error);
        assert_eq!(back.timestamp_ms, 1234);
    }

    #[test]
    fn notification_code_roundtrip_all_variants() {
        use NotificationCode::*;
        let variants = [
            EngineStopped { detail: "e".into() },
            KeyRemapUnavailable { detail: "e".into() },
            WirelessReinitFailed { device: "d".into() },
            DeviceReconnectFailed { device: "d".into() },
            ProfileSwitched {
                profile: "p".into(),
            },
            ChainLinkRestoreFailed {
                name: "n".into(),
                detail: "e".into(),
            },
            DeviceInitFailed {
                device: "d".into(),
                detail: "e".into(),
            },
            FanStalled { fan: "f".into() },
            Generic {
                message: "m".into(),
            },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: NotificationCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    // find_cap! accessors

    #[test]
    fn find_cap_battery_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Battery(vec![Battery {
                key: "b1".into(),
                label: "Battery".into(),
                level: 80,
                status: BatteryStatus::Discharging,
            }])],
            ..Default::default()
        };
        assert!(device.battery().is_some());
    }

    #[test]
    fn find_cap_battery_non_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Fan(FanStatus::default())],
            ..Default::default()
        };
        assert!(device.battery().is_none());
    }

    #[test]
    fn find_cap_fan_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Fan(FanStatus {
                channel: 0,
                rpm: 1500,
                duty: 50,
                controllable: true,
            })],
            ..Default::default()
        };
        assert!(device.fan().is_some());
    }

    #[test]
    fn find_cap_fan_non_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Rgb(RgbStatus {
                descriptor: RgbDescriptor {
                    zones: vec![],
                    native_effects: vec![],
                },
                state: None,
                zone_transforms: HashMap::new(),
                chainable_channels: vec![],
            })],
            ..Default::default()
        };
        assert!(device.fan().is_none());
    }

    #[test]
    fn find_cap_rgb_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Rgb(RgbStatus {
                descriptor: RgbDescriptor {
                    zones: vec![],
                    native_effects: vec![],
                },
                state: None,
                zone_transforms: HashMap::new(),
                chainable_channels: vec![],
            })],
            ..Default::default()
        };
        assert!(device.rgb().is_some());
    }

    #[test]
    fn find_cap_sensors_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Sensors(vec![Sensor {
                id: "s1".into(),
                name: "CPU Temp".into(),
                value: 45.0,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: VisibilityState::Visible,
            }])],
            ..Default::default()
        };
        assert!(device.sensors().is_some());
    }

    #[test]
    fn find_cap_children_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Children(vec![WireDevice {
                id: "child-1".into(),
                name: "Child".into(),
                vendor: "V".into(),
                model: "M".into(),
                ..Default::default()
            }])],
            ..Default::default()
        };
        assert!(device.children().is_some());
    }

    #[test]
    fn find_cap_pairing_matching() {
        let device = WireDevice {
            capabilities: vec![DeviceCapability::Pairing(PairingStatus {
                state: PairingState::Idle,
                error: None,
                max_slots: 2,
                slots: vec![PairingSlot {
                    slot: 1,
                    device_id: "dev-1".into(),
                    name: "Mouse".into(),
                    connected: true,
                }],
            })],
            ..Default::default()
        };
        assert!(device.pairing().is_some());
    }

    // ── UH81: DeviceCapability tagged-enum serde round-trip ──────────────

    #[test]
    fn device_capability_rgb_round_trip() {
        let cap = DeviceCapability::Rgb(RgbStatus {
            descriptor: RgbDescriptor {
                zones: vec![],
                native_effects: vec![],
            },
            state: None,
            zone_transforms: HashMap::new(),
            chainable_channels: vec![],
        });
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains(r#""kind":"rgb""#));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DeviceCapability::Rgb(_)));
    }

    #[test]
    fn device_capability_fan_round_trip() {
        let cap = DeviceCapability::Fan(FanStatus {
            channel: 1,
            rpm: 2000,
            duty: 60,
            controllable: true,
        });
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains(r#""kind":"fan""#));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DeviceCapability::Fan(_)));
    }

    #[test]
    fn device_capability_pairing_round_trip() {
        let cap = DeviceCapability::Pairing(PairingStatus::default());
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains(r#""kind":"pairing""#));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DeviceCapability::Pairing(_)));
    }

    #[test]
    fn button_action_native_round_trip() {
        let action = ButtonAction::Native;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#"{"type":"native"}"#);
        let back: ButtonAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ButtonAction::Native);
    }

    #[test]
    fn button_action_default_serializes_as_native() {
        let action: ButtonAction = Default::default();
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#"{"type":"native"}"#);
    }

    #[test]
    fn button_action_key_chord_round_trip() {
        let action = ButtonAction::KeyChord {
            key: 42,
            modifiers: vec![ModKey::Ctrl, ModKey::Shift],
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: ButtonAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn button_action_macro_round_trip() {
        let action = ButtonAction::Macro {
            steps: vec![MacroStep {
                kind: MacroAtom::KeyDown { key: 7 },
                delay_after_ms: 50,
            }],
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: ButtonAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn button_action_dpi_cycle_round_trip() {
        let action = ButtonAction::DpiCycle {
            direction: CycleDir::Up,
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: ButtonAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn eq_preset_defaults_when_flags_absent() {
        let p: EqPreset =
            serde_json::from_value(serde_json::json!({"id": "custom", "label": "Custom"})).unwrap();
        assert!(!p.is_custom);
        assert!(!p.is_firmware);
        assert_eq!(p.bands, None);
    }

    proptest::proptest! {
        #[test]
        fn eq_preset_json_round_trips(
            id in ".{0,32}",
            label in ".{0,32}",
            is_custom: bool,
            is_firmware: bool,
            bands in proptest::option::of(
                proptest::collection::vec(-100.0f32..100.0, 0..12)
            ),
        ) {
            let p = EqPreset { id, label, is_custom, is_firmware, bands };
            let json = serde_json::to_string(&p).unwrap();
            let back: EqPreset = serde_json::from_str(&json).unwrap();
            proptest::prop_assert_eq!(back, p);
        }
    }
}

#[cfg(test)]
mod default_tests {
    use super::*;
    use serde_json::json;

    // Pin the `#[serde(default = "…")]` fallback values against mutation.

    #[test]
    fn placed_zone_defaults_when_size_and_rotation_omitted() {
        let z: PlacedZone =
            serde_json::from_str(r#"{"device_id":"d","zone_id":"z","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(z.w, 0.15, "default_zone_size");
        assert_eq!(z.h, 0.15, "default_zone_size");
        assert_eq!(z.rotation, 0.0, "default_rotation");
    }

    #[test]
    fn canvas_state_defaults_sample_radius_when_omitted() {
        let s: CanvasState =
            serde_json::from_str(r#"{"available_effects":[],"placed_zones":[]}"#).unwrap();
        assert_eq!(s.sample_radius, 3.0, "default_sample_radius");
    }

    #[test]
    fn gui_config_defaults_close_to_tray_when_omitted() {
        let c: GuiConfig = serde_json::from_str(r#"{"language":"en","log_level":"info"}"#).unwrap();
        assert!(c.close_to_tray, "default_close_to_tray");
        assert!(
            !c.suppress_dependency_warning,
            "default_suppress_dependency_warning"
        );
    }

    #[test]
    fn gui_config_defaults_plugin_downloads_to_unset_when_omitted() {
        let c: GuiConfig = serde_json::from_str(r#"{"language":"en","log_level":"info"}"#).unwrap();
        assert_eq!(c.plugin_downloads, PluginDownloadConsent::Unset);
    }

    #[test]
    fn plugin_download_consent_round_trips() {
        for v in [
            PluginDownloadConsent::Unset,
            PluginDownloadConsent::Allowed,
            PluginDownloadConsent::Denied,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: PluginDownloadConsent = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
        assert_eq!(
            serde_json::to_string(&PluginDownloadConsent::Allowed).unwrap(),
            "\"allowed\""
        );
    }

    #[test]
    fn validate_image_filename_accepts_uuid_image_names() {
        assert!(validate_image_filename("0e7d4c2a-1234-5678-9abc-def012345678.png").is_ok());
        assert!(validate_image_filename("pic_1.jpg").is_ok());
        assert!(validate_image_filename("a.gif").is_ok());
    }

    #[test]
    fn validate_image_filename_rejects_traversal_and_paths() {
        for bad in [
            "../secret.png",
            "a/b.png",
            "a\\b.png",
            "C:\\x.png",
            ".hidden.png",
            "noext",
            "x.txt",
            "name.png:stream",
            "",
        ] {
            assert!(
                validate_image_filename(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn validate_logo_dimensions_accepts_square_and_mild_rectangles() {
        assert!(validate_logo_dimensions(64, 64).is_ok());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM, MAX_PLUGIN_LOGO_DIM).is_ok());
        // Exactly the aspect bound is allowed; just past it is not.
        assert!(validate_logo_dimensions(200, 100).is_ok());
        assert!(validate_logo_dimensions(100, 200).is_ok());
        assert!(validate_logo_dimensions(201, 100).is_err());
    }

    #[test]
    fn validate_logo_dimensions_rejects_zero_oversize_and_banners() {
        assert!(validate_logo_dimensions(0, 64).is_err());
        assert!(validate_logo_dimensions(64, 0).is_err());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM + 1, 64).is_err());
        assert!(validate_logo_dimensions(64, MAX_PLUGIN_LOGO_DIM + 1).is_err());
        assert!(validate_logo_dimensions(500, 50).is_err());
    }

    #[test]
    fn sniff_ext_detects_formats() {
        assert_eq!(sniff_ext(b"GIF89aXXX"), "gif");
        assert_eq!(sniff_ext(b"GIF87aXXX"), "gif");
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0x00]), "jpg");
        assert_eq!(sniff_ext(&[0x89, 0x50, 0x4E, 0x47]), "png");
        assert_eq!(sniff_ext(b"anything"), "unknown");
    }

    #[test]
    fn battery_status_wire_form_is_snake_case() {
        // Crosses the daemon→UI IPC boundary; lock the lowercase wire form.
        assert_eq!(
            serde_json::to_value(BatteryStatus::Charging).unwrap(),
            json!("charging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Discharging).unwrap(),
            json!("discharging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Unknown).unwrap(),
            json!("unknown")
        );
        let s: BatteryStatus = serde_json::from_value(json!("charging")).unwrap();
        assert_eq!(s, BatteryStatus::Charging);
    }

    #[test]
    fn write_rate_status_defaults_to_no_limit() {
        // No device declares a ceiling unless it opts in — the wire default
        // must not imply one.
        assert_eq!(WriteRateStatus::default().limit, None);
    }

    #[test]
    fn write_rate_status_round_trips_through_json_with_limit() {
        let status = WriteRateStatus {
            limit: Some(WriteRateLimit {
                max_bytes_per_sec: 42,
            }),
            current_writes_per_sec: 12.5,
            current_bytes_per_sec: 640.0,
            rejected_total: 3,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: WriteRateStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn write_rate_status_round_trips_through_json_without_limit() {
        let status = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 0.0,
            current_bytes_per_sec: 0.0,
            rejected_total: 0,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: WriteRateStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn wire_device_defaults_write_rate_to_none_when_omitted() {
        let d: WireDevice = serde_json::from_str(
            r#"{"id":"d","name":"d","vendor":"v","model":"m","device_type":"other","connected":true,"capabilities":[]}"#,
        )
        .unwrap();
        assert_eq!(d.write_rate, None);
    }

    #[test]
    fn wire_device_defaults_control_layout_to_empty_when_omitted() {
        let d: WireDevice = serde_json::from_str(
            r#"{"id":"d","name":"d","vendor":"v","model":"m","device_type":"other","connected":true,"capabilities":[]}"#,
        )
        .unwrap();
        assert!(d.control_layout.is_empty());
    }

    #[test]
    fn choice_display_defaults_to_inline() {
        // The generic Controls tab hardcoded inline pills before it read this
        // field; the enum default must keep matching that appearance.
        assert_eq!(ChoiceDisplay::default(), ChoiceDisplay::Inline);
        let c: Choice =
            serde_json::from_str(r#"{"key":"k","label":"K","options":[],"selected":0}"#).unwrap();
        assert_eq!(c.display, ChoiceDisplay::Inline);
    }

    #[test]
    fn range_display_defaults_to_slider() {
        assert_eq!(RangeDisplay::default(), RangeDisplay::Slider);
    }

    #[test]
    fn category_layout_defaults_span_to_one_when_omitted() {
        let l: CategoryLayout = serde_json::from_str(r#"{"category":"Audio"}"#).unwrap();
        assert_eq!(l.order, 0);
        assert_eq!(l.column, 0);
        assert_eq!(l.span, 1);
    }

    #[test]
    fn category_layout_round_trips() {
        let l = CategoryLayout {
            category: "Audio".into(),
            order: 2,
            column: 1,
            span: 2,
        };
        let json = serde_json::to_string(&l).unwrap();
        let back: CategoryLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn visible_when_absent_when_none() {
        let c = Choice {
            key: "k".into(),
            label: "K".into(),
            options: vec![],
            selected: 0,
            category: String::new(),
            display: ChoiceDisplay::default(),
            visible_when: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("visible_when"));
    }

    #[test]
    fn visible_when_round_trips_when_present() {
        let r = Range {
            key: "nc_level".into(),
            label: "Transparency Level".into(),
            min: 1,
            max: 10,
            step: 1,
            value: 5,
            read_only: false,
            category: "Noise Cancelling".into(),
            start_label: None,
            end_label: None,
            display: RangeDisplay::default(),
            visible_when: Some(VisibleWhen {
                key: "nc_mode".into(),
                equals: vec![1],
            }),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Range = serde_json::from_str(&json).unwrap();
        assert_eq!(back.visible_when, r.visible_when);
    }
}

#[cfg(test)]
mod fan_curve_status_tests {
    use super::*;

    #[test]
    fn fan_curve_status_ok_round_trips() {
        let status = FanCurveStatus::Ok;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""ok""#);
        let back: FanCurveStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, FanCurveStatus::Ok);
    }

    #[test]
    fn fan_curve_status_no_sensor_round_trips() {
        let status = FanCurveStatus::NoSensor;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""no_sensor""#);
    }

    #[test]
    fn fan_curve_status_write_error_preserves_message() {
        let status = FanCurveStatus::WriteError("permission denied".into());
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("write_error"));
        assert!(json.contains("permission denied"));
        let back: FanCurveStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FanCurveStatus::WriteError(ref s) if s == "permission denied"));
    }
}

#[cfg(test)]
mod rgb_state_tests {
    use super::*;

    #[test]
    fn per_led_uses_string_keys_for_led_ids() {
        let state = RgbState::PerLed {
            zones: {
                let mut zones = std::collections::HashMap::new();
                let mut led_map = std::collections::HashMap::new();
                led_map.insert("12".to_string(), RgbColor { r: 255, g: 0, b: 0 });
                zones.insert("z0".to_string(), led_map);
                zones
            },
        };
        let json = serde_json::to_value(&state).unwrap();
        // Verify LED ID is a string key, not a number
        assert_eq!(json["zones"]["z0"]["12"]["r"], 255);
        // Round-trip
        let back: RgbState = serde_json::from_value(json).unwrap();
        assert!(matches!(back, RgbState::PerLed { .. }));
    }

    #[test]
    fn lcd_preview_lease_is_at_least_double_the_keepalive() {
        // One lost/late keepalive must never flap the preview stream.
        assert!(LCD_PREVIEW_LEASE_SECS as f64 >= 2.0 * LCD_PREVIEW_KEEPALIVE_SECS);
    }
}

#[cfg(test)]
mod dpi_status_tests {
    use super::*;

    fn make_status(steps: Vec<u16>, current_index: usize) -> DpiStatus {
        DpiStatus {
            steps,
            current_index,
            current_dpi: 800,
            available_dpis: vec![],
            mode: DpiMode::Host,
        }
    }

    #[test]
    fn get_guards_out_of_range_index() {
        // current_index beyond steps.len() must not panic — use .get()
        let s = make_status(vec![400, 800], 5);
        assert!(s.steps.get(s.current_index).is_none());
    }

    #[test]
    fn get_returns_correct_step() {
        let s = make_status(vec![400, 800, 1600], 1);
        assert_eq!(s.steps.get(s.current_index), Some(&800));
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

pub const DEFAULT_LANGUAGE: &str = "en";

/// The daemon accepts only these in `SetLanguage`; each needs a `locales/<code>/` catalog.
pub const SUPPORTED_LANGUAGES: &[&str] = &["en", "it"];

/// Falls back to `DEFAULT_LANGUAGE` when the OS locale has no matching catalog.
pub fn detect_system_language() -> &'static str {
    sys_locale::get_locale().map_or(DEFAULT_LANGUAGE, |locale| narrow_language(&locale))
}

pub(super) fn narrow_language(locale: &str) -> &'static str {
    let primary = locale
        .split(['-', '_', '.'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .find(|code| *code == primary)
        .unwrap_or(DEFAULT_LANGUAGE)
}

pub const ONBOARDING_TOUR_KEY: &str = "onboarding";

pub const MAX_HOME_WIDGETS: usize = 12;

pub const MAX_BATTERY_WIDGET_ENTRIES: usize = 3;

pub const BATTERY_REF_SEPARATOR: char = '/';

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HomeWidgetKind {
    Chart {
        sensor_id: String,
    },
    Gauge {
        sensor_id: String,
        min: f64,
        max: f64,
    },
    /// A segmented bar across `[min, max]`.
    Meter {
        sensor_id: String,
        min: f64,
        max: f64,
    },
    /// Battery references, each `"{device_id}/{battery_key}"`.
    Battery {
        batteries: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HomeWidget {
    pub id: String,
    pub kind: HomeWidgetKind,
    /// Index into the GUI's widget accent palette.
    pub color: u8,
    /// Overrides the source's own name when non-empty.
    #[serde(default)]
    pub label: String,
}

pub fn battery_ref(device_id: &str, key: &str) -> String {
    format!("{device_id}{BATTERY_REF_SEPARATOR}{key}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeWidgetsError {
    TooMany,
    DuplicateId,
    EmptyId,
    TooManyBatteries,
    EmptyRange,
}

impl std::fmt::Display for HomeWidgetsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::TooMany => "too many home widgets",
            Self::DuplicateId => "duplicate home widget id",
            Self::EmptyId => "home widget id must not be empty",
            Self::TooManyBatteries => "too many batteries in one widget",
            Self::EmptyRange => "gauge range must have min below max",
        };
        f.write_str(text)
    }
}

impl std::error::Error for HomeWidgetsError {}

pub fn validate_home_widgets(widgets: &[HomeWidget]) -> Result<(), HomeWidgetsError> {
    if widgets.len() > MAX_HOME_WIDGETS {
        return Err(HomeWidgetsError::TooMany);
    }
    let mut seen = std::collections::HashSet::new();
    for w in widgets {
        if w.id.trim().is_empty() {
            return Err(HomeWidgetsError::EmptyId);
        }
        if !seen.insert(&w.id) {
            return Err(HomeWidgetsError::DuplicateId);
        }
        match &w.kind {
            HomeWidgetKind::Gauge { min, max, .. } | HomeWidgetKind::Meter { min, max, .. }
                if min >= max =>
            {
                return Err(HomeWidgetsError::EmptyRange)
            }
            HomeWidgetKind::Battery { batteries }
                if batteries.len() > MAX_BATTERY_WIDGET_ENTRIES =>
            {
                return Err(HomeWidgetsError::TooManyBatteries)
            }
            _ => {}
        }
    }
    Ok(())
}

/// Splits from the right so a device id containing the separator survives.
pub fn split_battery_ref(reference: &str) -> Option<(&str, &str)> {
    reference.rsplit_once(BATTERY_REF_SEPARATOR)
}

/// Persisted under `config.yaml`'s `gui` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub language: String,
    pub close_to_tray: bool,
    pub suppress_dependency_warning: bool,
    /// For tiling window managers that drive those actions themselves.
    pub hide_window_controls: bool,
    /// Notify once below 20%, then again below 10% and 5% while discharging.
    pub low_battery_notifications: bool,
    /// Persistence keys (e.g. "page:home") of completed or skipped tutorials.
    pub seen_tours: BTreeSet<String>,
    pub log_level: String,
    /// In display order.
    pub home_widgets: Vec<HomeWidget>,
    /// In display order; separate from `home_widgets` so each page keeps its own sensors.
    pub cooling_widgets: Vec<HomeWidget>,
    pub check_for_updates: bool,
    /// Last version an update notification was sent for.
    pub update_notified: String,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            language: DEFAULT_LANGUAGE.to_string(),
            close_to_tray: true,
            suppress_dependency_warning: false,
            hide_window_controls: false,
            low_battery_notifications: true,
            seen_tours: BTreeSet::new(),
            log_level: "info".to_string(),
            home_widgets: Vec::new(),
            cooling_widgets: Vec::new(),
            check_for_updates: true,
            update_notified: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

/// The GUI owns all human-readable copy; `severity()` is derived from the variant, not sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum NotificationCode {
    EngineStopped {
        detail: String,
    },
    KeyRemapUnavailable {
        detail: String,
    },
    ProfileSwitched {
        profile: String,
    },
    LowBattery {
        device_id: crate::ids::DeviceId,
        device: String,
        battery: String,
        level: u8,
        threshold: u8,
    },
    DeviceInitFailed {
        device_id: crate::ids::DeviceId,
        device: String,
        detail: String,
    },
    /// `detail` carries the complete error chain for the Details modal.
    DeviceWriteFailed {
        device_id: crate::ids::DeviceId,
        device: String,
        detail: String,
    },
    FanStalled {
        fan: String,
    },
    /// Background-path Lua failure, deduplicated daemon-side so it alerts once, not every frame.
    PluginRuntimeError {
        plugin: crate::ids::PluginId,
        detail: String,
    },
    /// Deduplicated daemon-side per plugin, alerting once per failure episode.
    PluginConnectFailed {
        plugin: crate::ids::PluginId,
        detail: String,
    },
    /// `name` is the display name and never includes device paths or serials.
    PluginRecommended {
        plugin: crate::ids::PluginId,
        name: String,
    },
    PluginUpdateNeedsPermissions {
        plugin: crate::ids::PluginId,
        name: String,
    },
    UpdateAvailable {
        version: String,
    },
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
            | DeviceInitFailed { .. }
            | DeviceWriteFailed { .. }
            | Generic { .. } => NotificationSeverity::Error,
            KeyRemapUnavailable { .. }
            | FanStalled { .. }
            | PluginRuntimeError { .. }
            | PluginConnectFailed { .. } => NotificationSeverity::Warning,
            ProfileSwitched { .. }
            | LowBattery { .. }
            | PluginRecommended { .. }
            | PluginUpdateNeedsPermissions { .. }
            | UpdateAvailable { .. } => NotificationSeverity::Info,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        use NotificationCode::*;
        match self {
            DeviceWriteFailed { detail, .. }
            | PluginRuntimeError { detail, .. }
            | PluginConnectFailed { detail, .. } => Some(detail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub checking: bool,
    pub available: Option<String>,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(flatten)]
    pub code: NotificationCode,
    #[serde(rename = "showNative", default)]
    pub show_native: bool,
    pub timestamp_ms: u64,
}

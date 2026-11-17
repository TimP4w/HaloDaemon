// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// Relative to the daemon's config dir.
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
    #[serde(default)]
    pub available_widgets: Vec<LcdWidgetDescriptor>,
    #[serde(default)]
    pub available_presets: Vec<LcdPresetDescriptor>,
    /// device_id → template_id
    pub device_templates: HashMap<String, String>,
    /// device_id → (param id → value) for the active template.
    #[serde(default)]
    pub device_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdWidgetResize {
    #[default]
    Uniform,
    Box,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdWidgetUpdates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u32>,
    #[serde(default)]
    pub data: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_when: Option<LcdParamVisibility>,
    #[serde(default)]
    pub audio: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_when: Option<LcdParamVisibility>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdWidgetDescriptor {
    /// Namespaced `<plugin_id>:<widget_id>` catalog id.
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    /// `name` remains the mandatory fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_key: Option<String>,
    /// Inert plugin-owned catalogs, indexed by locale then translation key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, BTreeMap<String, String>>,
    /// Declared SVG asset, fetched through `GetLcdWidgetIcon`.
    pub icon: String,
    /// Additional SVGs this widget may access through `ctx:draw_asset`.
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
    pub resize: LcdWidgetResize,
    /// Initial horizontal scale and height/width ratio used by the editor.
    #[serde(default = "default_widget_scale")]
    pub default_scale: f32,
    #[serde(default = "default_widget_min_scale")]
    pub min_scale: f32,
    #[serde(default = "default_widget_aspect")]
    pub default_aspect: f32,
    /// Text parameter whose content should grow the widget horizontally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_width_param: Option<String>,
    #[serde(default)]
    pub param_visibility: HashMap<String, LcdParamVisibility>,
    #[serde(default)]
    pub uses_color: bool,
    #[serde(default)]
    pub uses_font: bool,
    #[serde(default = "default_true")]
    pub font_controls: bool,
    /// `None` inherits the screen font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_font: Option<String>,
    /// Host-enforced weight for fixed-style widgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_text_weight: Option<String>,
    #[serde(default)]
    pub updates: LcdWidgetUpdates,
}

impl LcdWidgetDescriptor {
    pub fn localized_name(&self, locale: &str) -> &str {
        let Some(key) = self.name_key.as_deref() else {
            return &self.name;
        };
        let locale = locale.to_ascii_lowercase();
        let base = locale.split(['-', '_']).next().unwrap_or(&locale);
        self.translations
            .get(&locale)
            .and_then(|c| c.get(key))
            .or_else(|| self.translations.get(base).and_then(|c| c.get(key)))
            .or_else(|| self.translations.get("en").and_then(|c| c.get(key)))
            .map(String::as_str)
            .unwrap_or(&self.name)
    }
}

fn default_widget_scale() -> f32 {
    1.0
}

fn default_widget_min_scale() -> f32 {
    0.6
}

fn default_widget_aspect() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdParamVisibility {
    pub param: String,
    pub equals: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPresetDescriptor {
    /// Namespaced `<plugin_id>:<preset_id>` catalog id.
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    pub file: String,
}

/// Pushed only to the uploading client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdUploadProgress {
    pub device_id: String,
    pub stage: LcdUploadStage,
    /// 0–100 within the stage; `None` when the stage has no measurable extent.
    #[serde(default)]
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdUploadStage {
    /// GIFs report per-frame percent.
    Processing,
    Applying,
    /// Terminal.
    Done,
    /// Terminal.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdEngineFrame {
    pub device_id: String,
    pub frame_id: u64,
    /// Base64-encoded PNG preview image.
    pub preview_b64: String,
}

/// Kept well under `LCD_PREVIEW_LEASE_SECS`.
pub const LCD_PREVIEW_KEEPALIVE_SECS: f64 = 1.0;

/// Daemon pauses streaming after this.
pub const LCD_PREVIEW_LEASE_SECS: u64 = 3;

/// Allowlist, not a traversal denylist: a bare `[A-Za-z0-9._-]` name with a known image
/// extension and no leading dot, ruling out separators, `..`, absolute paths and NTFS streams.
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

/// Sized so its base64 fits one IPC frame ([`crate::frames::MAX_PAYLOAD`]).
pub const MAX_LCD_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

pub fn validate_image_upload_size(len: u64) -> Result<(), String> {
    if len > MAX_LCD_IMAGE_BYTES {
        return Err(format!(
            "image is {len} bytes, over the {MAX_LCD_IMAGE_BYTES}-byte limit"
        ));
    }
    Ok(())
}

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

/// Persisted under `config.yaml`'s `lcd` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LcdConfig {
    pub enabled: bool,
    pub fps: u32,
}

impl Default for LcdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fps: 20,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LcdState {
    #[serde(default)]
    pub engine: WireLcdEngineState,
    /// `lcd/<name>.yaml`, sorted.
    #[serde(default)]
    pub templates: Vec<String>,
    #[serde(default)]
    pub config: LcdConfig,
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
    EditorPreview,
}

/// Orthogonal to `LcdMode`: whether the active content is actually rendering.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LcdHealth {
    #[default]
    Stable,
    Starting,
    Stopping,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdStatus {
    pub descriptor: LcdDescriptor,
    pub brightness: u8,
    pub rotation: ScreenRotation,
    pub mode: LcdMode,
    /// Relative to lcd_images_dir; `None` in built-in mode.
    pub active_image: Option<String>,
    #[serde(default)]
    pub video_path: Option<String>,
    #[serde(default)]
    pub raw_streaming: bool,
    #[serde(default)]
    pub health: LcdHealth,
}

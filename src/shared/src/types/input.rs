// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

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

pub const MACRO_MAX_STEPS: usize = 512;

pub const MACRO_MAX_DELAY_MS: u32 = 60_000;

/// So a macro within the per-step and step-count limits still can't schedule input for hours.
pub const MACRO_MAX_TOTAL_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    /// No divert; firmware handles it.
    #[default]
    Native,
    /// Swallow the press.
    Disable,
    /// Emulate a mouse button click.
    MouseButton { btn: MouseBtn },
    /// Emulate scroll wheel movement.
    Scroll { axis: ScrollAxis, clicks: i32 },
    /// Emulate a key chord.
    KeyChord { key: u32, modifiers: Vec<ModKey> },
    /// Media/volume OS key.
    MediaKey { key: MediaAction },
    /// Host mode only.
    DpiCycle { direction: CycleDir },
    /// Non-host mode only.
    ProfileCycle { direction: CycleDir },
    /// Restored on release.
    MomentaryDpi { dpi: u16 },
    /// Global Layer Shift modifier.
    LayerShift,
    /// Runs asynchronously.
    Macro { steps: Vec<MacroStep> },
    /// Launch an application by path.
    OpenApp { path: String },
    /// Spawn a shell command.
    Command { cmd: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonDescriptor {
    /// As reported by getCidInfo.
    pub cid: u16,
    /// Derived from the task ID.
    pub label: String,
    pub divertable: bool,
    /// Buttons in the same group share a hardware slot.
    pub group: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonMapping {
    pub cid: u16,
    #[serde(default)]
    pub base: ButtonAction,
    #[serde(default)]
    pub shifted: ButtonAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRemapStatus {
    /// Order from getCidInfo.
    pub buttons: Vec<ButtonDescriptor>,
    /// Configured mappings, including explicit Native overrides of plugin defaults.
    pub mappings: Vec<ButtonMapping>,
    pub requires_host_mode: bool,
    pub host_mode_active: bool,
}

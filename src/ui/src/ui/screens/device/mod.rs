// SPDX-License-Identifier: GPL-3.0-or-later
//! The per-device page: a capability/device-type-driven tab layout (header +
//! tab bar + per-tab content), driven entirely by live daemon state.

mod chains;
mod children;
mod controls;
mod cooling;
mod equalizer;
mod header;
pub(crate) mod info;
mod keys;
mod lcd;
mod lighting;
mod macro_editor;
mod onboard;
mod performance;

use std::collections::{HashMap, HashSet};

use halod_shared::commands::DaemonCommand;
use halod_shared::debug_info::DebugInfo;
use halod_shared::types::{AppState, ButtonAction, EffectParamValue, RgbColor, WireDevice};

use crate::domain::models::device_tabs::{realign_tab, tab_label, tabs_for, TabKind};
use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
use header::header;

/// Debounce window for coalesced slider/curve/paint commands.
const DEBOUNCE_SECS: f64 = 0.14;

/// Transient edit state for the open device page, reset when the device
/// changes. Edit buffers let in-flight edits survive the daemon's ~250 ms
/// state rebroadcast (see [`editing`]).
#[derive(Default)]
pub struct DeviceUi {
    pub id: String,
    pub tab: usize,
    /// Selected tab tracked by kind, so the selection survives the tab set
    /// changing shape (e.g. adding the first chain link makes a "Devices" tab
    /// appear and shifts every index). Mirrors the GTK page restoring by name.
    tab_kind: Option<TabKind>,
    /// Scratch values for one-off sliders (brightness, generic ranges), keyed
    /// by a caller string, so an in-flight drag isn't clobbered by the daemon's
    /// re-broadcast.
    pub scratch: HashMap<String, f32>,
    /// Debounced outbound commands. Flushed by [`flush_pending`] once the
    /// user pauses.
    pub pending: crate::domain::state::Debouncer,
    /// Whether a `GetDebugInfo` snapshot has been requested for this device.
    pub debug_requested: bool,
    /// Last time (egui seconds) the user touched an edit buffer.
    pub last_edit: f64,
    /// Whether the device-name inline edit field is open.
    pub rename_editing: bool,
    /// Buffer for the in-progress rename value.
    pub rename_val: String,
    /// Set to true when rename editing starts; cleared after requesting focus
    /// so the TextEdit receives keyboard input on the first frame.
    pub rename_just_started: bool,
    pub lighting: LightingTab,
    pub cooling: CoolingTab,
    pub perf: PerfTab,
    pub equalizer: EqualizerTab,
    pub keys: KeysTab,
    pub lcd: LcdTab,
}

/// Per-tab edit state for the Lighting tab.
#[derive(Default)]
pub struct LightingTab {
    /// Selected lighting zone id (empty = first zone).
    pub zone: String,
    /// Brush color for per-LED paint / static fill.
    pub paint_color: Option<RgbColor>,
    /// Per-zone per-LED paint buffer (zone id → led id → color).
    pub paint_buf: HashMap<String, HashMap<u32, RgbColor>>,
    pub paint_seeded: bool,
    pub confirm_leave_canvas: Option<DaemonCommand>,
    /// Live Sensor/Enum effect param values, keyed like `DeviceUi::scratch`.
    pub param_strs: HashMap<String, String>,
    /// Live non-master Color effect param values (any `Color` param id other
    /// than `"color"`, which is bound to the shared color picker instead).
    pub param_colors: HashMap<String, RgbColor>,
    /// Live Steps effect param values, keyed like `param_strs`.
    pub param_steps: HashMap<String, Vec<halod_shared::types::ColorStep>>,
}

/// Per-tab edit state for the Cooling tab.
#[derive(Default)]
pub struct CoolingTab {
    /// Fan/pump curve point buffer + the sensor it is bound to.
    pub curve: Vec<[f32; 2]>,
    pub curve_seeded: bool,
    pub curve_sensor: Option<String>,
}

/// Per-tab edit state for the Performance tab.
#[derive(Default)]
pub struct PerfTab {
    /// DPI stage buffer + active stage index.
    pub dpi: Vec<u32>,
    pub dpi_seeded: bool,
    pub dpi_active: usize,
}

/// Per-tab edit state for the Equalizer tab.
#[derive(Default)]
pub struct EqualizerTab {
    /// Equalizer band buffer.
    pub eq: Vec<f32>,
    pub eq_seeded: bool,
    /// Preset the buffer was last seeded from; re-seed when the selection changes.
    pub eq_preset: usize,
}

/// Per-tab edit state for the Keys/Buttons tab.
#[derive(Default)]
pub struct KeysTab {
    /// Selected button CID for the Keys/Buttons tab (None = auto-select first).
    pub keys_sel_cid: Option<u16>,
    /// In-progress base-layer action for the selected button; re-seeded from
    /// daemon state when outside the edit window (mirrors the LiveGuard
    /// pattern).
    pub keys_action: Option<ButtonAction>,
    /// In-progress layer-shift action for the selected button, same seeding
    /// rules as `keys_action`.
    pub keys_shifted_action: Option<ButtonAction>,
    /// Active macro recording (at most one across cid/layer); None = idle.
    pub macro_rec: Option<macro_editor::RecState>,
    /// In-flight macro pill reorder drag.
    pub macro_drag: Option<macro_editor::DragState>,
    /// In-flight delay-pill resize drag (`from` = step index).
    pub macro_resize: Option<macro_editor::DragState>,
    /// Armed "press a key…" capture for the palette Key ↓ / Key ↑ tiles.
    pub macro_capture: Option<macro_editor::CaptureState>,
    /// Palette mouse tile with its button-picker row open (`up` = Mouse ↑).
    pub macro_mouse_menu: Option<macro_editor::CaptureState>,
    /// Native file picker for an Open-app action, running on a background
    /// thread (blocking the UI thread trips the desktop "not responding"
    /// prompt; see the LCD picker). Tagged with the target button CID and
    /// whether it applies to the shift layer.
    pub app_picker: Option<(
        u16,
        bool,
        std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
    )>,
}

/// Which panel is shown in the Media card.
#[derive(Default, Clone, Copy, PartialEq)]
pub enum LcdMediaTab {
    #[default]
    Images,
    Video,
    Template,
}

/// One decoded GIF frame: `(rgba_bytes, width, height, delay_ms)`.
pub type GifFrame = (Vec<u8>, usize, usize, u32);

/// Animated-GIF frames as textures, for the editor's Image-widget preview.
pub struct GifTex {
    pub frames: Vec<egui::TextureHandle>,
    pub delays: Vec<f64>,
    pub total_ms: f64,
}

/// What a completed native file pick applies to.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PickerTarget {
    Image,
    Video,
}

/// Per-tab edit state for the LCD tab.
#[derive(Default)]
pub struct LcdTab {
    // ── Library ───────────────────────────────────────────────────────────
    /// Loaded thumbnail textures keyed by filename.
    pub image_cache: HashMap<String, egui::TextureHandle>,
    /// Filenames whose thumbnail decode has been attempted, so the one-per-frame
    /// grid loop doesn't re-read a file that failed to decode. Cleared on delete.
    pub requested: HashSet<String>,
    /// True after the initial `ListLcdImages` request has been sent.
    pub list_requested: bool,
    /// Library size when an upload was sent; the upload spinner clears once the
    /// refreshed library grows past it.
    pub upload_base: Option<usize>,
    /// Image filename pending delete confirmation, if any.
    pub confirm_delete_image: Option<String>,
    /// Native file picker running on a background thread; `Some` while the
    /// dialog is open. Blocking the UI thread on the dialog would stop the
    /// event loop and trip GNOME's "application is not responding" prompt.
    pub picker: Option<(
        PickerTarget,
        std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
    )>,
    // ── Tabs / templates ──────────────────────────────────────────────────
    /// Which media panel is shown.
    pub media_tab: LcdMediaTab,
    /// Profile `media_tab` was last seeded for; a profile switch re-seeds from
    /// the daemon mode (mirrors `seeded_for` in the Lighting view).
    pub seeded_profile: Option<String>,
    /// `media_tab` as of the last frame; compared to detect tab switches.
    pub prev_mode_tab: Option<LcdMediaTab>,
    /// Live param values for the selected engine template.
    pub template_params: HashMap<String, EffectParamValue>,
    // ── Preview ───────────────────────────────────────────────────────────
    /// Egui time of the last preview keepalive; 0.0 = never sent.
    pub preview_keepalive_at: f64,
    /// Cached preview texture for the display card.
    pub preview_tex: Option<egui::TextureHandle>,
    /// Key identifying what `preview_tex` holds (e.g. `"frame:42"` or
    /// `"img:foo.png"`); the texture is rebuilt whenever the key changes.
    pub preview_key: String,
    /// Filename we asked the device to display; the preview shows a spinner until
    /// the daemon confirms it as the active image.
    pub preview_pending: Option<String>,
    // ── GIF animation ─────────────────────────────────────────────────────
    /// Filename of the GIF currently being animated (empty = none).
    pub gif_source: String,
    /// Frames decoded so far. Streamed in incrementally so the first frame can
    /// paint before the whole GIF is decoded (see `spawn_gif_stream`).
    pub gif_frames: Vec<GifFrame>,
    /// Per-frame textures built once as frames arrive; looked up by index
    /// instead of re-allocated on every frame flip.
    pub gif_tex: Vec<egui::TextureHandle>,
    /// Channel delivering freshly-decoded frames off the UI thread. `Some` while
    /// a decode is in progress; dropped to `None` once the GIF is fully decoded.
    pub gif_rx: Option<std::sync::mpsc::Receiver<GifFrame>>,
    /// True once a decode has been spawned for `gif_source`, so a zero-frame
    /// decode (missing/corrupt file) doesn't respawn a thread every frame.
    pub gif_started: bool,
    /// Index of the frame currently displayed.
    pub gif_idx: usize,
    /// Egui time (seconds) at which to advance to the next GIF frame.
    /// `None` means "show the first frame as soon as it arrives".
    pub gif_advance_at: Option<f64>,
    /// Editor Image-widget GIF frames keyed by filename. `None` marks a file
    /// that isn't an animated GIF (or failed) so it isn't retried.
    pub gif_widget_tex: HashMap<String, Option<GifTex>>,
    /// Editor state for the "custom" template's stage + inspector.
    pub editor: lcd::editor::EditorState,
}

impl DeviceUi {
    /// Fresh edit state for the device `id`.
    pub fn new(id: String) -> Self {
        DeviceUi {
            id,
            ..Default::default()
        }
    }

    /// The tour applicable to the currently selected tab, if any (the LCD
    /// tab's custom-template editor gets its own tour instead of the plain
    /// LCD one while it's the active media panel).
    pub fn tour_key(&self) -> Option<crate::domain::tour::TourKey> {
        use crate::domain::tour::TourKey;
        match self.tab_kind? {
            TabKind::Devices => Some(TourKey::TabDevices),
            TabKind::Lighting => Some(TourKey::TabLighting),
            TabKind::Chains => Some(TourKey::TabChains),
            TabKind::Cooling => Some(TourKey::TabCooling),
            TabKind::Lcd if self.lcd.media_tab == LcdMediaTab::Template => Some(TourKey::LcdEditor),
            TabKind::Lcd => Some(TourKey::TabLcd),
            TabKind::Equalizer => Some(TourKey::TabEqualizer),
            TabKind::Keys => Some(TourKey::TabKeys),
            TabKind::Performance => Some(TourKey::TabPerformance),
            TabKind::Controls => Some(TourKey::TabControls),
            TabKind::Onboard => Some(TourKey::TabOnboard),
            TabKind::Pairing => Some(TourKey::TabPairing),
            TabKind::Info => None,
        }
    }

    /// The value to show for a guarded slider: the scratch value while the user
    /// is actively editing, otherwise the live daemon value.
    pub fn guarded(&self, key: &str, live: f32, time: f64) -> f32 {
        if editing(self, time) {
            self.scratch.get(key).copied().unwrap_or(live)
        } else {
            live
        }
    }

    /// Record a slider edit (scratch value + edit timestamp).
    pub fn set(&mut self, key: &str, value: f32, time: f64) {
        self.scratch.insert(key.to_string(), value);
        self.last_edit = time;
    }

    /// Queue a debounced command; the latest value per `key` wins and is sent
    /// once the user pauses for [`DEBOUNCE_SECS`].
    pub fn queue(&mut self, key: &str, cmd: DaemonCommand, time: f64) {
        self.queue_debounced(key, cmd, time, DEBOUNCE_SECS);
    }

    /// Like [`queue`](Self::queue) but with a caller-chosen quiet window. Used
    /// for edits whose side effect is expensive — onboard DPI writes to device
    /// flash, so its edits coalesce over a longer pause.
    pub fn queue_debounced(&mut self, key: &str, cmd: DaemonCommand, time: f64, debounce: f64) {
        self.last_edit = time;
        self.pending.queue(key, cmd, time, debounce);
    }
}

/// Flush any debounced commands whose quiet period has elapsed.
fn flush_pending(st: &mut DeviceUi, cmd: &CommandTx, time: f64) {
    st.pending.flush(cmd, time);
}

/// True while the user is editing; blocks daemon re-seeding.
pub fn editing(ui_state: &DeviceUi, time: f64) -> bool {
    time - ui_state.last_edit < 1.5
}

/// Shared per-tab context to keep tab signatures small.
pub struct TabCtx<'a> {
    pub state: &'a AppState,
    pub dev: &'a WireDevice,
    pub cmd: &'a CommandTx,
    pub time: f64,
    pub debug: Option<&'a DebugInfo>,
    pub lcd_images: &'a [String],
    /// Latest engine-rendered LCD preview for the open device (pre-decoded RGBA).
    pub lcd_preview: Option<crate::runtime::ipc::DecodedFrame>,
    /// Stage/percent of the in-flight LCD image upload; `None` when idle.
    pub lcd_upload: Option<halod_shared::types::LcdUploadProgress>,
    /// A just-loaded named LCD template, consumed once by the open editor.
    pub lcd_template: Option<(String, halod_shared::lcd_custom::CustomTemplateDef)>,
    /// Latest on-demand LCD editor render (per-widget sprites) for the open
    /// device; the custom-template editor composites these.
    pub lcd_editor_render: Option<crate::runtime::ipc::DecodedEditorRender>,
    /// Live per-LED colors from the latest canvas frame (lighting preview).
    pub led_colors: &'a crate::ui::screens::canvas::LedColorMap,
    /// Rolling history of the device's effective write-rate throughput
    /// (bytes/sec). `None` until the first sample lands.
    pub write_rate_history: Option<&'a std::collections::VecDeque<f32>>,
}

/// Shared empty LED-color map for contexts without live canvas frames (tests).
pub fn empty_led_colors() -> &'static crate::ui::screens::canvas::LedColorMap {
    static EMPTY: std::sync::OnceLock<crate::ui::screens::canvas::LedColorMap> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(Default::default)
}

fn pending_tab_id() -> egui::Id {
    egui::Id::new("__halod_pending_device_tab")
}

pub fn request_cooling_tab(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(pending_tab_id(), true));
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    id: &str,
    ui_state: &mut DeviceUi,
    page: &mut Page,
    time: f64,
    debug: Option<&DebugInfo>,
    lcd_images: &[String],
    lcd_preview: Option<crate::runtime::ipc::DecodedFrame>,
    lcd_upload: Option<halod_shared::types::LcdUploadProgress>,
    lcd_template: Option<(String, halod_shared::lcd_custom::CustomTemplateDef)>,
    lcd_editor_render: Option<crate::runtime::ipc::DecodedEditorRender>,
    led_colors: &crate::ui::screens::canvas::LedColorMap,
    write_rate_history: Option<&std::collections::VecDeque<f32>>,
) {
    let Some(dev) = state.devices.iter().find(|d| d.id == id) else {
        *page = Page::Home;
        return;
    };
    // A disabled device has no live hardware to control — never sit on its page.
    if dev.active_state == halod_shared::types::VisibilityState::Disabled {
        *page = Page::Home;
        return;
    }
    if ui_state.id != id {
        *ui_state = DeviceUi::new(id.to_string());
    }
    // Honour a one-shot tab request (e.g. Cooling page's "Open device").
    if ui
        .ctx()
        .data_mut(|d| d.remove_temp::<bool>(pending_tab_id()))
        .unwrap_or(false)
    {
        ui_state.tab_kind = Some(TabKind::Cooling);
    }

    let tabs = tabs_for(dev);
    ui_state.tab = realign_tab(&tabs, ui_state.tab_kind, ui_state.tab);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            crate::ui::components::page_frame(ui, |ui| {
                if crate::ui::components::back_link(ui, &t!("devtabs.back_all_devices")) {
                    *page = Page::Home;
                }
                header(ui, dev, ui_state, cmd);
                ui.add_space(8.0);

                let labels: Vec<std::borrow::Cow<'static, str>> =
                    tabs.iter().map(|t| tab_label(t.kind, dev)).collect();
                let label_refs: Vec<&str> = labels.iter().map(|c| c.as_ref()).collect();
                let tab_bar_rect =
                    crate::ui::components::tab_bar(ui, &mut ui_state.tab, &label_refs);
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::DeviceTabBar,
                    tab_bar_rect,
                );
                // Remember the selection by kind for the next frame's realign.
                ui_state.tab_kind = tabs.get(ui_state.tab).map(|t| t.kind);

                let ctx = TabCtx {
                    state,
                    dev,
                    cmd,
                    time,
                    debug,
                    lcd_images,
                    lcd_preview,
                    lcd_upload,
                    lcd_template,
                    lcd_editor_render,
                    led_colors,
                    write_rate_history,
                };
                if let Some(tab) = tabs.get(ui_state.tab) {
                    match tab.kind {
                        TabKind::Lighting => lighting::show(ui, &ctx, ui_state),
                        TabKind::Cooling => cooling::show(ui, &ctx, ui_state),
                        TabKind::Lcd => lcd::show(ui, &ctx, ui_state),
                        TabKind::Equalizer => equalizer::show(ui, &ctx, ui_state),
                        TabKind::Keys => keys::show(ui, &ctx, ui_state),
                        TabKind::Performance => performance::show(ui, &ctx, ui_state),
                        TabKind::Controls => controls::show(ui, &ctx, ui_state),
                        TabKind::Onboard => onboard::show(ui, &ctx),
                        TabKind::Pairing => children::pairing(ui, &ctx),
                        TabKind::Chains => chains::show(ui, &ctx),
                        TabKind::Devices => children::show(ui, &ctx, page),
                        TabKind::Info => info::show(ui, &ctx, ui_state),
                    }
                }

                // Flush any debounced edits whose quiet period elapsed.
                flush_pending(ui_state, cmd, time);
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{DeviceCapability, DeviceType};

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            capabilities: caps,
            ..Default::default()
        }
    }

    #[test]
    fn cooling_tab_request_is_one_shot() {
        let ctx = egui::Context::default();
        let read = |ctx: &egui::Context| {
            ctx.data_mut(|d| d.remove_temp::<bool>(pending_tab_id()))
                .unwrap_or(false)
        };
        // Nothing pending by default.
        assert!(!read(&ctx));
        // A request is observed exactly once, then consumed.
        request_cooling_tab(&ctx);
        assert!(read(&ctx));
        assert!(!read(&ctx));
    }

    #[test]
    fn editing_guard_expires_after_window() {
        let mut st = DeviceUi::new("dev".into());
        st.last_edit = 10.0;
        assert!(editing(&st, 10.0));
        assert!(editing(&st, 11.4)); // still within the 1.5 s window
        assert!(!editing(&st, 11.5)); // boundary: window elapsed
        assert!(!editing(&st, 20.0));
    }

    #[test]
    fn guarded_returns_scratch_while_editing_else_live() {
        let mut st = DeviceUi::new("dev".into());
        // Before any edit: always live.
        assert_eq!(st.guarded("b", 40.0, 0.0), 40.0);
        st.set("b", 75.0, 100.0);
        // Within the edit window the scratch value wins.
        assert_eq!(st.guarded("b", 40.0, 100.0), 75.0);
        // After the window elapses the live value wins again.
        assert_eq!(st.guarded("b", 40.0, 200.0), 40.0);
        // An unknown key falls back to live even while editing.
        assert_eq!(st.guarded("other", 40.0, 100.0), 40.0);
    }

    #[test]
    fn queue_and_flush_pending_respects_debounce() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut st = DeviceUi::new("dev".into());
        st.queue("k", DaemonCommand::GetDebugInfo, 10.0);
        // Not yet due — nothing flushed, command still pending.
        flush_pending(&mut st, &tx, 10.0 + DEBOUNCE_SECS - 0.01);
        assert!(rx.try_recv().is_err());
        assert!(st.pending.contains_key("k"));
        // Past the debounce window — command is sent and removed.
        flush_pending(&mut st, &tx, 10.0 + DEBOUNCE_SECS);
        assert!(rx.try_recv().is_ok());
        assert!(st.pending.is_empty());
    }

    #[test]
    fn queue_keeps_only_latest_value_per_key() {
        let mut st = DeviceUi::new("dev".into());
        st.queue("k", DaemonCommand::GetDebugInfo, 1.0);
        st.queue("k", DaemonCommand::ListLcdImages, 2.0);
        assert_eq!(st.pending.len(), 1);
        let (_, due) = &st.pending["k"];
        assert_eq!(*due, 2.0 + DEBOUNCE_SECS);
    }

    #[test]
    fn pairing_tab_dispatches_to_children_pairing() {
        // `show` must route TabKind::Pairing to `children::pairing`, not some
        // other tab handler. `pairing()` is the only arm that stamps a
        // `("pair_started", id)` temp value while `PairingState::Listening`,
        // so its presence after a render is proof the dispatch happened.
        use halod_shared::types::{PairingState, PairingStatus};

        let d = dev(
            DeviceType::Other,
            vec![DeviceCapability::Pairing(PairingStatus {
                state: PairingState::Listening,
                error: None,
                max_slots: 1,
                slots: vec![],
            })],
        );
        let mut state = AppState::default();
        state.devices.push(d);
        let id = state.devices[0].id.clone();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let mut ui_state = DeviceUi::new(id.clone());
        ui_state.tab_kind = Some(TabKind::Pairing);
        let mut page = Page::Device(id.clone());

        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            show(
                ui,
                &state,
                &tx,
                &id,
                &mut ui_state,
                &mut page,
                0.0,
                None,
                &[],
                None,
                None,
                None,
                None,
                empty_led_colors(),
                None,
            );
        });

        let stamped = ctx.data(|d| {
            d.get_temp::<f64>(egui::Id::new(("pair_started", id.as_str())))
                .is_some()
        });
        assert!(stamped, "children::pairing did not run for the Pairing tab");
    }
}

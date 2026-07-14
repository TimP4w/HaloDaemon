// SPDX-License-Identifier: GPL-3.0-or-later
//! Effects Canvas page — live canvas frame, draggable zone placement,
//! effect selection, and per-effect parameter editing.

mod chrome;
mod geometry;
mod modals;
mod params;
mod rack;
mod viewport;

use std::collections::{HashMap, HashSet};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use egui::Color32;
use halod_shared::types::{AppState, CanvasFrame, EffectParamValue, PlacedZone, RgbColor};

pub(crate) use chrome::chrome;
pub(crate) use modals::fps_modal;
use modals::{new_instance_modal, zones_assign_modal};
use rack::right_panel;
use viewport::canvas_view;

// ── Constants ─────────────────────────────────────────────────────────────────
/// Total right-panel width (card column + inner margins + scrollbar).
const SIDEBAR_W: f32 = 292.0;
const MIN_CANVAS_H: f32 = 320.0;
const DEBOUNCE: f64 = 0.14;
const HANDLE_R: f32 = 5.0;
const HANDLE_HIT_R: f32 = 14.0;
const MIN_ZONE: f32 = 0.03;
/// Most chips shown on an instance card before collapsing into "+N more".
const MAX_ZONE_CHIPS: usize = 7;

// ── Interaction types ─────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum Handle {
    Body,
    Corner(usize), // 0=TL  1=TR  2=BR  3=BL
    Rotation,
}

/// How LEDs and the canvas background are rendered.
#[derive(Clone, Copy, PartialEq)]
enum LedMode {
    /// Background shows the live canvas frame; LEDs drawn as white markers.
    Frame,
    /// Background is dark; each LED is drawn in its actual sampled color.
    Leds,
}

struct DragState {
    handle: Handle,
    orig: PlacedZone,
    /// Originals of every selected zone, captured at press. A `Body` drag moves
    /// them all together; resize/rotate act on `orig` alone.
    group: Vec<PlacedZone>,
    start_norm: egui::Pos2,
    /// Pointer position in screen pixels at drag start (for rotation deltas).
    press_screen: egui::Pos2,
}

/// Rubber-band (marquee) selection state — active while dragging on empty canvas.
struct MarqueeState {
    start_norm: egui::Pos2,
    cur_norm: egui::Pos2,
    /// True when a modifier was held at start → union with `base`.
    additive: bool,
    /// Selection present when the marquee began (kept for additive drags).
    base: HashSet<(String, String)>,
}

/// Rolling 1-second FPS estimate driven by per-frame `timestamp_ms` deltas.
#[derive(Default)]
struct FpsCounter {
    display: u32,
    acc_ms: f64,
    n: u32,
    last_ts_ms: Option<u64>,
}

impl FpsCounter {
    /// Record a frame at the given device timestamp; returns the refreshed
    /// FPS estimate once a full 1 s window has accumulated.
    fn tick(&mut self, timestamp_ms: u64) -> Option<u32> {
        let mut out = None;
        if let Some(last) = self.last_ts_ms {
            let dt = timestamp_ms.saturating_sub(last) as f64;
            if dt > 0.0 {
                self.acc_ms += dt;
                self.n += 1;
                if self.acc_ms >= 1000.0 {
                    self.display = ((self.n as f64) / (self.acc_ms / 1000.0)).round() as u32;
                    self.acc_ms = 0.0;
                    self.n = 0;
                    out = Some(self.display);
                }
            }
        }
        self.last_ts_ms = Some(timestamp_ms);
        out
    }
}

/// Debounced one-shot commands (zone move / effect param edit) awaiting flush.
/// `move_zones` is keyed by `zone_key` so a group move debounces every dragged
/// zone independently, latest position winning per zone.
#[derive(Default)]
struct PendingCommands {
    move_zones: HashMap<String, (halod_shared::commands::DaemonCommand, f64)>,
    effect: Option<(String, halod_shared::commands::DaemonCommand, f64)>,
    sample: Option<(halod_shared::commands::DaemonCommand, f64)>,
    fps: Option<(halod_shared::commands::DaemonCommand, f64)>,
}

impl PendingCommands {
    /// Send each pending command whose deadline has passed; keep the rest.
    fn flush(&mut self, cmd: &crate::runtime::ipc::CommandTx, time: f64) {
        let due: Vec<String> = self
            .move_zones
            .iter()
            .filter(|(_, (_, deadline))| time >= *deadline)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            if let Some((c, _)) = self.move_zones.remove(&k) {
                crate::runtime::ipc::send(cmd, c);
            }
        }
        for slot in [&mut self.sample, &mut self.fps] {
            if let Some((c, deadline)) = slot.take() {
                if time >= deadline {
                    crate::runtime::ipc::send(cmd, c);
                } else {
                    *slot = Some((c, deadline));
                }
            }
        }
        if let Some((id, c, deadline)) = self.effect.take() {
            if time >= deadline {
                crate::runtime::ipc::send(cmd, c);
            } else {
                self.effect = Some((id, c, deadline));
            }
        }
    }

    /// True when nothing is awaiting flush.
    fn is_empty(&self) -> bool {
        self.move_zones.is_empty()
            && self.effect.is_none()
            && self.sample.is_none()
            && self.fps.is_none()
    }

    /// Queue a debounced move for one zone (replacing any pending move for it).
    fn queue_move(&mut self, z: &PlacedZone, deadline: f64) {
        self.move_zones.insert(
            geometry::zone_key(&z.device_id, &z.zone_id),
            (
                halod_shared::commands::DaemonCommand::CanvasMoveZone {
                    device_id: z.device_id.clone(),
                    zone_id: z.zone_id.clone(),
                    x: z.x as f64,
                    y: z.y as f64,
                    w: Some(z.w as f64),
                    h: Some(z.h as f64),
                    rotation: Some(z.rotation as f64),
                    effect: None,
                    sampling_mode: None,
                },
                deadline,
            ),
        );
    }
}

// ── CanvasUi ──────────────────────────────────────────────────────────────────
pub struct CanvasUi {
    pub selected: HashSet<(String, String)>,
    texture: Option<egui::TextureHandle>,
    texture_frame_id: u64,
    fps: FpsCounter,
    drag: Option<DragState>,
    /// Optimistic zone positions while dragging / pending flush
    drag_zones: HashMap<String, PlacedZone>,
    pending: PendingCommands,
    param_edits: HashMap<String, HashMap<String, EffectParamValue>>,
    marquee: Option<MarqueeState>,
    /// The instance rack row currently expanded for editing.
    selected_instance: Option<String>,
    /// The instance whose name is being edited inline, plus its buffer.
    rename_instance: Option<String>,
    rename_buf: String,
    /// Set when rename editing starts; cleared after requesting focus.
    rename_just_started: bool,
    /// The instance whose assign-zones modal is open.
    zones_modal: Option<String>,
    /// Whether the "new instance" picker modal is open.
    new_instance_modal: bool,
    /// Aspect ratio (w/h) of the daemon canvas, for letterboxing the view.
    canvas_aspect: f32,
    led_mode: LedMode,
    /// Per-LED colors keyed by (device, zone); inner map is led_id → color.
    led_colors: LedColorMap,
    /// Frame id used to skip re-ingesting the same frame.
    led_frame_id: u64,
    pub(crate) fps_modal_open: bool,
    canvas_fps: f32,
    fps_edit_at: f64,
    sample_radius: f32,
    sample_edit_at: f64,
    /// Cached formatted FPS string; recomputed only when `fps.display` changes.
    fps_label: String,
    /// Target of a right-click context menu, cleared after showing.
    context_menu_target: Option<(String, String)>,
}

impl Default for CanvasUi {
    fn default() -> Self {
        Self {
            selected: HashSet::new(),
            texture: None,
            texture_frame_id: u64::MAX,
            fps: FpsCounter::default(),
            drag: None,
            drag_zones: HashMap::new(),
            pending: PendingCommands::default(),
            param_edits: HashMap::new(),
            marquee: None,
            selected_instance: None,
            rename_instance: None,
            rename_buf: String::new(),
            rename_just_started: false,
            zones_modal: None,
            new_instance_modal: false,
            canvas_aspect: 4.0 / 3.0,
            led_mode: LedMode::Frame,
            led_colors: HashMap::new(),
            led_frame_id: u64::MAX,
            fps_modal_open: false,
            canvas_fps: 20.0,
            fps_edit_at: 0.0,
            sample_radius: 3.0,
            sample_edit_at: 0.0,
            fps_label: "0 FPS".to_string(),
            context_menu_target: None,
        }
    }
}

// ── Frame ingestion ───────────────────────────────────────────────────────────

/// Latest per-LED colors keyed by (device id, zone id) → led id → color.
pub type LedColorMap = HashMap<(String, String), HashMap<u32, RgbColor>>;

impl CanvasUi {
    /// Live per-LED colors from the latest ingested canvas frame, for consumers
    /// outside the canvas page (e.g. the device lighting preview).
    pub fn led_colors(&self) -> &LedColorMap {
        &self.led_colors
    }

    /// Send any debounced canvas commands whose deadline has passed. `body`
    /// calls this every frame on the Canvas tab; the FPS modal can also be
    /// opened from the Direct tab (where `body` never runs), so that tab drains
    /// the queue through here. Returns true while commands are still pending, so
    /// the caller can keep the frame ticking until the trailing flush fires.
    pub(crate) fn flush_pending(
        &mut self,
        cmd: &crate::runtime::ipc::CommandTx,
        time: f64,
    ) -> bool {
        self.pending.flush(cmd, time);
        !self.pending.is_empty()
    }
}

/// Group flat LED entries into per-zone buckets, reusing inner allocations.
/// Entries aren't assumed pre-grouped by zone (daemon join order is non-deterministic).
fn regroup_led_colors(dst: &mut LedColorMap, entries: &[halod_shared::types::LedFrameEntry]) {
    for inner in dst.values_mut() {
        inner.clear();
    }
    for e in entries {
        dst.entry((e.device_id.clone(), e.zone_id.clone()))
            .or_default()
            .insert(e.led_id, e.color);
    }
    dst.retain(|_, inner| !inner.is_empty());
}

// Update per-LED colors without loading canvas texture. No-op when already current.
pub fn ingest_led_colors(ui: &mut CanvasUi, frame: &CanvasFrame) {
    if frame.frame_id == ui.led_frame_id {
        return;
    }
    regroup_led_colors(&mut ui.led_colors, &frame.led_colors);
    ui.led_frame_id = frame.frame_id;
}

pub fn ingest_frame(ctx: &egui::Context, ui: &mut CanvasUi, frame: &CanvasFrame) {
    if let Some(new_fps) = ui.fps.tick(frame.timestamp_ms) {
        ui.fps_label = format!("{new_fps} FPS");
    }

    ingest_led_colors(ui, frame);

    if frame.frame_id == ui.texture_frame_id {
        return;
    }
    let Ok(bytes) = B64.decode(&frame.canvas_srgb_b64) else {
        return;
    };
    let (w, h) = (frame.canvas_w as usize, frame.canvas_h as usize);
    if bytes.len() != w * h * 4 {
        return;
    }
    if w > 0 && h > 0 {
        ui.canvas_aspect = w as f32 / h as f32;
    }
    let pixels: Vec<Color32> = bytes
        .chunks_exact(4)
        .map(|c| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
        .collect();
    let image = egui::ColorImage::new([w, h], pixels);
    ui.texture = Some(ctx.load_texture("canvas_frame", image, egui::TextureOptions::LINEAR));
    ui.texture_frame_id = frame.frame_id;
}

// ── Body ──────────────────────────────────────────────────────────────────────
/// The Effects Canvas tab body: right-hand instance rack + the live canvas
/// stage. The shared RGB Lighting header (title + transport chrome + tab bar)
/// is rendered by [`crate::ui::screens::lighting::show`] above this.
#[allow(clippy::too_many_arguments)] // top-level canvas view dependencies are explicit borrows
pub(crate) fn body(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &crate::runtime::ipc::CommandTx,
    canvas_ui: &mut CanvasUi,
    canvas_frame: Option<&CanvasFrame>,
    time: f64,
    designer_ui: &mut crate::ui::screens::effect_designer::DesignerUi,
    page: &mut crate::domain::state::Page,
) {
    if let Some(frame) = canvas_frame {
        ingest_frame(ui.ctx(), canvas_ui, frame);
    }
    canvas_ui.pending.flush(cmd, time);
    prune_drag_zones(canvas_ui, state);

    if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
        let to_remove: Vec<_> = canvas_ui.selected.drain().collect();
        for (dev, zone) in to_remove {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::CanvasRemoveZone {
                    device_id: dev,
                    zone_id: zone,
                },
            );
        }
    }

    egui::Panel::right("canvas_right")
        .exact_size(SIDEBAR_W)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin {
            left: 8,
            right: 20,
            top: 26,
            bottom: 0,
        }))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    right_panel(ui, state, canvas_ui, cmd, time, designer_ui, page);
                    ui.add_space(26.0);
                });
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.inner_margin(egui::Margin {
            left: 36,
            right: 16,
            top: 0,
            bottom: 26,
        }))
        .show(ui, |ui| {
            let w = ui.available_width();
            canvas_view(ui, state, canvas_ui, cmd, time, w);
        });

    if canvas_ui.zones_modal.is_some() {
        zones_assign_modal(&ui.ctx().clone(), state, canvas_ui, cmd);
    }
    if canvas_ui.new_instance_modal {
        new_instance_modal(&ui.ctx().clone(), state, canvas_ui, cmd);
    }
}

// Remove drag overrides once the daemon confirms them.
fn prune_drag_zones(canvas_ui: &mut CanvasUi, state: &AppState) {
    if canvas_ui.drag.is_some() {
        return;
    }
    canvas_ui.drag_zones.retain(|key, ov| {
        match state
            .lighting
            .canvas
            .placed_zones
            .iter()
            .find(|z| geometry::zone_key(&z.device_id, &z.zone_id) == *key)
        {
            Some(z) => {
                let matched = (z.x - ov.x).abs() < 5e-3
                    && (z.y - ov.y).abs() < 5e-3
                    && (z.w - ov.w).abs() < 5e-3
                    && (z.h - ov.h).abs() < 5e-3
                    && (z.rotation - ov.rotation).abs() < 0.5;
                !matched
            }
            None => false,
        }
    });
}

#[cfg(test)]
mod test_fixtures {
    use super::*;
    use halod_shared::types::SamplingMode;

    pub fn r() -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(800.0, 600.0))
    }

    pub fn z(x: f32, y: f32, w: f32, h: f32) -> PlacedZone {
        PlacedZone {
            device_id: "d".into(),
            zone_id: "z".into(),
            x,
            y,
            w,
            h,
            rotation: 0.0,
            effect: None,
            sampling_mode: SamplingMode::default(),
        }
    }

    pub fn zone_with(zone_id: &str, effect: Option<&str>) -> PlacedZone {
        let mut zone = z(0.0, 0.0, 0.1, 0.1);
        zone.zone_id = zone_id.into();
        zone.effect = effect.map(str::to_string);
        zone
    }

    pub fn drag(zone: &PlacedZone, handle: Handle) -> DragState {
        DragState {
            handle,
            orig: zone.clone(),
            group: vec![zone.clone()],
            start_norm: egui::Pos2::ZERO,
            press_screen: egui::Pos2::ZERO,
        }
    }

    pub fn def_of(effect_id: &str) -> halod_shared::types::EffectDef {
        halod_shared::types::EffectDef {
            effect_id: effect_id.into(),
            name: None,
            params: HashMap::new(),
        }
    }

    pub fn led(dev: &str, zone: &str, id: u32, v: u8) -> halod_shared::types::LedFrameEntry {
        halod_shared::types::LedFrameEntry {
            device_id: dev.into(),
            zone_id: zone.into(),
            led_id: id,
            color: RgbColor { r: v, g: v, b: v },
        }
    }

    pub fn frame(id: u64, w: u32, h: u32, bytes: &[u8]) -> CanvasFrame {
        CanvasFrame {
            frame_id: id,
            timestamp_ms: 0,
            canvas_srgb_b64: B64.encode(bytes),
            canvas_w: w,
            canvas_h: h,
            led_colors: vec![],
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::test_fixtures::{frame, led};
    use super::*;

    #[test]
    fn fps_counter_accumulates_over_one_second() {
        let mut fps = FpsCounter::default();
        // First tick only seeds last_ts (no delta yet).
        assert_eq!(fps.tick(0), None);
        // 10 frames at 100 ms each = 1000 ms window → ~10 FPS.
        let mut last = None;
        for i in 1..=10 {
            last = fps.tick(i * 100);
        }
        assert_eq!(last, Some(10));
        assert_eq!(fps.display, 10);
        // Window reset after emitting.
        assert_eq!(fps.n, 0);
        assert_eq!(fps.acc_ms, 0.0);
    }

    #[test]
    fn pending_fps_flushes_only_after_its_deadline() {
        use halod_shared::commands::{DaemonCommand, EngineKind};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut pending = PendingCommands {
            fps: Some((DaemonCommand::set_engine_fps(EngineKind::Canvas, 30), 1.0)),
            ..Default::default()
        };
        // Before the deadline: nothing sent, still pending (so the caller keeps
        // repainting until it fires — the bug was the Direct tab never flushing).
        pending.flush(&tx, 0.5);
        assert!(!pending.is_empty());
        assert!(rx.try_recv().is_err());
        // At/after the deadline: the command is sent and the queue drains.
        pending.flush(&tx, 1.0);
        assert!(pending.is_empty());
        assert!(matches!(
            rx.try_recv().expect("fps command sent"),
            DaemonCommand::SetEngineConfig {
                engine: EngineKind::Canvas,
                fps: Some(30),
                ..
            }
        ));
    }

    #[test]
    fn fps_counter_ignores_nonincreasing_timestamps() {
        let mut fps = FpsCounter::default();
        fps.tick(500);
        // A stale / equal timestamp contributes nothing.
        assert_eq!(fps.tick(500), None);
        assert_eq!(fps.n, 0);
        assert_eq!(fps.acc_ms, 0.0);
    }

    #[test]
    fn regroup_buckets_by_device_zone() {
        let mut dst = HashMap::new();
        let entries = vec![
            led("d1", "z1", 0, 10),
            led("d1", "z1", 1, 20),
            led("d1", "z2", 0, 30),
            led("d2", "z1", 5, 40),
        ];
        regroup_led_colors(&mut dst, &entries);
        assert_eq!(dst.len(), 3);
        assert_eq!(dst[&("d1".into(), "z1".into())].len(), 2);
        assert_eq!(dst[&("d1".into(), "z1".into())][&1].r, 20);
        assert_eq!(dst[&("d2".into(), "z1".into())][&5].r, 40);
    }

    #[test]
    fn regroup_buckets_interleaved_zone_entries() {
        // The daemon's JoinSet yields zone tasks in non-deterministic order, so
        // entries for one zone can be split by another's; every LED must still
        // land in its own (device, zone) bucket.
        let mut dst = HashMap::new();
        let entries = vec![
            led("d1", "z1", 0, 10),
            led("d1", "z2", 0, 30),
            led("d1", "z1", 1, 20),
            led("d2", "z1", 5, 40),
            led("d1", "z2", 1, 31),
        ];
        regroup_led_colors(&mut dst, &entries);
        assert_eq!(dst.len(), 3);
        assert_eq!(dst[&("d1".into(), "z1".into())].len(), 2);
        assert_eq!(dst[&("d1".into(), "z1".into())][&0].r, 10);
        assert_eq!(dst[&("d1".into(), "z1".into())][&1].r, 20);
        assert_eq!(dst[&("d1".into(), "z2".into())].len(), 2);
        assert_eq!(dst[&("d1".into(), "z2".into())][&1].r, 31);
        assert_eq!(dst[&("d2".into(), "z1".into())][&5].r, 40);
    }

    #[test]
    fn regroup_drops_absent_zones_on_next_frame() {
        let mut dst = HashMap::new();
        regroup_led_colors(&mut dst, &[led("d1", "z1", 0, 10), led("d1", "z2", 0, 20)]);
        assert_eq!(dst.len(), 2);
        // Next frame only mentions z1 → z2 must be dropped, not stale.
        regroup_led_colors(&mut dst, &[led("d1", "z1", 0, 99)]);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[&("d1".into(), "z1".into())][&0].r, 99);
    }

    #[test]
    fn ingest_led_colors_skips_already_seen_frame() {
        let mut ui = CanvasUi::default();
        let mut f = frame(1, 0, 0, &[]);
        f.led_colors = vec![led("d1", "z1", 0, 10)];
        ingest_led_colors(&mut ui, &f);
        assert_eq!(ui.led_colors[&("d1".into(), "z1".into())][&0].r, 10);

        // Same frame_id → no re-regroup, even if the entry list differs.
        f.led_colors = vec![led("d1", "z1", 0, 99)];
        ingest_led_colors(&mut ui, &f);
        assert_eq!(ui.led_colors[&("d1".into(), "z1".into())][&0].r, 10);

        // New frame_id → colors update.
        f.frame_id = 2;
        ingest_led_colors(&mut ui, &f);
        assert_eq!(ui.led_colors[&("d1".into(), "z1".into())][&0].r, 99);
    }

    #[test]
    fn malformed_frame_leaves_texture_state_intact() {
        let ctx = egui::Context::default();
        let mut ui = CanvasUi::default();
        // A frame whose decoded length != w*h*4 must be rejected: texture
        // tracking stays at its sentinel and aspect is not corrupted.
        let bad = frame(7, 4, 4, &[0u8; 4 * 4 * 4 - 1]);
        let aspect_before = ui.canvas_aspect;
        ingest_frame(&ctx, &mut ui, &bad);
        assert_eq!(ui.texture_frame_id, u64::MAX);
        assert!((ui.canvas_aspect - aspect_before).abs() < 1e-6);
    }

    #[test]
    fn well_formed_frame_updates_texture_and_aspect() {
        let ctx = egui::Context::default();
        let mut ui = CanvasUi::default();
        let good = frame(3, 4, 2, &[0u8; 4 * 2 * 4]);
        ingest_frame(&ctx, &mut ui, &good);
        assert_eq!(ui.texture_frame_id, 3);
        assert!((ui.canvas_aspect - 2.0).abs() < 1e-6); // 4/2
    }

    #[test]
    fn fps_label_updated_only_on_window_boundary() {
        let ctx = egui::Context::default();
        let mut ui = CanvasUi::default();
        // Label starts at the sentinel; sub-second frames must not change it.
        assert_eq!(ui.fps_label, "0 FPS");
        let f0 = frame(0, 4, 2, &[0u8; 32]);
        ingest_frame(&ctx, &mut ui, &f0);
        assert_eq!(
            ui.fps_label, "0 FPS",
            "single frame should not trigger update"
        );
        // Drive 10 frames spaced 100 ms apart → 1000 ms window → 10 FPS label.
        for i in 1u64..=10 {
            let mut f = frame(i, 4, 2, &[0u8; 32]);
            f.timestamp_ms = i * 100;
            ingest_frame(&ctx, &mut ui, &f);
        }
        assert_eq!(ui.fps_label, "10 FPS");
    }
}

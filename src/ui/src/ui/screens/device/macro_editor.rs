// SPDX-License-Identifier: GPL-3.0-or-later
//! Macro designer for the Keys/Buttons tab: record key/mouse input into a
//! `ButtonAction::Macro` step sequence, shown as a strip of pills that can be
//! dragged to reorder, removed, and — for waits — resized by dragging the
//! right edge. Edits go out through the same `SetButtonMapping` flow as the
//! other per-button parameters.
//!
//! The strip edits a *normalized* form of the steps: event steps carry
//! `delay_after_ms = 0` and every pause is an explicit `Delay` step holding
//! its milliseconds. `run_macro` sleeps `delay_after_ms` after each step, so
//! this is playback-equivalent to the compact inline form and valid wire data.

use crate::ui::components as widgets;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::keycodes;
use halod_shared::types::{
    ButtonAction, ButtonMapping, MacroAtom, MacroStep, MouseBtn, MACRO_MAX_DELAY_MS,
    MACRO_MAX_STEPS,
};

use super::keys::Layer;
use super::{DeviceUi, TabCtx};
use crate::ui::theme::{self, a};

/// Smallest representable wait; also the recording gap threshold.
pub const DELAY_MIN_MS: u32 = 10;
/// Delay pill width per millisecond (1 s ≈ 80 px on top of the base width).
const PX_PER_MS: f32 = 0.08;
const DELAY_PILL_MIN_W: f32 = 64.0;
const DELAY_PILL_MAX_W: f32 = 220.0;
const CHIP_H: f32 = 28.0;
/// Trailing "×" zone width, shown on hover.
const CLOSE_W: f32 = 18.0;
/// Right-edge resize handle width on delay pills.
const RESIZE_W: f32 = 10.0;

/// Active recording session.
pub struct RecState {
    pub cid: u16,
    pub shifted: bool,
    /// egui time of the last recorded atom; gaps ≥ [`DELAY_MIN_MS`] become
    /// `Delay` steps. `None` until the first atom (no leading wait).
    pub last_event: Option<f64>,
}

/// In-flight pill drag (reorder or delay resize).
pub struct DragState {
    pub cid: u16,
    pub shifted: bool,
    pub from: usize,
}

/// Armed one-shot key capture for Insert › Key; doubles as the open
/// Insert › Mouse button-picker state.
pub struct CaptureState {
    pub cid: u16,
    pub shifted: bool,
}

// ── Pure model ───────────────────────────────────────────────────────────────

/// Editing form: non-`Delay` steps get `delay_after_ms = 0`; inline delays
/// become explicit `Delay` steps. Idempotent, preserves total wait time and
/// the event-atom sequence.
fn normalize(steps: &[MacroStep]) -> Vec<MacroStep> {
    let mut out = Vec::new();
    for s in steps {
        match s.kind {
            MacroAtom::Delay => out.push(s.clone()),
            _ => {
                out.push(MacroStep {
                    kind: s.kind.clone(),
                    delay_after_ms: 0,
                });
                if s.delay_after_ms > 0 {
                    out.push(MacroStep {
                        kind: MacroAtom::Delay,
                        delay_after_ms: s.delay_after_ms,
                    });
                }
            }
        }
    }
    out
}

/// Move the step at `from` so it lands at insertion index `to` (0..=len).
fn reorder(steps: &mut Vec<MacroStep>, from: usize, to: usize) {
    if from >= steps.len() {
        return;
    }
    let to = to.min(steps.len());
    let item = steps.remove(from);
    let to = if to > from { to - 1 } else { to };
    steps.insert(to.min(steps.len()), item);
}

/// Insertion index for a pointer over a wrapped strip of pill `rects`
/// (visual order): the first pill whose row the pointer is in (or above) and
/// whose centre is right of the pointer; `len` past the last pill.
fn drop_index(rects: &[Rect], p: Pos2) -> usize {
    for (i, r) in rects.iter().enumerate() {
        if p.y < r.min.y - 2.0 {
            return i;
        }
        if p.y <= r.max.y + 2.0 && p.x < r.center().x {
            return i;
        }
    }
    rects.len()
}

fn delay_pill_width(ms: u32) -> f32 {
    (DELAY_PILL_MIN_W + ms as f32 * PX_PER_MS).min(DELAY_PILL_MAX_W)
}

fn resize_delay(ms: u32, dx: f32) -> u32 {
    let delta = (dx / PX_PER_MS) as i64;
    (ms as i64 + delta).clamp(DELAY_MIN_MS as i64, MACRO_MAX_DELAY_MS as i64) as u32
}

fn atom_label(step: &MacroStep) -> String {
    match &step.kind {
        MacroAtom::KeyDown { key } => format!("{} ↓", keycodes::label(*key)),
        MacroAtom::KeyUp { key } => format!("{} ↑", keycodes::label(*key)),
        MacroAtom::MouseDown { btn } => format!("{btn:?} ↓"),
        MacroAtom::MouseUp { btn } => format!("{btn:?} ↑"),
        MacroAtom::Delay => format!("{} ms", step.delay_after_ms),
    }
}

/// `(fill, border, ink)` for a step chip: presses filled, releases outlined.
fn atom_style(kind: &MacroAtom) -> (Color32, Color32, Color32) {
    match kind {
        MacroAtom::KeyDown { .. } => (
            theme::SELECTED_BG,
            theme::ACCENT_BORDER,
            theme::SELECTED_TEXT,
        ),
        MacroAtom::KeyUp { .. } => (theme::INNER_BG, theme::ACCENT_BORDER, theme::ACCENT_INK),
        MacroAtom::MouseDown { .. } => (theme::OK_FILL, theme::OK_BORDER, theme::ONLINE_TEXT),
        MacroAtom::MouseUp { .. } => (theme::INNER_BG, theme::OK_BORDER, theme::ONLINE_TEXT),
        MacroAtom::Delay => (theme::RAISED_BG, theme::BORDER_INNER, theme::TEXT_DIM),
    }
}

fn mouse_btn_for(b: egui::PointerButton) -> MouseBtn {
    match b {
        egui::PointerButton::Primary => MouseBtn::Left,
        egui::PointerButton::Secondary => MouseBtn::Right,
        egui::PointerButton::Middle => MouseBtn::Middle,
        egui::PointerButton::Extra1 => MouseBtn::Back,
        egui::PointerButton::Extra2 => MouseBtn::Forward,
    }
}

/// Whether the sequence ends with keys or buttons still held. Playback
/// auto-releases them (see the daemon's `unreleased`), but it usually means a
/// recording was stopped mid-press, so the editor shows a hint.
fn has_unreleased(steps: &[MacroStep]) -> bool {
    let mut keys: Vec<u32> = Vec::new();
    let mut btns: Vec<&MouseBtn> = Vec::new();
    for s in steps {
        match &s.kind {
            MacroAtom::KeyDown { key } => keys.push(*key),
            MacroAtom::KeyUp { key } => keys.retain(|k| k != key),
            MacroAtom::MouseDown { btn } => btns.push(btn),
            MacroAtom::MouseUp { btn } => btns.retain(|b| *b != btn),
            MacroAtom::Delay => {}
        }
    }
    !keys.is_empty() || !btns.is_empty()
}

/// Append `kind`, first materializing the gap since the previous atom as a
/// `Delay` step. Silently drops atoms once the sequence is full.
fn push_atom(steps: &mut Vec<MacroStep>, last_event: &mut Option<f64>, time: f64, kind: MacroAtom) {
    if let Some(t0) = *last_event {
        let gap = ((time - t0) * 1000.0).round() as i64;
        if gap >= DELAY_MIN_MS as i64 && steps.len() < MACRO_MAX_STEPS {
            steps.push(MacroStep {
                kind: MacroAtom::Delay,
                delay_after_ms: (gap as u32).min(MACRO_MAX_DELAY_MS),
            });
        }
    }
    *last_event = Some(time);
    if steps.len() < MACRO_MAX_STEPS {
        steps.push(MacroStep {
            kind,
            delay_after_ms: 0,
        });
    }
}

/// Native scancode for a key event. Prefers the layout-independent
/// `physical_key` (winit's physical scancode) over the logical `key`, so a
/// non-US layout records the key the user physically pressed rather than the
/// modifier-dependent character it produces — e.g. on a Swiss keyboard Shift+'
/// yields logical `Questionmark` (unmapped, dropped) but physical `Quote`.
/// Falls back to the logical key when winit gives no physical mapping.
pub(super) fn key_code(key: egui::Key, physical: Option<egui::Key>) -> Option<u32> {
    physical
        .and_then(|k| keycodes::native_code(k.name()))
        .or_else(|| keycodes::native_code(key.name()))
}

/// Translate one frame's raw input into appended steps. Key repeats and keys
/// missing from the shared table are skipped; mouse buttons are recorded only
/// inside `strip` (the visible capture zone). egui-winit swallows the key
/// *press* of clipboard shortcuts and emits `Copy`/`Cut`/`Paste` instead, so
/// those synthesize the letter's key-down (its release arrives normally).
fn record_frame(
    events: &[egui::Event],
    strip: Rect,
    time: f64,
    last_event: &mut Option<f64>,
    steps: &mut Vec<MacroStep>,
) {
    let named = |name: &str| keycodes::native_code(name);
    for ev in events {
        match ev {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                repeat: false,
                ..
            } => {
                if let Some(code) = key_code(*key, *physical_key) {
                    let kind = if *pressed {
                        MacroAtom::KeyDown { key: code }
                    } else {
                        MacroAtom::KeyUp { key: code }
                    };
                    push_atom(steps, last_event, time, kind);
                }
            }
            egui::Event::Copy => {
                if let Some(code) = named("C") {
                    push_atom(steps, last_event, time, MacroAtom::KeyDown { key: code });
                }
            }
            egui::Event::Cut => {
                if let Some(code) = named("X") {
                    push_atom(steps, last_event, time, MacroAtom::KeyDown { key: code });
                }
            }
            egui::Event::Paste(_) => {
                if let Some(code) = named("V") {
                    push_atom(steps, last_event, time, MacroAtom::KeyDown { key: code });
                }
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } if strip.contains(*pos) => {
                let btn = mouse_btn_for(*button);
                let kind = if *pressed {
                    MacroAtom::MouseDown { btn }
                } else {
                    MacroAtom::MouseUp { btn }
                };
                push_atom(steps, last_event, time, kind);
            }
            _ => {}
        }
    }
}

// ── Daemon-facing state logic ────────────────────────────────────────────────

/// Write `steps` to the layer buffer and send/queue the `SetButtonMapping`.
/// Structural edits (add/remove/reorder/record-stop/clear) are immediate;
/// only the delay-resize drag is debounced.
fn commit(
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    steps: Vec<MacroStep>,
    immediate: bool,
) {
    commit_action(
        ctx,
        st,
        id,
        cid,
        layer,
        ButtonAction::Macro { steps },
        immediate,
    );
}

fn commit_action(
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    action: ButtonAction,
    immediate: bool,
) {
    let (base, shifted) = layer.pair(st, action.clone());
    layer.set(st, action);
    let cmd = DaemonCommand::Device {
        device_id: id.to_string(),
        op: DeviceOp::SetButtonMapping {
            mapping: ButtonMapping { cid, base, shifted },
        },
    };
    let DaemonCommand::Device {
        op: DeviceOp::SetButtonMapping { mapping, .. },
        ..
    } = &cmd
    else {
        unreachable!()
    };
    if !super::keys::mapping_is_complete(&mapping.base, &mapping.shifted) {
        return;
    }
    if immediate {
        st.last_edit = ctx.time;
        crate::runtime::ipc::send(ctx.cmd, cmd);
    } else {
        st.queue(&format!("btn:macro:{cid}:{}", layer.tag()), cmd, ctx.time);
    }
}

/// Stop and commit the active recording, whatever button/layer owns it.
fn stop_recording(ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    if let Some(rec) = st.keys.macro_rec.take() {
        let layer = Layer::from_shifted(rec.shifted);
        if let Some(ButtonAction::Macro { steps }) = layer.get(st) {
            commit(ctx, st, id, rec.cid, layer, steps, true);
        }
    }
}

/// Wind down macro state owned by a button that is no longer selected: a
/// recording is committed to its owning button (its buffer is still intact —
/// recording keeps the edit window open, which blocks re-seeding), everything
/// else is dropped. Called from `keys::show` before the buffers re-seed.
pub(super) fn sync_selection(ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    let sel = st.keys.keys_sel_cid;
    if st
        .keys
        .macro_rec
        .as_ref()
        .is_some_and(|r| Some(r.cid) != sel)
    {
        stop_recording(ctx, st, id);
        // Let the new selection seed immediately instead of waiting out the
        // edit window the commit just refreshed.
        st.last_edit = ctx.time - 2.0;
    }
    let stale = |cid: u16| Some(cid) != sel;
    if st.keys.macro_drag.as_ref().is_some_and(|d| stale(d.cid)) {
        st.keys.macro_drag = None;
    }
    if st.keys.macro_resize.as_ref().is_some_and(|d| stale(d.cid)) {
        st.keys.macro_resize = None;
    }
    if st.keys.macro_capture.as_ref().is_some_and(|c| stale(c.cid)) {
        st.keys.macro_capture = None;
    }
    if st
        .keys
        .macro_mouse_menu
        .as_ref()
        .is_some_and(|c| stale(c.cid))
    {
        st.keys.macro_mouse_menu = None;
    }
}

// ── Editor UI ────────────────────────────────────────────────────────────────

pub(super) fn show(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    steps: Vec<MacroStep>,
) {
    let mut steps = steps;
    let norm = normalize(&steps);
    if norm != steps {
        steps = norm;
        layer.set(
            st,
            ButtonAction::Macro {
                steps: steps.clone(),
            },
        );
    }
    let shifted = layer == Layer::Shifted;
    let mine = |c: u16, s: bool| c == cid && s == shifted;
    let recording = st
        .keys
        .macro_rec
        .as_ref()
        .is_some_and(|r| mine(r.cid, r.shifted));
    let dragging = st
        .keys
        .macro_drag
        .as_ref()
        .is_some_and(|d| mine(d.cid, d.shifted));

    header_row(ui, ctx, st, id, cid, layer, &steps, recording);
    ui.add_space(theme::SPACE_6);
    let (strip, rects, remove, resize_to) =
        timeline(ui, st, cid, shifted, &steps, recording, dragging);

    if let Some(i) = remove {
        let mut s = steps.clone();
        s.remove(i);
        commit(ctx, st, id, cid, layer, s, true);
        return;
    }
    if let Some((i, ms)) = resize_to {
        let mut s = steps.clone();
        if let Some(step) = s.get_mut(i) {
            step.delay_after_ms = ms;
            steps = s.clone();
            commit(ctx, st, id, cid, layer, s, false);
        }
    }

    // Reorder drag: ghost label at the pointer + insertion caret, reorder on
    // release (mirrors the LCD editor's library-drag pattern).
    if dragging {
        st.last_edit = ctx.time;
        let from = st.keys.macro_drag.as_ref().unwrap().from;
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            let idx = drop_index(&rects, pos);
            if let Some(caret) = caret_line(&rects, idx) {
                ui.painter()
                    .line_segment(caret, Stroke::new(2.0, theme::CYAN));
            }
            if let Some(step) = steps.get(from) {
                ui.painter().text(
                    pos + Vec2::new(12.0, -12.0),
                    Align2::LEFT_CENTER,
                    atom_label(step),
                    theme::body_md(),
                    atom_style(&step.kind).2,
                );
            }
        }
        if ui.input(|i| i.pointer.primary_released()) {
            st.keys.macro_drag = None;
            let idx = ui
                .input(|i| i.pointer.interact_pos())
                .map(|p| drop_index(&rects, p));
            if let Some(idx) = idx {
                let mut s = steps.clone();
                reorder(&mut s, from, idx);
                if s != steps {
                    commit(ctx, st, id, cid, layer, s, true);
                    return;
                }
            }
        }
    }
    if st.keys.macro_resize.is_some() {
        st.last_edit = ctx.time;
    }

    if recording {
        record_ui(ui, ctx, st, id, cid, layer, strip, &mut steps);
    }
    ui.add_space(theme::SPACE_6);
    insert_row(ui, ctx, st, id, cid, layer, &steps, recording);

    if steps.len() >= MACRO_MAX_STEPS {
        hint(ui, &t!("misc.macro_full"), theme::STAT_AMBER);
    } else if has_unreleased(&steps) {
        hint(ui, &t!("misc.macro_unreleased"), theme::STAT_AMBER);
    }
}

fn hint(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add_space(theme::SPACE_3);
    ui.label(
        egui::RichText::new(text)
            .font(theme::body_sm())
            .color(color),
    );
}

#[allow(clippy::too_many_arguments)]
fn header_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    steps: &[MacroStep],
    recording: bool,
) {
    let shifted = layer == Layer::Shifted;
    let size = Vec2::new(0.0, 30.0);
    egui::Sides::new().height(30.0).show(
        ui,
        |ui| widgets::section_label(ui, &t!("misc.macro_timeline")),
        |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_3;
            let rec_label = if recording {
                t!("misc.macro_stop")
            } else {
                t!("misc.macro_record")
            };
            if widgets::button(ui, &rec_label, widgets::ButtonKind::Danger, size).clicked() {
                stop_recording(ctx, st, id);
                if !recording {
                    st.keys.macro_capture = None;
                    st.keys.macro_mouse_menu = None;
                    st.keys.macro_rec = Some(RecState {
                        cid,
                        shifted,
                        last_event: None,
                    });
                }
            }
            if steps.is_empty() || recording {
                return;
            }
            if widgets::button(
                ui,
                &t!("misc.macro_clear"),
                widgets::ButtonKind::Ghost,
                size,
            )
            .clicked()
            {
                // Empty macros are deliberately invalid at the daemon
                // boundary. Clearing means restoring this layer's native
                // action, while preserving the other layer.
                commit_action(ctx, st, id, cid, layer, ButtonAction::Native, true);
            }
            if widgets::button(ui, &t!("misc.macro_play"), widgets::ButtonKind::Ghost, size)
                .clicked()
            {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    DaemonCommand::PlayMacro {
                        steps: steps.to_vec(),
                    },
                );
            }
        },
    );
    let msg = if recording {
        t!("misc.macro_hint_recording")
    } else if steps.is_empty() {
        t!("misc.macro_hint_empty")
    } else {
        t!("misc.macro_hint_playback")
    };
    hint(ui, &msg, theme::TEXT_MUT);
}

/// Render the step timeline; returns the strip rect (the mouse capture zone
/// while recording), each chip's rect (for drop-index math) plus any remove
/// click / delay resize performed this frame.
#[allow(clippy::type_complexity)]
fn timeline(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    cid: u16,
    shifted: bool,
    steps: &[MacroStep],
    recording: bool,
    dragging: bool,
) -> (Rect, Vec<Rect>, Option<usize>, Option<(usize, u32)>) {
    let mut rects = Vec::with_capacity(steps.len());
    let mut remove = None;
    let mut resize_to = None;
    let border = if recording {
        theme::OFFLINE
    } else {
        theme::BORDER
    };
    let frame = egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(theme::RADIUS_SM)
        .inner_margin(theme::PAD_WELL)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for (i, step) in steps.iter().enumerate() {
                    let (rect, removed, resized) = chip(ui, st, cid, shifted, i, step, recording);
                    rects.push(rect);
                    remove = remove.or(removed.then_some(i));
                    resize_to = resize_to.or(resized.map(|ms| (i, ms)));
                }
                let (caret, _) = ui.allocate_exact_size(Vec2::new(2.0, CHIP_H), Sense::hover());
                if !dragging {
                    ui.painter().rect_filled(
                        Rect::from_center_size(caret.center(), Vec2::new(2.0, 22.0)),
                        1.0,
                        theme::CYAN,
                    );
                }
                if recording && steps.is_empty() {
                    ui.label(
                        egui::RichText::new(t!("misc.macro_click_to_record"))
                            .font(theme::body_md())
                            .color(theme::TEXT_MUT),
                    );
                }
            });
        });
    (frame.response.rect, rects, remove, resize_to)
}

/// One step chip. Returns its rect, whether its × was clicked and a new delay
/// from a resize drag.
fn chip(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    cid: u16,
    shifted: bool,
    i: usize,
    step: &MacroStep,
    recording: bool,
) -> (Rect, bool, Option<u32>) {
    let is_delay = matches!(step.kind, MacroAtom::Delay);
    let (fill, border, color) = atom_style(&step.kind);
    let galley = ui
        .painter()
        .layout_no_wrap(atom_label(step), theme::body_md(), color);
    let natural = galley.size().x + 20.0;
    let w = if is_delay {
        delay_pill_width(step.delay_after_ms).max(natural + RESIZE_W)
    } else {
        natural
    };
    let sense = if recording {
        Sense::hover()
    } else {
        Sense::click_and_drag()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, CHIP_H), sense);

    let handle_w = if is_delay { RESIZE_W } else { 0.0 };
    let handle = Rect::from_min_max(Pos2::new(rect.max.x - handle_w, rect.min.y), rect.max);
    let close = Rect::from_min_max(
        Pos2::new(handle.min.x - CLOSE_W, rect.min.y),
        Pos2::new(handle.min.x, rect.max.y),
    );
    let hovered = resp.hovered() && !recording;
    let close_hovered = hovered && resp.hover_pos().is_some_and(|hp| close.contains(hp));

    let p = ui.painter();
    theme::paint_surface(p, rect, theme::RADIUS_XS + 2.0, fill, border);
    p.galley(
        Pos2::new(rect.min.x + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    if hovered {
        p.rect_filled(close.shrink(3.0), theme::RADIUS_XS, fill);
        p.text(
            close.center(),
            Align2::CENTER_CENTER,
            "×",
            theme::body_md(),
            if close_hovered { theme::TEXT } else { color },
        );
        if is_delay {
            for dx in [-4.0, -1.5] {
                p.line_segment(
                    [
                        Pos2::new(rect.max.x + dx, rect.min.y + 8.0),
                        Pos2::new(rect.max.x + dx, rect.max.y - 8.0),
                    ],
                    Stroke::new(1.0, a(color, 0.6)),
                );
            }
        }
    }
    if recording {
        return (rect, false, None);
    }

    let in_handle = |hp: Option<Pos2>| is_delay && hp.is_some_and(|hp| handle.contains(hp));
    if resp.hovered() {
        let icon = if in_handle(resp.hover_pos()) {
            egui::CursorIcon::ResizeHorizontal
        } else if close_hovered {
            egui::CursorIcon::PointingHand
        } else {
            egui::CursorIcon::Grab
        };
        ui.ctx().set_cursor_icon(icon);
    }
    let removed = resp.clicked()
        && resp
            .interact_pointer_pos()
            .is_some_and(|hp| close.contains(hp));
    if resp.drag_started() {
        let start = resp.interact_pointer_pos();
        let state = DragState {
            cid,
            shifted,
            from: i,
        };
        if in_handle(start) {
            st.keys.macro_resize = Some(state);
        } else if !start.is_some_and(|hp| close.contains(hp)) {
            st.keys.macro_drag = Some(state);
        }
    }
    let mut resized = None;
    let resizing_this = st
        .keys
        .macro_resize
        .as_ref()
        .is_some_and(|d| d.cid == cid && d.shifted == shifted && d.from == i);
    if resizing_this {
        if resp.dragged() && resp.drag_delta().x != 0.0 {
            resized = Some(resize_delay(step.delay_after_ms, resp.drag_delta().x));
        }
        if resp.drag_stopped() {
            st.keys.macro_resize = None;
        }
    }
    (rect, removed, resized)
}

/// End points of the insertion caret for `drop_index` result `idx`.
fn caret_line(rects: &[Rect], idx: usize) -> Option<[Pos2; 2]> {
    let (x, r) = if idx < rects.len() {
        (rects[idx].min.x - 3.0, &rects[idx])
    } else {
        let last = rects.last()?;
        (last.max.x + 3.0, last)
    };
    Some([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)])
}

/// Per-frame recording: hold the edit window open, keep keyboard focus (so
/// other shortcuts stay quiet) and append this frame's input to the layer
/// buffer; mouse buttons record inside the timeline strip. The commit happens
/// on Stop.
#[allow(clippy::too_many_arguments)]
fn record_ui(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    strip: Rect,
    steps: &mut Vec<MacroStep>,
) {
    st.last_edit = ctx.time;
    ui.memory_mut(|m| m.request_focus(egui::Id::unique(("macro_rec_focus", cid, layer.tag()))));
    // Swallow presses in the strip so they don't hit the chips below them.
    ui.interact(
        strip,
        egui::Id::unique(("macro_rec_zone", cid, layer.tag())),
        Sense::click_and_drag(),
    );

    let events = ui.input(|i| i.events.clone());
    let Some(mut rec) = st.keys.macro_rec.take() else {
        return;
    };
    let before = steps.len();
    record_frame(&events, strip, ctx.time, &mut rec.last_event, steps);
    st.keys.macro_rec = Some(rec);
    if steps.len() != before {
        layer.set(
            st,
            ButtonAction::Macro {
                steps: steps.clone(),
            },
        );
    }
    if !ui.input(|i| i.focused) {
        stop_recording(ctx, st, id);
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    cid: u16,
    layer: Layer,
    steps: &[MacroStep],
    recording: bool,
) {
    let shifted = layer == Layer::Shifted;
    let append = |ctx: &TabCtx, st: &mut DeviceUi, atoms: &[(MacroAtom, u32)]| {
        if steps.len() + atoms.len() > MACRO_MAX_STEPS {
            return;
        }
        let mut s = steps.to_vec();
        s.extend(atoms.iter().map(|(kind, ms)| MacroStep {
            kind: kind.clone(),
            delay_after_ms: *ms,
        }));
        commit(ctx, st, id, cid, layer, s, true);
    };

    let capturing = st
        .keys
        .macro_capture
        .as_ref()
        .is_some_and(|c| c.cid == cid && c.shifted == shifted);
    if capturing {
        st.last_edit = ctx.time;
        ui.memory_mut(|m| m.request_focus(egui::Id::unique(("macro_cap_focus", cid, layer.tag()))));
        let code = ui.input(|i| {
            i.events.iter().find_map(|ev| match ev {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    repeat: false,
                    ..
                } => key_code(*key, *physical_key),
                _ => None,
            })
        });
        if let Some(key) = code {
            st.keys.macro_capture = None;
            append(
                ctx,
                st,
                &[
                    (MacroAtom::KeyDown { key }, 0),
                    (MacroAtom::KeyUp { key }, 0),
                ],
            );
        }
    }
    let menu_open = st
        .keys
        .macro_mouse_menu
        .as_ref()
        .is_some_and(|c| c.cid == cid && c.shifted == shifted);

    let size = Vec2::new(0.0, 30.0);
    let kind = |on: bool| {
        if on {
            widgets::ButtonKind::Primary
        } else {
            widgets::ButtonKind::Secondary
        }
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        widgets::section_label_inline(ui, &t!("misc.macro_insert"));
        ui.add_space(theme::SPACE_2);
        let key_label = if capturing {
            t!("misc.macro_press_key")
        } else {
            t!("misc.macro_insert_key")
        };
        if widgets::button_enabled(ui, &key_label, kind(capturing), size, !recording) {
            st.keys.macro_mouse_menu = None;
            st.keys.macro_capture = (!capturing).then_some(CaptureState { cid, shifted });
        }
        if widgets::button_enabled(
            ui,
            &t!("misc.macro_insert_mouse"),
            kind(menu_open),
            size,
            !recording,
        ) {
            st.keys.macro_capture = None;
            st.keys.macro_mouse_menu = (!menu_open).then_some(CaptureState { cid, shifted });
        }
        if widgets::button_enabled(
            ui,
            &t!("misc.macro_insert_delay"),
            widgets::ButtonKind::Secondary,
            size,
            !recording,
        ) {
            append(ctx, st, &[(MacroAtom::Delay, 250)]);
        }
    });

    if menu_open {
        ui.add_space(theme::SPACE_3);
        let options = [
            (t!("misc.macro_mouse_left"), MouseBtn::Left),
            (t!("misc.macro_mouse_right"), MouseBtn::Right),
            (t!("misc.macro_mouse_middle"), MouseBtn::Middle),
            (t!("misc.macro_mouse_back"), MouseBtn::Back),
            (t!("misc.macro_mouse_forward"), MouseBtn::Forward),
        ];
        let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_ref()).collect();
        let picked = widgets::option_grid(ui, &labels, None, 5)
            .iter()
            .position(|r| r.clicked())
            .map(|i| options[i].1.clone());
        if let Some(btn) = picked {
            st.keys.macro_mouse_menu = None;
            append(
                ctx,
                st,
                &[
                    (MacroAtom::MouseDown { btn: btn.clone() }, 0),
                    (MacroAtom::MouseUp { btn }, 0),
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::topic_store::TopicStore;
    use egui::pos2;
    use halod_shared::types::WireDevice;
    use proptest::prelude::*;

    /// egui keys that intentionally have no entry in the shared key table:
    /// clipboard pseudo-keys, shifted aliases without their own scancode, and
    /// F-keys beyond the evdev range. A new egui key must land in the table
    /// or here, otherwise it silently wouldn't record.
    const KNOWN_UNMAPPED: &[egui::Key] = &[
        egui::Key::Copy,
        egui::Key::Cut,
        egui::Key::Paste,
        egui::Key::Colon,
        egui::Key::Plus,
        egui::Key::Pipe,
        egui::Key::Questionmark,
        egui::Key::Exclamationmark,
        egui::Key::OpenCurlyBracket,
        egui::Key::CloseCurlyBracket,
        egui::Key::F25,
        egui::Key::F26,
        egui::Key::F27,
        egui::Key::F28,
        egui::Key::F29,
        egui::Key::F30,
        egui::Key::F31,
        egui::Key::F32,
        egui::Key::F33,
        egui::Key::F34,
        egui::Key::F35,
        egui::Key::BrowserForward,
        egui::Key::BrowserRefresh,
        egui::Key::BrowserSearch,
        egui::Key::BrowserHome,
        egui::Key::BrowserFavorites,
        egui::Key::BrowserStop,
        egui::Key::MediaPlayPause,
        egui::Key::MediaTrackNext,
        egui::Key::MediaTrackPrevious,
        egui::Key::MediaStop,
        egui::Key::AudioVolumeMute,
        egui::Key::AudioVolumeDown,
        egui::Key::AudioVolumeUp,
        egui::Key::LaunchMail,
        egui::Key::LaunchApp1,
        egui::Key::LaunchApp2,
        egui::Key::CapsLock,
        egui::Key::NumLock,
        egui::Key::ScrollLock,
        egui::Key::PrintScreen,
        egui::Key::Pause,
        egui::Key::Menu,
        egui::Key::Fn,
        egui::Key::Eject,
        egui::Key::Help,
        egui::Key::Power,
        egui::Key::Sleep,
        egui::Key::Clear,
    ];

    #[test]
    fn every_egui_key_is_mapped_or_known_unmapped() {
        for &key in egui::Key::ALL {
            assert!(
                keycodes::by_name(key.name()).is_some() || KNOWN_UNMAPPED.contains(&key),
                "egui key {key:?} (name {:?}) is neither in the shared key table nor KNOWN_UNMAPPED",
                key.name()
            );
        }
        for key in KNOWN_UNMAPPED {
            assert!(
                keycodes::by_name(key.name()).is_none(),
                "{key:?} is in the table now — remove it from KNOWN_UNMAPPED"
            );
        }
    }

    fn key_step(code: u32, down: bool) -> MacroStep {
        MacroStep {
            kind: if down {
                MacroAtom::KeyDown { key: code }
            } else {
                MacroAtom::KeyUp { key: code }
            },
            delay_after_ms: 0,
        }
    }

    fn delay_step(ms: u32) -> MacroStep {
        MacroStep {
            kind: MacroAtom::Delay,
            delay_after_ms: ms,
        }
    }

    fn key_event(key: egui::Key, pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn key_event_phys(key: egui::Key, physical: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(physical),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    const STRIP: Rect = Rect {
        min: pos2(0.0, 0.0),
        max: pos2(100.0, 50.0),
    };

    #[test]
    fn record_frame_captures_press_and_release() {
        let mut steps = Vec::new();
        let mut last = None;
        let events = [
            key_event(egui::Key::A, true, false),
            key_event(egui::Key::A, false, false),
        ];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        let a = keycodes::native_code("A").unwrap();
        assert_eq!(steps, vec![key_step(a, true), key_step(a, false)]);
    }

    #[test]
    fn record_frame_skips_repeats_and_unmapped_keys() {
        let mut steps = Vec::new();
        let mut last = None;
        let events = [
            key_event(egui::Key::A, true, true),     // key-repeat
            key_event(egui::Key::Plus, true, false), // no scancode of its own
        ];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        assert!(steps.is_empty());
    }

    #[test]
    fn record_frame_materializes_gaps_as_delay_steps() {
        let mut steps = Vec::new();
        let mut last = None;
        record_frame(
            &[key_event(egui::Key::A, true, false)],
            STRIP,
            1.0,
            &mut last,
            &mut steps,
        );
        // 5 ms — below the threshold, no delay pill.
        record_frame(
            &[key_event(egui::Key::A, false, false)],
            STRIP,
            1.005,
            &mut last,
            &mut steps,
        );
        // 250 ms — becomes a Delay step.
        record_frame(
            &[key_event(egui::Key::B, true, false)],
            STRIP,
            1.255,
            &mut last,
            &mut steps,
        );
        let a = keycodes::native_code("A").unwrap();
        let b = keycodes::native_code("B").unwrap();
        assert_eq!(
            steps,
            vec![
                key_step(a, true),
                key_step(a, false),
                delay_step(250),
                key_step(b, true)
            ]
        );
    }

    #[test]
    fn record_frame_prefers_physical_key_over_logical() {
        // Swiss keyboard: the '/? key sits at the US Minus position. Its logical
        // key shifts with modifier/dead-key state (Questionmark on the press,
        // Quote on the release), but the physical key stays put, so both edges
        // must record the same scancode.
        let mut steps = Vec::new();
        let mut last = None;
        let events = [
            key_event_phys(egui::Key::Questionmark, egui::Key::Minus, true),
            key_event_phys(egui::Key::Quote, egui::Key::Minus, false),
        ];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        let minus = keycodes::native_code("Minus").unwrap();
        assert_eq!(steps, vec![key_step(minus, true), key_step(minus, false)]);
    }

    #[test]
    fn key_code_falls_back_to_logical_without_physical() {
        let quote = keycodes::native_code("Quote").unwrap();
        assert_eq!(key_code(egui::Key::Quote, None), Some(quote));
        // Unmapped logical key with no physical mapping stays unrecordable.
        assert_eq!(key_code(egui::Key::Questionmark, None), None);
    }

    #[test]
    fn record_frame_records_modifier_keys_as_plain_keys() {
        let mut steps = Vec::new();
        let mut last = None;
        let events = [
            key_event(egui::Key::ControlLeft, true, false),
            key_event(egui::Key::SuperLeft, true, false),
        ];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        let ctrl = keycodes::native_code("ControlLeft").unwrap();
        let sup = keycodes::native_code("SuperLeft").unwrap();
        assert_eq!(steps, vec![key_step(ctrl, true), key_step(sup, true)]);
    }

    #[test]
    fn record_frame_synthesizes_letter_down_for_clipboard_events() {
        // egui-winit swallows the C press of Ctrl+C and emits Copy instead;
        // the release still arrives as a normal Key event.
        let mut steps = Vec::new();
        let mut last = None;
        let events = [egui::Event::Copy, key_event(egui::Key::C, false, false)];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        let c = keycodes::native_code("C").unwrap();
        assert_eq!(steps, vec![key_step(c, true), key_step(c, false)]);
    }

    #[test]
    fn record_frame_records_pointer_buttons_only_inside_strip() {
        let mut steps = Vec::new();
        let mut last = None;
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let events = [
            button(pos2(50.0, 25.0), true),
            button(pos2(500.0, 500.0), false),
        ];
        record_frame(&events, STRIP, 1.0, &mut last, &mut steps);
        assert_eq!(
            steps,
            vec![MacroStep {
                kind: MacroAtom::MouseDown {
                    btn: MouseBtn::Right
                },
                delay_after_ms: 0,
            }]
        );
    }

    #[test]
    fn push_atom_stops_at_the_step_cap() {
        let mut steps = vec![delay_step(1); MACRO_MAX_STEPS];
        let mut last = None;
        push_atom(&mut steps, &mut last, 1.0, MacroAtom::KeyDown { key: 30 });
        assert_eq!(steps.len(), MACRO_MAX_STEPS);
    }

    #[test]
    fn mouse_btn_for_maps_all_five_buttons() {
        assert_eq!(mouse_btn_for(egui::PointerButton::Primary), MouseBtn::Left);
        assert_eq!(
            mouse_btn_for(egui::PointerButton::Secondary),
            MouseBtn::Right
        );
        assert_eq!(mouse_btn_for(egui::PointerButton::Middle), MouseBtn::Middle);
        assert_eq!(mouse_btn_for(egui::PointerButton::Extra1), MouseBtn::Back);
        assert_eq!(
            mouse_btn_for(egui::PointerButton::Extra2),
            MouseBtn::Forward
        );
    }

    #[test]
    fn has_unreleased_flags_open_press() {
        assert!(!has_unreleased(&[key_step(30, true), key_step(30, false)]));
        assert!(has_unreleased(&[key_step(30, true)]));
    }

    #[test]
    fn drop_index_over_two_wrapped_rows() {
        let r = |x0: f32, y0: f32| Rect::from_min_max(pos2(x0, y0), pos2(x0 + 40.0, y0 + 26.0));
        let rects = [r(0.0, 0.0), r(46.0, 0.0), r(0.0, 32.0), r(46.0, 32.0)];
        assert_eq!(drop_index(&rects, pos2(-10.0, -10.0)), 0); // above everything
        assert_eq!(drop_index(&rects, pos2(5.0, 13.0)), 0); // left of first
        assert_eq!(drop_index(&rects, pos2(44.0, 13.0)), 1); // between row-1 pills
        assert_eq!(drop_index(&rects, pos2(95.0, 13.0)), 2); // right of row 1 → start of row 2
        assert_eq!(drop_index(&rects, pos2(44.0, 45.0)), 3); // between row-2 pills
        assert_eq!(drop_index(&rects, pos2(95.0, 45.0)), 4); // past the last pill
        assert_eq!(drop_index(&[], pos2(0.0, 0.0)), 0);
    }

    #[test]
    fn resize_delay_clamps_to_bounds() {
        assert_eq!(resize_delay(100, -10_000.0), DELAY_MIN_MS);
        assert_eq!(resize_delay(100, 10_000_000.0), MACRO_MAX_DELAY_MS);
    }

    #[test]
    fn delay_pill_width_clamps_and_grows() {
        assert_eq!(delay_pill_width(0), DELAY_PILL_MIN_W);
        assert_eq!(delay_pill_width(u32::MAX), DELAY_PILL_MAX_W);
        assert!(delay_pill_width(500) > delay_pill_width(100));
    }

    fn test_ctx<'a>(
        state: &'a TopicStore,
        dev: &'a WireDevice,
        tx: &'a tokio::sync::mpsc::UnboundedSender<DaemonCommand>,
    ) -> TabCtx<'a> {
        TabCtx {
            state,
            dev,
            cmd: tx,
            time: 100.0,
            debug: None,
            lcd_images: &[],
            lcd_preview: None,
            lcd_upload: None,
            lcd_upload_terminal: None,
            lcd_template: None,
            lcd_editor_render: None,
            led_colors: crate::ui::screens::device::empty_led_colors(),
            write_rate_history: None,
            plugin_assets: crate::ui::screens::device::empty_plugin_assets(),
        }
    }

    #[test]
    fn commit_immediate_sends_mapping_with_other_layer_preserved() {
        let state = TopicStore::default();
        let dev = WireDevice::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = test_ctx(&state, &dev, &tx);
        let mut st = DeviceUi::new("dev1".into());
        st.keys.keys_action = Some(ButtonAction::Native);
        st.keys.keys_shifted_action = Some(ButtonAction::Disable);

        let steps = vec![key_step(30, true), key_step(30, false)];
        commit(&ctx, &mut st, "dev1", 7, Layer::Base, steps.clone(), true);

        assert_eq!(st.last_edit, 100.0);
        assert_eq!(
            Layer::Base.get(&st),
            Some(ButtonAction::Macro {
                steps: steps.clone()
            })
        );
        match rx.try_recv().unwrap() {
            DaemonCommand::Device {
                device_id: id,
                op: DeviceOp::SetButtonMapping { mapping },
            } => {
                assert_eq!(id, "dev1");
                assert_eq!(mapping.cid, 7);
                assert_eq!(mapping.base, ButtonAction::Macro { steps });
                assert_eq!(mapping.shifted, ButtonAction::Disable);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commit_debounced_lands_in_pending_not_channel() {
        let state = TopicStore::default();
        let dev = WireDevice::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = test_ctx(&state, &dev, &tx);
        let mut st = DeviceUi::new("dev1".into());

        commit(
            &ctx,
            &mut st,
            "dev1",
            7,
            Layer::Shifted,
            vec![delay_step(500)],
            false,
        );
        assert!(rx.try_recv().is_err());
        assert!(st.pending.contains_key("btn:macro:7:shifted"));
    }

    #[test]
    fn clearing_macro_restores_native_layer_and_sends_valid_mapping() {
        let state = TopicStore::default();
        let dev = WireDevice::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = test_ctx(&state, &dev, &tx);
        let mut st = DeviceUi::new("dev1".into());
        st.keys.keys_action = Some(ButtonAction::Macro {
            steps: vec![key_step(30, true), key_step(30, false)],
        });
        st.keys.keys_shifted_action = Some(ButtonAction::Disable);

        commit_action(
            &ctx,
            &mut st,
            "dev1",
            7,
            Layer::Base,
            ButtonAction::Native,
            true,
        );

        assert_eq!(Layer::Base.get(&st), Some(ButtonAction::Native));
        match rx.try_recv().unwrap() {
            DaemonCommand::Device {
                op: DeviceOp::SetButtonMapping { mapping, .. },
                ..
            } => {
                assert_eq!(mapping.base, ButtonAction::Native);
                assert_eq!(mapping.shifted, ButtonAction::Disable);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn sync_selection_commits_recording_to_its_owning_button() {
        let state = TopicStore::default();
        let dev = WireDevice::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = test_ctx(&state, &dev, &tx);
        let mut st = DeviceUi::new("dev1".into());
        let steps = vec![key_step(30, true)];
        st.keys.keys_action = Some(ButtonAction::Macro {
            steps: steps.clone(),
        });
        st.keys.macro_rec = Some(RecState {
            cid: 1,
            shifted: false,
            last_event: None,
        });
        st.keys.keys_sel_cid = Some(2); // selection moved away mid-recording

        sync_selection(&ctx, &mut st, "dev1");

        assert!(st.keys.macro_rec.is_none());
        match rx.try_recv().unwrap() {
            DaemonCommand::Device {
                op: DeviceOp::SetButtonMapping { mapping, .. },
                ..
            } => {
                assert_eq!(mapping.cid, 1, "must commit to the recording's button");
                assert_eq!(mapping.base, ButtonAction::Macro { steps });
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // The edit window was rewound so the new selection seeds immediately.
        assert!(!crate::ui::screens::device::editing(&st, ctx.time));
    }

    #[test]
    fn sync_selection_drops_stale_drag_and_capture() {
        let state = TopicStore::default();
        let dev = WireDevice::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = test_ctx(&state, &dev, &tx);
        let mut st = DeviceUi::new("dev1".into());
        st.keys.keys_sel_cid = Some(2);
        st.keys.macro_drag = Some(DragState {
            cid: 1,
            shifted: false,
            from: 0,
        });
        st.keys.macro_capture = Some(CaptureState {
            cid: 1,
            shifted: false,
        });
        st.keys.macro_mouse_menu = Some(CaptureState {
            cid: 1,
            shifted: true,
        });

        sync_selection(&ctx, &mut st, "dev1");

        assert!(st.keys.macro_drag.is_none());
        assert!(st.keys.macro_capture.is_none());
        assert!(st.keys.macro_mouse_menu.is_none());
    }

    fn arb_steps() -> impl Strategy<Value = Vec<MacroStep>> {
        proptest::collection::vec(
            (0u8..5, 1u32..200, 0u32..2000).prop_map(|(op, n, ms)| MacroStep {
                kind: match op {
                    0 => MacroAtom::KeyDown { key: n },
                    1 => MacroAtom::KeyUp { key: n },
                    2 => MacroAtom::MouseDown {
                        btn: MouseBtn::Left,
                    },
                    3 => MacroAtom::MouseUp {
                        btn: MouseBtn::Left,
                    },
                    _ => MacroAtom::Delay,
                },
                delay_after_ms: ms,
            }),
            0..30,
        )
    }

    proptest! {
        #[test]
        fn normalize_preserves_total_wait_and_event_order(steps in arb_steps()) {
            let norm = normalize(&steps);
            let total = |s: &[MacroStep]| s.iter().map(|x| x.delay_after_ms as u64).sum::<u64>();
            prop_assert_eq!(total(&norm), total(&steps), "playback wall time must not change");
            let events = |s: &[MacroStep]| s.iter()
                .filter(|x| !matches!(x.kind, MacroAtom::Delay))
                .map(|x| x.kind.clone())
                .collect::<Vec<_>>();
            prop_assert_eq!(events(&norm), events(&steps));
            prop_assert!(norm.iter().all(|s| matches!(s.kind, MacroAtom::Delay) || s.delay_after_ms == 0));
            prop_assert_eq!(normalize(&norm).len(), norm.len(), "normalize must be idempotent");
        }

        #[test]
        fn reorder_preserves_the_multiset(steps in arb_steps(), from in 0usize..40, to in 0usize..40) {
            let mut moved = steps.clone();
            reorder(&mut moved, from, to);
            let sort_key = |s: &MacroStep| format!("{:?}", s);
            let mut a: Vec<String> = steps.iter().map(sort_key).collect();
            let mut b: Vec<String> = moved.iter().map(sort_key).collect();
            a.sort();
            b.sort();
            prop_assert_eq!(a, b);
        }

        #[test]
        fn reorder_to_own_slot_is_identity(steps in arb_steps()) {
            for from in 0..steps.len() {
                let mut moved = steps.clone();
                reorder(&mut moved, from, from);
                prop_assert_eq!(&moved, &steps);
                let mut moved = steps.clone();
                reorder(&mut moved, from, from + 1);
                prop_assert_eq!(&moved, &steps);
            }
        }

        #[test]
        fn resize_delay_is_monotonic_and_bounded(ms in 0u32..=60_000, dx1 in -3000.0f32..3000.0, dx2 in -3000.0f32..3000.0) {
            let (lo, hi) = if dx1 <= dx2 { (dx1, dx2) } else { (dx2, dx1) };
            prop_assert!(resize_delay(ms, lo) <= resize_delay(ms, hi));
            let r = resize_delay(ms, dx1);
            prop_assert!((DELAY_MIN_MS..=MACRO_MAX_DELAY_MS).contains(&r));
        }
    }
}

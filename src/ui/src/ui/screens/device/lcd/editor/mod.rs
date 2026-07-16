// SPDX-License-Identifier: GPL-3.0-or-later
//! Drag-and-drop editor for the daemon's data-driven "custom" LCD template.
//!
//! The editor keeps its own `CustomTemplateDef` buffer as the source of truth
//! while the tab is open, pushing edits out as `LcdEngineSetTemplate`. The
//! stage is a static schematic of the layout, not the daemon's live render —
//! this keeps placement instant and matches the Prism Control design, which
//! also mocks the stage. One divergence: an unbound gauge/bar shows a 65%
//! mock fill so it reads as a gauge, where the device renders it empty.

mod geometry;
mod inspector;
mod library;
mod sprites;
mod stage;

use std::collections::{HashMap, HashSet};

use egui::{Pos2, Rect, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::{CustomTemplateDef, WIDGETS_JSON_PARAM};
use halod_shared::types::EffectParamValue;

use inspector::{empty_selection_card, screen_style_card, selected_widget_card};
use library::{delete_template_modal, library_card};
use stage::stage;

use crate::ui::components as widgets;
use crate::ui::screens::device::{DeviceUi, LcdMediaTab, TabCtx};

/// Per-device editor state, held in `LcdTab`.
#[derive(Default)]
pub struct EditorState {
    pub def: CustomTemplateDef,
    /// Widget ids currently selected on the stage. Per-widget handles and the
    /// inspector only appear when exactly one is selected ([`Self::primary`]);
    /// a multi-selection supports group move and delete.
    pub selected: HashSet<String>,
    /// Rubber-band marquee in progress (drag on empty stage), if any.
    pub marquee: Option<Marquee>,
    /// True once seeded from the daemon's device_template_params (or defaulted).
    pub seeded: bool,
    /// Catalog id currently being dragged from the widget library.
    pub dragging_new: Option<String>,
    /// The stage canvas rect from the current/last frame, so a library-tile
    /// drag release (handled after the stage/inspector split closure returns)
    /// can hit-test against it.
    pub stage_rect: Option<Rect>,
    /// Id of the Text widget currently being edited
    pub editing_text: Option<String>,
    pub focus_editing: bool,
    /// Name typed into the "save as template" field.
    pub template_name: String,
    /// Template name pending delete confirmation, if any.
    pub confirm_delete: Option<String>,
    /// Cached widget-sprite textures from the daemon's editor render, keyed by
    /// widget id → `(signature, texture)`. An unchanged signature skips re-upload.
    pub sprite_tex: HashMap<String, (u64, egui::TextureHandle)>,
    /// Plugin widgets unavailable on the daemon. The stage draws these locally.
    pub missing_widgets: HashSet<String>,
    pub widget_icon_tex: HashMap<String, egui::TextureHandle>,
    pub requested_widget_icons: HashSet<String>,
    /// Font families already resolved locally, including unavailable ones.
    pub attempted_fonts: HashSet<String>,
    pub registered_fonts: HashSet<String>,
    /// Egui time of the last `RenderLcdEditor` request, for rate-limiting.
    pub last_render_req: f64,
    /// Device canvas dims from the most recent render, cached since a delta
    /// reply doesn't repeat them when its `sprites` is empty.
    pub canvas: Option<(u32, u32)>,
    /// Active resize gesture's captured start state, so the outline/sprite can
    /// preview the new size instantly rather than lagging the daemon re-render.
    pub resize_preview: Option<ResizePreview>,
    pub library_collapsed: bool,
}

impl EditorState {
    /// The sole selected widget id, iff exactly one is selected. Drives the
    /// inspector card and the per-widget resize/rotate/remove handles.
    pub fn primary(&self) -> Option<&String> {
        if self.selected.len() == 1 {
            self.selected.iter().next()
        } else {
            None
        }
    }
    pub fn is_selected(&self, id: &str) -> bool {
        self.selected.contains(id)
    }
    /// Replace the selection with just `id`.
    pub fn select_only(&mut self, id: String) {
        self.selected.clear();
        self.selected.insert(id);
    }
    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }
}

/// Rubber-band (marquee) selection over the stage, active while dragging on
/// empty space — mirrors the Effects Canvas marquee.
pub struct Marquee {
    pub start: Pos2,
    pub cur: Pos2,
    /// A modifier was held at start → union with `base` rather than replace.
    pub additive: bool,
    /// Selection present when the marquee began (kept for additive drags).
    pub base: HashSet<String>,
}

/// Snapshot taken when a resize-handle drag begins: the sprite content rect size
/// and the widget scale(s) at that moment. During the drag the stage scales the
/// last-rendered sprite by the live scale ratio, so resizing feels as instant as
/// move/rotate (which need no daemon round-trip).
#[derive(Clone)]
pub struct ResizePreview {
    pub id: String,
    pub start_size: Vec2,
    pub start_scale: f32,
    pub start_scale_y: f32,
}

pub fn show(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    lcd: &halod_shared::types::LcdStatus,
) {
    seed_if_needed(ctx, st, id);
    if Some(LcdMediaTab::Template) != st.lcd.prev_mode_tab {
        send_def(ctx, st, id, true);
        st.lcd.prev_mode_tab = Some(LcdMediaTab::Template);
    }
    if let Some((_name, def)) = ctx.lcd_template.clone() {
        apply_loaded_template(st, def);
        send_def(ctx, st, id, true);
    }

    // Stage + persistent inspector. Keep object controls beside the stage so
    // selecting a widget never causes a long form to appear below the canvas.
    let avail = ui.available_width();
    let mut left_column = Rect::NOTHING;
    let mut inspector_rect = Rect::NOTHING;
    widgets::split_columns(ui, avail * 0.66, 16.0, |left, right| {
        widgets::card(left, |ui| stage(ui, ctx, st, id, lcd));
        // Drop selections whose widgets no longer exist (deleted this frame).
        let live: HashSet<String> = st
            .lcd
            .editor
            .def
            .widgets
            .iter()
            .map(|w| w.id.clone())
            .collect();
        st.lcd.editor.selected.retain(|s| live.contains(s));
        // Selection is edited in the right-hand inspector below. The stage is
        // deliberately the only content in the left column.
        left_column = left.min_rect();

        let sel = st.lcd.editor.primary().cloned();
        widgets::card(right, |ui| match &sel {
            Some(sel) => selected_widget_card(ui, ctx, st, id, sel),
            None => empty_selection_card(ui),
        });
        right.add_space(crate::ui::theme::SPACE_6);
        widgets::card(right, |ui| library_card(ui, ctx, st, id));
        right.add_space(crate::ui::theme::SPACE_6);
        widgets::card(right, |ui| screen_style_card(ui, ctx, st, id, lcd));
        inspector_rect = right.min_rect();
    });

    // Library and stage live in different split-column UIs, so complete the
    // cross-column drop here after both have recorded their response geometry.
    if let Some(catalog_id) = st.lcd.editor.dragging_new.clone() {
        if let Some(pos) = ui.input(|input| input.pointer.hover_pos()) {
            let name = ctx
                .state
                .lcd
                .engine
                .available_widgets
                .iter()
                .find(|descriptor| descriptor.id == catalog_id)
                .map(|descriptor| descriptor.name.as_str())
                .unwrap_or("Widget");
            let ghost = Rect::from_center_size(pos, Vec2::new(80.0, 42.0));
            ui.painter().rect_filled(
                ghost,
                crate::ui::theme::RADIUS_MD,
                crate::ui::theme::a(crate::ui::theme::CYAN, 0.16),
            );
            ui.painter().text(
                ghost.center(),
                egui::Align2::CENTER_CENTER,
                name,
                crate::ui::theme::caption(),
                crate::ui::theme::TEXT,
            );
        }
        if ui.input(|input| input.pointer.primary_released()) {
            st.lcd.editor.dragging_new = None;
            if let (Some(stage_rect), Some(pos)) = (
                st.lcd.editor.stage_rect,
                ui.input(|input| input.pointer.interact_pos()),
            ) {
                if stage_rect.contains(pos) {
                    let descriptor = ctx
                        .state
                        .lcd
                        .engine
                        .available_widgets
                        .iter()
                        .find(|descriptor| descriptor.id == catalog_id)
                        .cloned();
                    if let Some(descriptor) = descriptor {
                        let x = ((pos.x - stage_rect.min.x) / stage_rect.width()).clamp(0.0, 1.0);
                        let y = ((pos.y - stage_rect.min.y) / stage_rect.height()).clamp(0.0, 1.0);
                        let new_id = library::spawn_plugin_widget(st, &descriptor, x, y);
                        st.lcd.editor.select_only(new_id);
                        send_def(ctx, st, id, true);
                    }
                }
            }
        }
    }

    // A click anywhere outside both columns deselects.
    if ui.input(|i| i.pointer.primary_clicked()) {
        if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
            let inside = st.lcd.editor.stage_rect.is_some_and(|r| r.contains(p))
                || inspector_rect.contains(p)
                || left_column.contains(p);
            if !inside {
                st.lcd.editor.clear_selection();
                st.lcd.editor.editing_text = None;
            }
        }
    }

    delete_template_modal(ui, ctx, st);
}

/// Seed the editor's local `def` from the daemon-reported params once per tab
/// open — either the running custom template's live params, or the default.
/// Waits (unseeded) until the daemon reports params for this device at all, so
/// a snapshot arriving after the tab opens still seeds instead of the default
/// def silently replacing a saved layout on the first edit.
fn seed_if_needed(ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    if st.lcd.editor.seeded {
        return;
    }
    let Some(params) = ctx.state.lcd.engine.device_template_params.get(id) else {
        return;
    };
    st.lcd.editor.seeded = true;
    if let Some(EffectParamValue::Str(json)) = params.get(WIDGETS_JSON_PARAM) {
        if let Ok(def) = serde_json::from_str::<CustomTemplateDef>(json) {
            st.lcd.editor.def = def;
        }
    }
}

/// Replace the editor's local buffer with a loaded named template, making it
/// authoritative like any other user edit — a late daemon snapshot from
/// before the load must not clobber it (`seeded = true`, same as `send_def`).
fn apply_loaded_template(st: &mut DeviceUi, def: CustomTemplateDef) {
    st.lcd.editor.def = def;
    st.lcd.editor.clear_selection();
    st.lcd.editor.seeded = true;
}

/// Serialize the local def and push it to the daemon as the "custom" template.
fn send_def(ctx: &TabCtx, st: &mut DeviceUi, id: &str, immediate: bool) {
    // Any user edit makes the local def authoritative: block a later daemon
    // param snapshot (which may reflect an earlier send that's still in flight)
    // from re-seeding and clobbering edits made since. Seeding from a running
    // template only matters before the first edit — see `seed_if_needed`.
    st.lcd.editor.seeded = true;
    let json = match serde_json::to_string(&st.lcd.editor.def) {
        Ok(json) => json,
        Err(e) => {
            log::warn!("[LCD editor] cannot serialize template def: {e}");
            return;
        }
    };
    let mut params = HashMap::new();
    params.insert(WIDGETS_JSON_PARAM.to_string(), EffectParamValue::Str(json));
    if immediate {
        st.last_edit = ctx.time;
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::LcdEngineSetTemplate {
                device_id: id.to_string(),
                template_id: "custom".to_string(),
                params,
            },
        );
    } else {
        let cmd = DaemonCommand::LcdEngineSetTemplate {
            device_id: id.to_string(),
            template_id: "custom".to_string(),
            params,
        };
        st.queue("lcd_editor_def", cmd, ctx.time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::WireDevice;

    fn widget(id: &str, x: f32, y: f32) -> halod_shared::lcd_custom::WidgetDef {
        halod_shared::lcd_custom::WidgetDef {
            id: id.to_string(),
            widget: "test:text".to_owned(),
            x,
            y,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        }
    }

    /// Minimal `TabCtx` for exercising the editor's daemon-facing state logic
    /// (`seed_if_needed`, `send_def`) without an egui frame.
    fn test_ctx<'a>(
        state: &'a halod_shared::types::AppState,
        dev: &'a WireDevice,
        tx: &'a tokio::sync::mpsc::UnboundedSender<DaemonCommand>,
    ) -> TabCtx<'a> {
        TabCtx {
            state,
            dev,
            cmd: tx,
            time: 0.0,
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

    fn params_with(def: &CustomTemplateDef) -> HashMap<String, EffectParamValue> {
        let mut params = HashMap::new();
        params.insert(
            WIDGETS_JSON_PARAM.to_string(),
            EffectParamValue::Str(serde_json::to_string(def).unwrap()),
        );
        params
    }

    #[test]
    fn seed_if_needed_populates_def_from_daemon_params_once() {
        let mut def = CustomTemplateDef::default();
        def.widgets.push(widget("w7", 0.2, 0.3));
        let mut state = halod_shared::types::AppState::default();
        state
            .lcd
            .engine
            .device_template_params
            .insert("lcd".to_string(), params_with(&def));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dev = WireDevice::default();
        let ctx = test_ctx(&state, &dev, &tx);

        let mut st = DeviceUi::new("lcd".into());
        seed_if_needed(&ctx, &mut st, "lcd");
        assert!(st.lcd.editor.seeded);
        assert_eq!(st.lcd.editor.def, def);

        // A second snapshot must not re-seed (would drop edits made since).
        let mut later = halod_shared::types::AppState::default();
        later.lcd.engine.device_template_params.insert(
            "lcd".to_string(),
            params_with(&CustomTemplateDef::default()),
        );
        let ctx = test_ctx(&later, &dev, &tx);
        seed_if_needed(&ctx, &mut st, "lcd");
        assert_eq!(
            st.lcd.editor.def, def,
            "already-seeded editor must not re-seed"
        );
    }

    #[test]
    fn seed_if_needed_waits_until_the_daemon_reports_params() {
        // No params for this device yet (no template active) → stay unseeded so
        // a snapshot arriving later still seeds instead of the default winning.
        let state = halod_shared::types::AppState::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dev = WireDevice::default();
        let ctx = test_ctx(&state, &dev, &tx);

        let mut st = DeviceUi::new("lcd".into());
        seed_if_needed(&ctx, &mut st, "lcd");
        assert!(!st.lcd.editor.seeded);
        // With no daemon params the editor stays at the default plugin template
        // rather than adopting anything — a later snapshot seeds.
        assert_eq!(st.lcd.editor.def, CustomTemplateDef::default());
    }

    #[test]
    fn a_user_edit_blocks_a_late_snapshot_from_clobbering_it() {
        // Cold open (no template active): the user drags in a widget before the
        // daemon round-trips. A stale snapshot reflecting an earlier send must
        // not overwrite the edits made since.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dev = WireDevice::default();
        let mut st = DeviceUi::new("lcd".into());
        st.lcd.editor.def.widgets.clear();
        st.lcd.editor.def.widgets.push(widget("w1", 0.5, 0.5));

        let state = halod_shared::types::AppState::default();
        send_def(&test_ctx(&state, &dev, &tx), &mut st, "lcd", true);
        assert!(
            st.lcd.editor.seeded,
            "an edit makes the local def authoritative"
        );

        // A stale (empty) snapshot arrives afterwards.
        let mut late = halod_shared::types::AppState::default();
        late.lcd.engine.device_template_params.insert(
            "lcd".to_string(),
            params_with(&CustomTemplateDef::default()),
        );
        seed_if_needed(&test_ctx(&late, &dev, &tx), &mut st, "lcd");
        assert_eq!(
            st.lcd.editor.def.widgets.len(),
            1,
            "the user's edit must survive a late daemon snapshot"
        );
    }

    #[test]
    fn selection_helpers_track_single_vs_multi() {
        let mut ed = EditorState::default();
        assert_eq!(ed.primary(), None);

        ed.select_only("a".into());
        assert!(ed.is_selected("a"));
        assert_eq!(ed.primary(), Some(&"a".to_string()));

        // A second selection means no sole "primary" — handles/inspector hide.
        ed.selected.insert("b".into());
        assert_eq!(ed.primary(), None);
        assert!(ed.is_selected("a") && ed.is_selected("b"));

        // select_only replaces the whole set.
        ed.select_only("c".into());
        assert_eq!(ed.selected.len(), 1);
        assert!(ed.is_selected("c") && !ed.is_selected("a"));

        ed.clear_selection();
        assert!(ed.selected.is_empty());
    }

    #[test]
    fn apply_loaded_template_replaces_def_clears_selection_and_locks_out_reseed() {
        let mut st = DeviceUi::new("lcd".into());
        // Pre-existing local edit + selection that the load must overwrite.
        st.lcd.editor.def.widgets.push(widget("old", 0.1, 0.1));
        st.lcd.editor.select_only("old".to_string());

        let mut loaded = CustomTemplateDef::default();
        loaded.widgets.push(widget("new", 0.9, 0.9));
        apply_loaded_template(&mut st, loaded.clone());

        assert_eq!(
            st.lcd.editor.def, loaded,
            "def replaced by the loaded template"
        );
        assert!(st.lcd.editor.selected.is_empty(), "selection cleared");
        assert!(
            st.lcd.editor.seeded,
            "loaded template is authoritative — a late daemon snapshot must not re-seed"
        );

        // A stale snapshot arriving afterwards must not clobber the loaded def.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dev = WireDevice::default();
        let mut late = halod_shared::types::AppState::default();
        late.lcd.engine.device_template_params.insert(
            "lcd".to_string(),
            params_with(&CustomTemplateDef::default()),
        );
        seed_if_needed(&test_ctx(&late, &dev, &tx), &mut st, "lcd");
        assert_eq!(
            st.lcd.editor.def, loaded,
            "loaded template survives a late snapshot"
        );
    }

    #[test]
    fn param_str_and_variant_fall_back_on_missing_key() {
        let w = widget("w1", 0.0, 0.0);
        assert_eq!(halod_shared::lcd_custom::param_str(&w, "text"), "");
        assert_eq!(halod_shared::lcd_custom::param_variant(&w, "stat"), "stat");
    }

    #[test]
    fn shared_size_constants_match_expected() {
        // Pins the editor's view of the shared size constant it still uses (the
        // inline text-edit box); the daemon mirrors it so the two can't drift.
        assert_eq!(halod_shared::lcd_geometry::TEXT_FONT, 0.22);
    }
}

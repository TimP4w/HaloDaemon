// SPDX-License-Identifier: GPL-3.0-or-later
//! Equalizer tab — a preset list on the left, vertical band sliders on the right.

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{DeviceCapability, EqBand, Equalizer};

use super::{editing, DeviceUi, TabCtx};
use crate::ui::theme;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabEqualizer,
        ui.max_rect(),
    );
    let Some(eq) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Equalizer(e) => Some(e.clone()),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

    // Seed the band buffer from daemon state unless the user is mid-edit. Re-seed on
    // a preset switch: different presets carry different curves at the same length.
    if !st.equalizer.eq_seeded
        || (!editing(st, ctx.time)
            && (st.equalizer.eq_preset != eq.selected_preset
                || st.equalizer.eq.len() != eq.bands.len()))
    {
        st.equalizer.eq = eq.bands.iter().map(|b| b.value).collect();
        st.equalizer.eq_preset = eq.selected_preset;
        st.equalizer.eq_seeded = true;
    }

    widgets::card_titled(
        ui,
        &t!("devtabs.equalizer"),
        |_| {},
        |ui| {
            ui.horizontal_top(|ui| {
                preset_list(ui, ctx, &id, &eq);
                ui.add_space(24.0);
                if eq.editable {
                    ui.vertical(|ui| bands(ui, ctx, st, &id, &eq));
                }
            });
        },
    );
}

/// Left column: the Custom preset as a button, every other preset in a select
/// whose placeholder entry maps back to Custom.
fn preset_list(ui: &mut egui::Ui, ctx: &TabCtx, id: &str, eq: &Equalizer) {
    const LIST_W: f32 = 200.0;
    let select = |idx: usize| {
        if idx != eq.selected_preset {
            crate::runtime::ipc::send(
                ctx.cmd,
                halod_shared::commands::DaemonCommand::SetEqPreset {
                    id: id.to_string(),
                    preset_index: idx,
                },
            );
        }
    };
    ui.allocate_ui_with_layout(
        Vec2::new(LIST_W, ui.available_height()),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 10.0);
            ui.spacing_mut().combo_width = LIST_W;

            let custom = eq.presets.iter().position(|p| p.is_custom);
            if let Some(ci) = custom {
                if widgets::pill_row(ui, &eq.presets[ci].label, ci == eq.selected_preset) {
                    select(ci);
                }
            }

            let options: Vec<(String, String)> = eq
                .presets
                .iter()
                .filter(|p| !p.is_custom)
                .map(|p| (p.id.clone(), p.label.clone()))
                .collect();
            if options.is_empty() {
                return;
            }
            let current = eq
                .presets
                .get(eq.selected_preset)
                .filter(|p| !p.is_custom)
                .map_or("", |p| p.id.as_str());
            if let Some(sel) = widgets::combo_picker(
                ui,
                ("eq_preset", id),
                &options,
                current,
                Some(&t!("devtabs.eq_presets")),
            ) {
                let idx = if sel.is_empty() {
                    custom
                } else {
                    eq.presets.iter().position(|p| p.id == sel && !p.is_custom)
                };
                if let Some(idx) = idx {
                    select(idx);
                }
            }
        },
    );
}

fn bands(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, eq: &Equalizer) {
    let n = eq.bands.len().max(1);
    let avail = ui.available_width();
    let col = (avail / n as f32).min(60.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, 200.0), Sense::hover());
    let mut changed = false;
    for (i, b) in eq.bands.iter().enumerate() {
        let cx = rect.left() + col * (i as f32 + 0.5);
        let band_rect =
            Rect::from_center_size(Pos2::new(cx, rect.top() + 80.0), Vec2::new(28.0, 160.0));
        if let Some(v) = st.equalizer.eq.get_mut(i) {
            if vband(ui, band_rect, v, b) {
                changed = true;
                st.last_edit = ctx.time;
            }
        }
        ui.painter().text(
            Pos2::new(cx, rect.top() + 174.0),
            Align2::CENTER_CENTER,
            &b.label,
            theme::caption(),
            theme::TEXT_MUT,
        );
        let val = st.equalizer.eq.get(i).copied().unwrap_or(b.value);
        ui.painter().text(
            Pos2::new(cx, rect.top() + 190.0),
            Align2::CENTER_CENTER,
            format!("{}{:.0}", if val >= 0.0 { "+" } else { "" }, val),
            theme::value_xs(),
            if val >= 0.0 {
                theme::CYAN
            } else {
                theme::STAT_PURPLE
            },
        );
    }
    if changed {
        st.queue(
            "eq",
            DaemonCommand::SetEqBands {
                id: id.to_string(),
                values: st.equalizer.eq.clone(),
            },
            ctx.time,
        );
    }
}

/// Map a normalized track position `t` (0 = bottom, 1 = top) to a band value:
/// linearly interpolate over `[min, max]`, then clamp and snap to the band step.
fn band_value(t: f32, b: &EqBand) -> f32 {
    let v = b.min + t * (b.max - b.min);
    widgets::snap_to_step(v, b.min, b.max, b.step)
}

/// A vertical band slider (track + fill from center + thumb). Returns changed.
fn vband(ui: &mut egui::Ui, rect: Rect, value: &mut f32, b: &EqBand) -> bool {
    let resp = ui.interact(rect, ui.id().with(("eq", b.index)), Sense::click_and_drag());
    let p = ui.painter();
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height()));
    p.rect_filled(track, 2.0, theme::hex(0x222936));

    let mut changed = false;
    if resp.dragged() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((rect.bottom() - pos.y) / rect.height()).clamp(0.0, 1.0);
            *value = band_value(t, b);
            changed = true;
        }
    }
    let t = ((*value - b.min) / (b.max - b.min)).clamp(0.0, 1.0);
    let y = rect.bottom() - t * rect.height();
    let mid = rect.center().y;
    let color = if *value >= 0.0 {
        theme::CYAN
    } else {
        theme::STAT_PURPLE
    };
    p.rect_filled(
        Rect::from_min_max(
            Pos2::new(track.left(), y.min(mid)),
            Pos2::new(track.right(), y.max(mid)),
        ),
        2.0,
        color,
    );
    p.circle_filled(Pos2::new(rect.center().x, y), 7.0, theme::hex(0xdfe6f2));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::band_value;
    use halod_shared::types::EqBand;

    fn band(step: f32) -> EqBand {
        EqBand {
            index: 0,
            label: "60Hz".into(),
            min: -10.0,
            max: 10.0,
            step,
            value: 0.0,
        }
    }

    #[test]
    fn band_value_maps_track_to_range() {
        let b = band(0.0); // no snapping
        assert_eq!(band_value(0.0, &b), -10.0); // bottom
        assert_eq!(band_value(1.0, &b), 10.0); // top
        assert!((band_value(0.5, &b) - 0.0).abs() < 1e-6); // centre
    }

    #[test]
    fn band_value_snaps_to_step_and_clamps() {
        let b = band(2.0);
        // t=0.6 → raw -10 + 12 = 2.0, already a multiple of 2 from min.
        assert_eq!(band_value(0.6, &b), 2.0);
        // t=0.65 → raw 3.0 → snaps to nearest multiple of 2 from min (4.0).
        assert_eq!(band_value(0.65, &b), 4.0);
        // Out-of-range t is clamped by snap_to_step into [min, max].
        assert_eq!(band_value(1.5, &b), 10.0);
        assert_eq!(band_value(-0.5, &b), -10.0);
    }
}

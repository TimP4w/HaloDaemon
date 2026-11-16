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

    let main_w = (ui.available_width() - 320.0 - 16.0).max(320.0);
    widgets::split_columns(ui, main_w, 16.0, |main, aside| {
        widgets::card(main, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SPACE_8;
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("devtabs.equalizer"))
                            .font(theme::semibold(18.0))
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    if eq.editable
                        && widgets::button(
                            ui,
                            &t!("devtabs.eq_reset_flat"),
                            widgets::ButtonKind::Ghost,
                            Vec2::new(0.0, 30.0),
                        )
                        .clicked()
                    {
                        let flat = flat_bands(&eq.bands);
                        st.equalizer.eq = flat.clone();
                        st.last_edit = ctx.time;
                        crate::runtime::ipc::send(
                            ctx.cmd,
                            DaemonCommand::SetEqBands {
                                id: id.clone(),
                                values: flat,
                            },
                        );
                    }
                },
            );
            if eq.editable {
                bands(ui, ctx, st, &id, &eq);
            }
        });
        widgets::card_with_margin(aside, egui::Margin::same(18), |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
            ui.label(
                egui::RichText::new(t!("devtabs.eq_presets"))
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_2);
            preset_list(ui, ctx, &id, &eq);
        });
    });
}

/// Every band at 0 dB, clamped into its range.
fn flat_bands(bands: &[EqBand]) -> Vec<f32> {
    bands.iter().map(|b| 0.0f32.clamp(b.min, b.max)).collect()
}

fn preset_note(preset: &halod_shared::types::EqPreset) -> std::borrow::Cow<'static, str> {
    if preset.is_custom {
        t!("devtabs.eq_custom")
    } else if preset.is_firmware {
        t!("devtabs.eq_device")
    } else {
        std::borrow::Cow::Borrowed("")
    }
}

fn preset_list(ui: &mut egui::Ui, ctx: &TabCtx, id: &str, eq: &Equalizer) {
    for (idx, preset) in eq.presets.iter().enumerate() {
        let selected = idx == eq.selected_preset;
        if widgets::choice_row(ui, &preset.label, &preset_note(preset), selected).clicked()
            && !selected
        {
            crate::runtime::ipc::send(
                ctx.cmd,
                DaemonCommand::SetEqPreset {
                    id: id.to_string(),
                    preset_index: idx,
                },
            );
        }
    }
}

const WELL_H: f32 = 300.0;

fn db_label(v: f32) -> String {
    let v = if v.abs() < 0.05 { 0.0 } else { v };
    if v > 0.0 {
        format!("+{v:.0} dB")
    } else if v < 0.0 {
        format!("−{:.0} dB", -v)
    } else {
        "0 dB".to_owned()
    }
}

fn bands(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, eq: &Equalizer) {
    let n = eq.bands.len().max(1);
    let (well, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), WELL_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(well, 8.0, theme::INNER_BG);
    let plot = Rect::from_min_max(
        well.min + Vec2::new(12.0, 20.0),
        well.max - Vec2::new(12.0, 32.0),
    );
    let mid = plot.center().y;
    p.extend(egui::Shape::dashed_line(
        &[Pos2::new(plot.left(), mid), Pos2::new(plot.right(), mid)],
        egui::Stroke::new(1.0, theme::BORDER_INNER),
        3.0,
        3.0,
    ));
    if let Some(first) = eq.bands.first() {
        p.text(
            Pos2::new(well.left() + 12.0, well.top() + 14.0),
            Align2::LEFT_TOP,
            db_label(first.max),
            theme::caption(),
            theme::TEXT_FAINT,
        );
        p.text(
            Pos2::new(well.left() + 12.0, well.bottom() - 34.0),
            Align2::LEFT_BOTTOM,
            db_label(first.min),
            theme::caption(),
            theme::TEXT_FAINT,
        );
    }
    let col = plot.width() / n as f32;
    let mut changed = false;
    for (i, b) in eq.bands.iter().enumerate() {
        let cx = plot.left() + col * (i as f32 + 0.5);
        let band_rect = Rect::from_center_size(Pos2::new(cx, mid), Vec2::new(28.0, plot.height()));
        if let Some(v) = st.equalizer.eq.get_mut(i) {
            if vband(ui, band_rect, v, b) {
                changed = true;
                st.last_edit = ctx.time;
            }
        }
        ui.painter().text(
            Pos2::new(cx, plot.bottom() + 18.0),
            Align2::CENTER_CENTER,
            &b.label,
            theme::body_sm(),
            theme::TEXT_DIM,
        );
    }
    let (values, _) = ui.allocate_exact_size(Vec2::new(well.width(), 16.0), Sense::hover());
    let vcol = values.width() / n as f32;
    for (i, b) in eq.bands.iter().enumerate() {
        let val = st.equalizer.eq.get(i).copied().unwrap_or(b.value);
        ui.painter().text(
            Pos2::new(values.left() + vcol * (i as f32 + 0.5), values.center().y),
            Align2::CENTER_CENTER,
            db_label(val),
            theme::body_sm(),
            theme::TEXT_SOFT,
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
    let resp = ui.interact(
        rect,
        ui.scope_id().with(("eq", b.index)),
        Sense::click_and_drag(),
    );
    let p = ui.painter();
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height()));
    p.rect_filled(track, 2.0, theme::BORDER);

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
    let color = theme::CYAN;
    p.rect_filled(
        Rect::from_min_max(
            Pos2::new(track.left(), y.min(mid)),
            Pos2::new(track.right(), y.max(mid)),
        ),
        2.0,
        color,
    );
    p.circle_filled(Pos2::new(rect.center().x, y), 8.0, theme::CYAN);
    p.circle_filled(Pos2::new(rect.center().x, y), 5.0, theme::TEXT);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{band_value, db_label, flat_bands};
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

    #[test]
    fn flat_is_zero_clamped_into_each_band() {
        let mut shifted = band(1.0);
        shifted.min = 2.0;
        assert_eq!(flat_bands(&[band(1.0), shifted]), vec![0.0, 2.0]);
    }

    #[test]
    fn db_labels_sign_only_nonzero_gains() {
        assert_eq!(db_label(4.0), "+4 dB");
        assert_eq!(db_label(0.0), "0 dB");
        assert_eq!(db_label(-0.01), "0 dB");
        assert_eq!(db_label(-1.0), "−1 dB");
    }
}

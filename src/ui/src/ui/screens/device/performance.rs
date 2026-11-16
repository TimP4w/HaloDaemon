// SPDX-License-Identifier: GPL-3.0-or-later
//! Performance tab — editable DPI stages (draggable axis + numeric list).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::types::{DeviceCapability, DpiStatus};

use super::{editing, DeviceUi, TabCtx};
use crate::ui::theme;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabPerformance,
        ui.max_rect(),
    );
    let Some(dpi) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Dpi(d) => Some(d.clone()),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

    let available: Vec<u32> = dpi.available_dpis.iter().map(|&v| v as u32).collect();
    let daemon_steps: Vec<u32> = dpi.steps.iter().map(|&v| v as u32).collect();

    // Seed the stage buffer from daemon state unless the user is mid-edit.
    // Compare values, not just length: switching onboard mode / profile changes
    // the step values while often keeping the same count, so a length-only guard
    // would leave the previous profile's stages on screen.
    if should_reseed_dpi(
        st.perf.dpi_seeded,
        editing(st, ctx.time),
        &st.perf.dpi,
        &daemon_steps,
    ) {
        st.perf.dpi = daemon_steps;
        st.perf.dpi_seeded = true;
    }

    let (lo, hi) = dpi_range(&dpi);
    let mut changed = false;

    widgets::card(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Sides::new().height(30.0).show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("devtabs.dpi_stages"))
                        .font(theme::semibold(18.0))
                        .color(theme::TEXT),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("devtabs.add_stage"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(0.0, 30.0),
                )
                .clicked()
                    && st.perf.dpi.len() < 8
                {
                    let v = snap_dpi(
                        st.perf
                            .dpi
                            .last()
                            .copied()
                            .unwrap_or(1600)
                            .saturating_add(400)
                            .min(hi),
                        &available,
                    );
                    st.perf.dpi.push(v);
                    st.last_edit = ctx.time;
                    changed = true;
                }
            },
        );
        ui.add_space(theme::SPACE_9);
        changed |= axis(ui, st, lo, hi, dpi.current_index, &available);
        ui.add_space(theme::SPACE_9);
        changed |= list(ui, st, lo, hi, dpi.current_index, &available);
        ui.add_space(theme::SPACE_9);
        ui.label(
            egui::RichText::new(t!("devtabs.dpi_help"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
    });

    if changed {
        // Onboard DPI edits write to device flash, so debounce more than the
        // default UI window to coalesce a drag / a run of keystrokes into one write.
        st.queue_debounced(
            "dpi",
            DaemonCommand::Device {
                device_id: id,
                op: DeviceOp::SetDpiSteps {
                    steps: st.perf.dpi.clone(),
                },
            },
            ctx.time,
            DPI_DEBOUNCE_SECS,
        );
    }
}

/// Debounce window for DPI edits — longer than the default because onboard DPI
/// writes commit to device flash.
const DPI_DEBOUNCE_SECS: f64 = 0.5;

/// Whether the stage buffer should be re-seeded from daemon state. Re-seed on
/// first paint, or — once the user is no longer editing — whenever the daemon's
/// steps differ from the buffer (onboard mode/profile switch, external change).
fn should_reseed_dpi(seeded: bool, editing: bool, buf: &[u32], daemon: &[u32]) -> bool {
    !seeded || (!editing && buf != daemon)
}

/// Round `dpi` to the closest hardware-supported value. Devices report the full
/// accepted list (`available_dpis`) and the daemon rejects any value not in it,
/// so every edit must snap first. Returns `dpi` unchanged when the list is empty.
fn snap_dpi(dpi: u32, available: &[u32]) -> u32 {
    available
        .iter()
        .copied()
        .min_by_key(|&a| a.abs_diff(dpi))
        .unwrap_or(dpi)
}

fn dpi_range(dpi: &DpiStatus) -> (u32, u32) {
    let lo = dpi.available_dpis.iter().copied().min().unwrap_or(100) as u32;
    let hi = dpi.available_dpis.iter().copied().max().unwrap_or(26000) as u32;
    (lo.min(100), hi.max(1600))
}

fn dpi_t(dpi: f32, lo: f32, hi: f32) -> f32 {
    ((dpi - lo) / (hi - lo).max(1.0)).clamp(0.0, 1.0)
}

fn t_dpi(t: f32, lo: f32, hi: f32) -> f32 {
    lo + t.clamp(0.0, 1.0) * (hi - lo)
}

/// The five axis labels: the floor, then quarters of the ceiling.
fn tick_labels(lo: u32, hi: u32) -> [u32; 5] {
    [lo, hi / 4, hi / 2, hi * 3 / 4, hi]
}

fn axis(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    lo: u32,
    hi: u32,
    current: usize,
    available: &[u32],
) -> bool {
    let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 90.0), Sense::hover());
    let rect = outer.shrink2(Vec2::new(12.0, 0.0));
    let p = ui.painter();
    let track = Rect::from_min_size(
        Pos2::new(rect.left(), rect.top() + 40.0),
        Vec2::new(rect.width(), 4.0),
    );
    p.rect_filled(track, 2.0, theme::BORDER);
    for (k, v) in tick_labels(lo, hi).into_iter().enumerate() {
        let align = match k {
            0 => Align2::LEFT_TOP,
            4 => Align2::RIGHT_TOP,
            _ => Align2::CENTER_TOP,
        };
        p.text(
            Pos2::new(
                rect.left() + rect.width() * k as f32 / 4.0,
                rect.top() + 70.0,
            ),
            align,
            v.to_string(),
            theme::caption(),
            theme::TEXT_FAINT,
        );
    }

    let mut changed = false;
    for i in 0..st.perf.dpi.len() {
        let v = st.perf.dpi[i];
        let cx = rect.left() + dpi_t(v as f32, lo as f32, hi as f32) * rect.width();
        let center = Pos2::new(cx, rect.top() + 36.0);
        let hit = Rect::from_center_size(center, Vec2::splat(24.0));
        let resp = ui.interact(hit, ui.scope_id().with(("dpi", i)), Sense::drag());
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let nt = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                st.perf.dpi[i] =
                    snap_dpi(t_dpi(nt, lo as f32, hi as f32).round() as u32, available);
                st.last_edit = ui.input(|i| i.time);
                changed = true;
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let is_current = i == current;
        let (fill, ring, ink) = if is_current {
            (theme::CYAN, theme::CYAN, theme::ACCENT_INK)
        } else {
            (theme::INNER_BG, theme::TEXT_FAINT2, theme::TEXT_DIM)
        };
        let p = ui.painter();
        p.circle_filled(center, 10.0, fill);
        p.circle_stroke(center, 10.0, Stroke::new(2.0, ring));
        p.text(
            Pos2::new(cx, rect.top()),
            Align2::CENTER_TOP,
            v.to_string(),
            theme::medium(13.0),
            ink,
        );
    }
    changed
}

fn list(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    lo: u32,
    hi: u32,
    current: usize,
    available: &[u32],
) -> bool {
    let mut changed = false;
    let mut remove: Option<usize> = None;
    let n = st.perf.dpi.len();

    for i in 0..n {
        if i > 0 {
            ui.add_space(theme::SPACE_4);
        }
        let is_current = i == current;
        let border = if is_current {
            theme::SELECTED_BORDER
        } else {
            theme::BORDER
        };
        egui::Frame::NONE
            .fill(theme::INNER_BG)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(theme::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::Sides::new().height(32.0).show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(t!("devtabs.stage_n", n = i + 1))
                                .font(theme::medium(14.0))
                                .color(theme::TEXT),
                        );
                        if is_current {
                            ui.label(
                                egui::RichText::new(t!("devtabs.stage_current"))
                                    .font(theme::body_sm())
                                    .color(theme::ACCENT_INK),
                            );
                        }
                    },
                    |ui| {
                        ui.spacing_mut().item_spacing.x = theme::SPACE_4;
                        if n > 1
                            && widgets::button(
                                ui,
                                &t!("devtabs.remove"),
                                widgets::ButtonKind::Ghost,
                                egui::vec2(0.0, 30.0),
                            )
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        ui.add_space(theme::SPACE_3);
                        let mut v = st.perf.dpi[i] as i32;
                        if ui
                            .add_sized(
                                [90.0, 32.0],
                                egui::DragValue::new(&mut v)
                                    .speed(50.0)
                                    .range(lo as i32..=hi as i32),
                            )
                            .changed()
                        {
                            st.perf.dpi[i] =
                                snap_dpi(v.clamp(lo as i32, hi as i32) as u32, available);
                            st.last_edit = ui.input(|inp| inp.time);
                            changed = true;
                        }
                        ui.label(
                            egui::RichText::new("DPI")
                                .font(theme::body_md())
                                .color(theme::TEXT_DIM),
                        );
                    },
                );
            });
    }

    if let Some(i) = remove {
        st.perf.dpi.remove(i);
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{dpi_t, should_reseed_dpi, snap_dpi, t_dpi, tick_labels};

    #[test]
    fn snap_dpi_rounds_to_nearest_available() {
        let avail = [400, 800, 1600, 3200];
        assert_eq!(snap_dpi(700, &avail), 800);
        assert_eq!(snap_dpi(500, &avail), 400);
        assert_eq!(snap_dpi(1600, &avail), 1600);
        assert_eq!(snap_dpi(9000, &avail), 3200);
        // Ties resolve to the first (lower) candidate.
        assert_eq!(snap_dpi(600, &avail), 400);
    }

    #[test]
    fn snap_dpi_passes_through_when_no_list() {
        assert_eq!(snap_dpi(1234, &[]), 1234);
    }

    #[test]
    fn reseed_on_first_paint_then_on_value_change_when_idle() {
        // First paint: always seed.
        assert!(should_reseed_dpi(false, false, &[], &[800, 1600]));
        // Idle + values differ (profile switch, same length): re-seed.
        assert!(should_reseed_dpi(true, false, &[800, 1600], &[400, 800]));
        // Idle + values match: no re-seed.
        assert!(!should_reseed_dpi(true, false, &[800, 1600], &[800, 1600]));
        // Mid-edit: never clobber the buffer even if daemon differs.
        assert!(!should_reseed_dpi(true, true, &[900], &[800, 1600]));
    }

    #[test]
    fn dpi_scale_round_trips() {
        for dpi in [100.0_f32, 800.0, 4000.0, 12000.0, 25600.0] {
            let back = t_dpi(dpi_t(dpi, 100.0, 25600.0), 100.0, 25600.0);
            assert!((back - dpi).abs() < 1.0, "{dpi} -> {back}");
        }
    }

    #[test]
    fn tick_labels_span_the_range_in_increasing_order() {
        let ticks = tick_labels(100, 25600);
        assert_eq!((ticks[0], ticks[4]), (100, 25600));
        assert!(ticks.windows(2).all(|w| w[0] < w[1]));
    }
}

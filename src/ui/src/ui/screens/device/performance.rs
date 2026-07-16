// SPDX-License-Identifier: GPL-3.0-or-later
//! Performance tab — editable DPI stages (draggable axis + numeric list).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
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
        st.perf.dpi_active = dpi.current_index.min(st.perf.dpi.len().saturating_sub(1));
        st.perf.dpi_seeded = true;
    }

    let (lo, hi) = dpi_range(&dpi);
    let mut changed = false;

    widgets::card(ui, |ui| {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("devtabs.dpi_stages"))
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("devtabs.add_stage"),
                    widgets::ButtonKind::Primary,
                    egui::vec2(96.0, 30.0),
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
        ui.add_space(theme::SPACE_7);
        ui.label(
            egui::RichText::new(t!("devtabs.dpi_help"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
        ui.add_space(theme::SPACE_9);
        if axis(ui, st, lo, hi, dpi.current_index, &available) {
            changed = true;
        }
        ui.add_space(22.0);
        if list(ui, st, lo, hi, dpi.current_index, &available) {
            changed = true;
        }
    });

    if changed {
        // Onboard DPI edits write to device flash, so debounce more than the
        // default UI window to coalesce a drag / a run of keystrokes into one write.
        st.queue_debounced(
            "dpi",
            DaemonCommand::SetDpiSteps {
                id,
                steps: st.perf.dpi.clone(),
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

/// Breakpoint of the piecewise-linear DPI track
const BREAK_DPI: f32 = 4000.0;
const BREAK_FRAC: f32 = 0.72;

fn dpi_t(dpi: f32, lo: f32, hi: f32) -> f32 {
    let t = if hi <= BREAK_DPI {
        (dpi - lo) / (hi - lo).max(1.0)
    } else if dpi <= BREAK_DPI {
        (dpi - lo) / (BREAK_DPI - lo).max(1.0) * BREAK_FRAC
    } else {
        BREAK_FRAC + (dpi - BREAK_DPI) / (hi - BREAK_DPI).max(1.0) * (1.0 - BREAK_FRAC)
    };
    t.clamp(0.0, 1.0)
}

fn t_dpi(t: f32, lo: f32, hi: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if hi <= BREAK_DPI || t <= BREAK_FRAC {
        let lo_max = BREAK_DPI.min(hi);
        lo + (t / BREAK_FRAC.max(0.001)) * (lo_max - lo)
    } else {
        BREAK_DPI + ((t - BREAK_FRAC) / (1.0 - BREAK_FRAC).max(0.001)) * (hi - BREAK_DPI)
    }
}

fn fmt_dpi(dpi: u32) -> String {
    if dpi >= 1000 {
        let k = dpi as f32 / 1000.0;
        if k.fract() == 0.0 {
            format!("{}K", k as u32)
        } else {
            format!("{k:.1}K")
        }
    } else {
        dpi.to_string()
    }
}

fn axis(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    lo: u32,
    hi: u32,
    current: usize,
    available: &[u32],
) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::hover());
    let p = ui.painter();
    let track_y = rect.bottom() - 14.0;
    let track = Rect::from_min_max(
        Pos2::new(rect.left(), track_y - 2.0),
        Pos2::new(rect.right(), track_y + 2.0),
    );
    p.rect_filled(track, 3.0, theme::hex(0x222b3a));

    // Tick hints: a few representative DPIs, placed on the same scale.
    let mut ticks = vec![lo, 1000, 2000, BREAK_DPI as u32, hi];
    ticks.retain(|&t| t >= lo && t <= hi);
    ticks.sort_unstable();
    ticks.dedup();
    for t in ticks {
        let x = rect.left() + dpi_t(t as f32, lo as f32, hi as f32) * rect.width();
        p.line_segment(
            [Pos2::new(x, track_y + 4.0), Pos2::new(x, track_y + 9.0)],
            Stroke::new(1.0, theme::hex(0x2a3446)),
        );
        p.text(
            Pos2::new(x, rect.bottom()),
            Align2::CENTER_CENTER,
            fmt_dpi(t),
            theme::mono(8.5),
            theme::TEXT_FAINT2,
        );
    }

    let mut changed = false;
    for i in 0..st.perf.dpi.len() {
        let v = st.perf.dpi[i];
        let t = dpi_t(v as f32, lo as f32, hi as f32);
        let cx = rect.left() + t * rect.width();
        let center = Pos2::new(cx, track_y);
        let hit = Rect::from_center_size(center, Vec2::splat(20.0));
        let resp = ui.interact(hit, ui.id().with(("dpi", i)), Sense::click_and_drag());
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let nt = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                st.perf.dpi[i] =
                    snap_dpi(t_dpi(nt, lo as f32, hi as f32).round() as u32, available);
                st.last_edit = ui.input(|i| i.time);
                changed = true;
            }
        }
        if resp.clicked() {
            st.perf.dpi_active = i;
        }
        // The stage the mouse is currently using (live) gets the cyan glow.
        let is_current = i == current;
        let col = if is_current {
            theme::CYAN
        } else {
            theme::hex(0x60a5fa)
        };
        if is_current {
            theme::glow(ui.painter(), center, 10.0, col, 0.45);
        }
        ui.painter().circle_filled(center, 8.0, col);
        ui.painter().circle_stroke(
            center,
            8.0,
            Stroke::new(
                2.0,
                if i == st.perf.dpi_active {
                    theme::TEXT
                } else {
                    theme::INNER_BG
                },
            ),
        );
        ui.painter().text(
            Pos2::new(cx, track_y - 18.0),
            Align2::CENTER_CENTER,
            fmt_dpi(v),
            theme::value_xs(),
            if is_current {
                theme::CYAN
            } else {
                theme::TEXT_DIM
            },
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
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
            ui.add_space(theme::SPACE_1);
            let (sep, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(sep, 0.0, theme::hex(0x1e2738));
            ui.add_space(theme::SPACE_1);
        }

        let is_current = i == current;
        ui.horizontal(|ui| {
            let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 34.0), Sense::hover());
            ui.painter().circle_filled(
                dot_rect.center(),
                5.0,
                if is_current {
                    theme::CYAN
                } else {
                    theme::hex(0x3a4860)
                },
            );

            let label = if is_current {
                t!("devtabs.stage_n_current", n = i + 1)
            } else {
                t!("devtabs.stage_n", n = i + 1)
            };
            ui.add_sized(
                [120.0, 34.0],
                egui::Label::new(egui::RichText::new(label).font(theme::body_sm()).color(
                    if is_current {
                        theme::CYAN
                    } else {
                        theme::TEXT_MUT
                    },
                )),
            );

            // Reserve space for "DPI" label + optional × button on the right.
            let item_sp = ui.spacing().item_spacing.x;
            let right_w = 32.0 + item_sp + if n > 1 { 30.0 + item_sp } else { 0.0 };
            let input_w = (ui.available_width() - right_w).max(40.0);

            let mut v = st.perf.dpi[i] as i32;
            if ui
                .add_sized(
                    [input_w, 30.0],
                    egui::DragValue::new(&mut v)
                        .speed(50.0)
                        .range(lo as i32..=hi as i32),
                )
                .changed()
            {
                st.perf.dpi[i] = snap_dpi(v.clamp(lo as i32, hi as i32) as u32, available);
                st.last_edit = ui.input(|inp| inp.time);
                changed = true;
            }

            ui.label(
                egui::RichText::new("DPI")
                    .font(theme::body_sm())
                    .color(theme::TEXT_FAINT),
            );

            if n > 1
                && widgets::button(ui, "×", widgets::ButtonKind::Ghost, egui::vec2(28.0, 28.0))
                    .clicked()
            {
                remove = Some(i);
            }
        });
    }

    if let Some(i) = remove {
        st.perf.dpi.remove(i);
        st.perf.dpi_active = st.perf.dpi_active.min(st.perf.dpi.len().saturating_sub(1));
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{dpi_t, fmt_dpi, should_reseed_dpi, snap_dpi, t_dpi};

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
    fn dpi_scale_round_trips_across_the_break() {
        for dpi in [100.0_f32, 800.0, 4000.0, 12000.0, 26000.0] {
            let t = dpi_t(dpi, 100.0, 26000.0);
            let back = t_dpi(t, 100.0, 26000.0);
            assert!((back - dpi).abs() < 1.0, "{dpi} -> {t} -> {back}");
        }
    }

    #[test]
    fn break_point_sits_at_72_percent() {
        assert!((dpi_t(4000.0, 100.0, 26000.0) - 0.72).abs() < 0.001);
    }

    #[test]
    fn fmt_dpi_uses_k_suffix() {
        assert_eq!(fmt_dpi(800), "800");
        assert_eq!(fmt_dpi(1600), "1.6K");
        assert_eq!(fmt_dpi(4000), "4K");
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! Controls tab — generic Choice/Range/Boolean/Action settings grouped by
//! category and laid out in a device-declared responsive grid. The catch-all
//! panel for devices without a richer dedicated tab.

use crate::ui::components as widgets;
use std::collections::{BTreeMap, HashMap};

use halod_shared::commands::DaemonCommand;
use halod_shared::types::{
    CategoryLayout, ChoiceDisplay, DeviceCapability, RangeDisplay, VisibleWhen,
};

use super::{DeviceUi, TabCtx};
use crate::ui::theme;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabControls,
        ui.max_rect(),
    );
    let id = ctx.dev.id.clone();
    let siblings = collect_siblings(&ctx.dev.capabilities);

    // Collect settings keyed by category, preserving capability order within.
    let mut groups: BTreeMap<String, Vec<Setting>> = BTreeMap::new();
    for cap in &ctx.dev.capabilities {
        match cap {
            DeviceCapability::Choice(items) => {
                for c in items {
                    if control_visible(&c.visible_when, &siblings) {
                        groups
                            .entry(cat(&c.category))
                            .or_default()
                            .push(Setting::Choice(c));
                    }
                }
            }
            DeviceCapability::Range(items) => {
                for r in items {
                    if control_visible(&r.visible_when, &siblings) {
                        groups
                            .entry(cat(&r.category))
                            .or_default()
                            .push(Setting::Range(r));
                    }
                }
            }
            DeviceCapability::Boolean(items) => {
                for b in items {
                    // Host mode has its own toggle in the Onboard tab.
                    if b.key == super::onboard::HOST_MODE_KEY {
                        continue;
                    }
                    if control_visible(&b.visible_when, &siblings) {
                        groups
                            .entry(cat(&b.category))
                            .or_default()
                            .push(Setting::Boolean(b));
                    }
                }
            }
            DeviceCapability::Action(items) => {
                for a in items {
                    if control_visible(&a.visible_when, &siblings) {
                        groups
                            .entry(cat(&a.category))
                            .or_default()
                            .push(Setting::Action(a));
                    }
                }
            }
            _ => {}
        }
    }

    let present: Vec<String> = groups.keys().cloned().collect();
    let rows = plan_category_grid(&ctx.dev.control_layout, &present);

    for row in rows {
        let row_cols = row
            .iter()
            .map(|p| (p.column as u32 + p.span as u32).max(1))
            .max()
            .unwrap_or(1) as f32;
        let cell_w = (ui.available_width() / row_cols).max(1.0);
        ui.horizontal_top(|ui| {
            for p in &row {
                let Some(items) = groups.get(&p.category) else {
                    continue;
                };
                ui.allocate_ui_with_layout(
                    egui::vec2(cell_w * p.span as f32, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        widgets::card_titled(
                            ui,
                            &p.category,
                            |_| {},
                            |ui| {
                                for s in items {
                                    setting_row(ui, &id, ctx, st, *s);
                                }
                            },
                        );
                    },
                );
            }
        });
        ui.add_space(16.0);
    }
}

fn cat(c: &str) -> String {
    if c.is_empty() {
        "Settings".to_string()
    } else {
        c.to_string()
    }
}

/// Sibling control values (Choice `selected`, Range `value`, Boolean
/// `0`/`1`) keyed by control key, for evaluating `visible_when`.
fn collect_siblings(caps: &[DeviceCapability]) -> HashMap<String, i64> {
    let mut siblings = HashMap::new();
    for cap in caps {
        match cap {
            DeviceCapability::Choice(items) => {
                for c in items {
                    siblings.insert(c.key.clone(), c.selected as i64);
                }
            }
            DeviceCapability::Range(items) => {
                for r in items {
                    siblings.insert(r.key.clone(), r.value as i64);
                }
            }
            DeviceCapability::Boolean(items) => {
                for b in items {
                    siblings.insert(b.key.clone(), b.value as i64);
                }
            }
            _ => {}
        }
    }
    siblings
}

/// Whether a control should be shown, given the current sibling values. A
/// sibling that can't be found fails open (visible) rather than hiding the
/// control on a stale/missing reference.
fn control_visible(vw: &Option<VisibleWhen>, siblings: &HashMap<String, i64>) -> bool {
    match vw {
        None => true,
        Some(vw) => siblings.get(&vw.key).is_none_or(|v| vw.equals.contains(v)),
    }
}

/// A category card's grid position, in columns.
#[derive(Debug, Clone, PartialEq)]
struct Placement {
    category: String,
    column: u8,
    span: u8,
}

/// Plans the Controls-tab grid: rows of category-card placements. Empty
/// `layouts` (or a device with no `control_layout` entries at all) returns
/// one full-width row per `present_sorted` category, in order — today's
/// stacked behavior. Otherwise, laid-out categories are row-packed by
/// `order`/`column`/`span`, and any present category with no matching layout
/// entry is appended as its own full-width row.
fn plan_category_grid(
    layouts: &[CategoryLayout],
    present_sorted: &[String],
) -> Vec<Vec<Placement>> {
    if layouts.is_empty() {
        return present_sorted
            .iter()
            .map(|c| {
                vec![Placement {
                    category: c.clone(),
                    column: 0,
                    span: 1,
                }]
            })
            .collect();
    }

    let mut laid_out: Vec<&CategoryLayout> = layouts
        .iter()
        .filter(|l| present_sorted.contains(&l.category))
        .collect();
    laid_out.sort_by_key(|l| l.order);

    let ncols = laid_out
        .iter()
        .map(|l| l.column as u32 + l.span.max(1) as u32)
        .max()
        .unwrap_or(1)
        .max(1) as u8;

    let mut rows: Vec<Vec<Placement>> = Vec::new();
    let mut row_occupied: Vec<(u8, u8)> = Vec::new();

    for l in &laid_out {
        let span = l.span.max(1);
        let start = l.column;
        let end = start.saturating_add(span);
        let overlaps = row_occupied.iter().any(|&(s, e)| start < e && s < end);
        let overflows = end > ncols;
        if rows.is_empty() || overlaps || overflows {
            rows.push(Vec::new());
            row_occupied.clear();
        }
        rows.last_mut().unwrap().push(Placement {
            category: l.category.clone(),
            column: start,
            span,
        });
        row_occupied.push((start, end));
    }

    let laid_out_categories: std::collections::HashSet<&str> =
        laid_out.iter().map(|l| l.category.as_str()).collect();
    for c in present_sorted {
        if !laid_out_categories.contains(c.as_str()) {
            rows.push(vec![Placement {
                category: c.clone(),
                column: 0,
                span: ncols,
            }]);
        }
    }

    rows
}

#[derive(Clone, Copy)]
enum Setting<'a> {
    Choice(&'a halod_shared::types::Choice),
    Range(&'a halod_shared::types::Range),
    Boolean(&'a halod_shared::types::Boolean),
    Action(&'a halod_shared::types::Action),
}

fn setting_row(ui: &mut egui::Ui, id: &str, ctx: &TabCtx, st: &mut DeviceUi, s: Setting) {
    match s {
        Setting::Choice(c) => {
            match c.display {
                ChoiceDisplay::Inline => {
                    ui.label(
                        egui::RichText::new(&c.label)
                            .font(theme::body(12.0))
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
                        for (i, opt) in c.options.iter().enumerate() {
                            if widgets::pill(ui, &opt.label, i == c.selected) && i != c.selected {
                                crate::runtime::ipc::send(
                                    ctx.cmd,
                                    halod_shared::commands::DaemonCommand::SetChoice {
                                        id: id.to_string(),
                                        key: c.key.clone(),
                                        selected: i,
                                    },
                                );
                            }
                        }
                    });
                }
                ChoiceDisplay::List => {
                    ui.label(
                        egui::RichText::new(&c.label)
                            .font(theme::body(12.0))
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(6.0);
                    let options: Vec<(String, String)> = c
                        .options
                        .iter()
                        .enumerate()
                        .map(|(i, opt)| (i.to_string(), opt.label.clone()))
                        .collect();
                    if let Some(new_id) =
                        widgets::combo_picker(ui, &c.key, &options, &c.selected.to_string(), None)
                    {
                        if let Ok(idx) = new_id.parse::<usize>() {
                            if idx != c.selected {
                                crate::runtime::ipc::send(
                                    ctx.cmd,
                                    halod_shared::commands::DaemonCommand::SetChoice {
                                        id: id.to_string(),
                                        key: c.key.clone(),
                                        selected: idx,
                                    },
                                );
                            }
                        }
                    }
                }
                ChoiceDisplay::Toggle => {
                    egui::Sides::new().show(
                        ui,
                        |ui| {
                            ui.label(
                                egui::RichText::new(&c.label)
                                    .font(theme::body(12.0))
                                    .color(theme::TEXT_DIM),
                            );
                        },
                        |ui| {
                            let on = c.selected == 1;
                            if widgets::toggle(ui, on) != on {
                                crate::runtime::ipc::send(
                                    ctx.cmd,
                                    halod_shared::commands::DaemonCommand::SetChoice {
                                        id: id.to_string(),
                                        key: c.key.clone(),
                                        selected: if on { 0 } else { 1 },
                                    },
                                );
                            }
                        },
                    );
                }
            }
            ui.add_space(14.0);
        }
        Setting::Range(r) => {
            let key = format!("range:{}", r.key);
            let v = st.guarded(&key, r.value as f32, ctx.time);
            let readout = format!("{}", v.round() as i32);
            match r.display {
                RangeDisplay::Slider => {
                    let mut v = v;
                    if widgets::slider_row(
                        ui,
                        &r.label,
                        &mut v,
                        r.min as f32..=r.max as f32,
                        &readout,
                    ) && !r.read_only
                    {
                        // Clamp + snap to the declared step before sending.
                        let step = (r.step.max(1)) as f32;
                        let snapped = widgets::snap_to_step(v, r.min as f32, r.max as f32, step);
                        st.set(&key, snapped, ctx.time);
                        st.queue(
                            &key,
                            DaemonCommand::SetRange {
                                id: id.to_string(),
                                key: r.key.clone(),
                                value: snapped.round() as i32,
                            },
                            ctx.time,
                        );
                    }
                }
                RangeDisplay::Stepper => {
                    let delta = widgets::stepper_row(ui, &r.label, &readout);
                    if delta != 0 && !r.read_only {
                        let step = (r.step.max(1)) as f32;
                        let snapped = widgets::snap_to_step(
                            v + delta as f32 * step,
                            r.min as f32,
                            r.max as f32,
                            step,
                        );
                        st.set(&key, snapped, ctx.time);
                        st.queue(
                            &key,
                            DaemonCommand::SetRange {
                                id: id.to_string(),
                                key: r.key.clone(),
                                value: snapped.round() as i32,
                            },
                            ctx.time,
                        );
                    }
                }
            }
            ui.add_space(14.0);
        }
        Setting::Boolean(b) => {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(&b.label)
                            .font(theme::body(12.0))
                            .color(theme::TEXT_DIM),
                    );
                },
                |ui| {
                    let on = b.value;
                    if b.read_only {
                        // Paint a non-interactive toggle showing the current value.
                        let (rect, _resp) = ui
                            .allocate_exact_size(egui::Vec2::new(34.0, 18.0), egui::Sense::hover());
                        let t = ui.ctx().animate_bool_with_time(ui.next_auto_id(), on, 0.1);
                        widgets::paint_toggle(ui.painter(), rect, t);
                    } else {
                        let new_val = widgets::toggle(ui, on);
                        if new_val != on {
                            crate::runtime::ipc::send(
                                ctx.cmd,
                                halod_shared::commands::DaemonCommand::SetBoolean {
                                    id: id.to_string(),
                                    key: b.key.clone(),
                                    value: !on,
                                },
                            );
                        }
                    }
                },
            );
            ui.add_space(10.0);
        }
        Setting::Action(a) => {
            if widgets::button(
                ui,
                &a.label,
                widgets::ButtonKind::Ghost,
                egui::vec2(ui.available_width().min(180.0), 32.0),
            )
            .clicked()
            {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    halod_shared::commands::DaemonCommand::TriggerAction {
                        id: id.to_string(),
                        key: a.key.clone(),
                    },
                );
            }
            ui.add_space(10.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cat_defaults_empty_to_settings() {
        assert_eq!(cat(""), "Settings");
        assert_eq!(cat("Performance"), "Performance");
    }

    // ── control_visible ─────────────────────────────────────────────────

    #[test]
    fn control_visible_no_condition_is_always_visible() {
        assert!(control_visible(&None, &HashMap::new()));
    }

    #[test]
    fn control_visible_matches_sibling_value() {
        let vw = Some(VisibleWhen {
            key: "nc_mode".into(),
            equals: vec![1],
        });
        let mut siblings = HashMap::new();
        siblings.insert("nc_mode".to_string(), 1i64);
        assert!(control_visible(&vw, &siblings));
    }

    #[test]
    fn control_visible_hidden_when_sibling_value_does_not_match() {
        let vw = Some(VisibleWhen {
            key: "nc_mode".into(),
            equals: vec![1],
        });
        let mut siblings = HashMap::new();
        siblings.insert("nc_mode".to_string(), 0i64);
        assert!(!control_visible(&vw, &siblings));
    }

    #[test]
    fn control_visible_fails_open_when_sibling_missing() {
        let vw = Some(VisibleWhen {
            key: "missing".into(),
            equals: vec![1],
        });
        assert!(control_visible(&vw, &HashMap::new()));
    }

    // ── plan_category_grid ──────────────────────────────────────────────

    fn names(rows: &[Vec<Placement>]) -> Vec<Vec<&str>> {
        rows.iter()
            .map(|row| row.iter().map(|p| p.category.as_str()).collect())
            .collect()
    }

    #[test]
    fn plan_category_grid_empty_layout_stacks_full_width_alphabetically() {
        let present = vec!["Audio".to_string(), "Microphone".to_string()];
        let rows = plan_category_grid(&[], &present);
        assert_eq!(names(&rows), vec![vec!["Audio"], vec!["Microphone"]]);
        for row in &rows {
            assert_eq!(row[0].column, 0);
            assert_eq!(row[0].span, 1);
        }
    }

    #[test]
    fn plan_category_grid_packs_non_overlapping_columns_into_one_row() {
        let present = vec!["A".to_string(), "B".to_string()];
        let layouts = vec![
            CategoryLayout {
                category: "A".into(),
                order: 0,
                column: 0,
                span: 1,
            },
            CategoryLayout {
                category: "B".into(),
                order: 1,
                column: 1,
                span: 1,
            },
        ];
        let rows = plan_category_grid(&layouts, &present);
        assert_eq!(names(&rows), vec![vec!["A", "B"]]);
    }

    #[test]
    fn plan_category_grid_reused_column_starts_a_new_row() {
        let present = vec!["A".to_string(), "B".to_string()];
        let layouts = vec![
            CategoryLayout {
                category: "A".into(),
                order: 0,
                column: 0,
                span: 1,
            },
            CategoryLayout {
                category: "B".into(),
                order: 1,
                column: 0,
                span: 1,
            },
        ];
        let rows = plan_category_grid(&layouts, &present);
        assert_eq!(names(&rows), vec![vec!["A"], vec!["B"]]);
    }

    #[test]
    fn plan_category_grid_respects_spans() {
        let present = vec!["Wide".to_string(), "Narrow".to_string()];
        let layouts = vec![
            CategoryLayout {
                category: "Wide".into(),
                order: 0,
                column: 0,
                span: 2,
            },
            CategoryLayout {
                category: "Narrow".into(),
                order: 1,
                column: 0,
                span: 1,
            },
        ];
        let rows = plan_category_grid(&layouts, &present);
        // Narrow's column 0 overlaps Wide's [0, 2) span, so it starts a new row.
        assert_eq!(names(&rows), vec![vec!["Wide"], vec!["Narrow"]]);
        assert_eq!(rows[0][0].span, 2);
    }

    #[test]
    fn plan_category_grid_appends_unlaid_out_categories_full_width() {
        let present = vec!["A".to_string(), "Unlisted".to_string()];
        let layouts = vec![CategoryLayout {
            category: "A".into(),
            order: 0,
            column: 0,
            span: 1,
        }];
        let rows = plan_category_grid(&layouts, &present);
        assert_eq!(names(&rows), vec![vec!["A"], vec!["Unlisted"]]);
        // The unlaid-out row spans the full grid width computed from `layouts`.
        assert_eq!(rows[1][0].span, 1);
    }
}

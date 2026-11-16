// SPDX-License-Identifier: GPL-3.0-or-later
//! The Home dashboard's configurable widget row: layout, Customize mode's
//! working copy, and the drag-reorder / add / remove interactions. The tiles
//! themselves are painted by [`super::widget_view`].

use std::collections::{HashMap, VecDeque};

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{HomeWidget, MAX_HOME_WIDGETS};

use crate::domain::models::sensors::SensorView;
use crate::domain::state::Debouncer;
use crate::domain::topic_store::TopicStore;
use crate::runtime::ipc::CommandTx;
use crate::ui::theme::{self, a};

use super::widget_config::Draft;
use super::widget_view::{self, CELL_H};

/// Cells per strip row.
const COLS: usize = 3;
const BADGE: f32 = 20.0;
const SAVE_DEBOUNCE: f64 = 0.25;

/// Customize mode and its unsaved working copy.
#[derive(Default)]
pub struct EditState {
    /// `None` follows the bus; `Some` is Customize mode's working copy.
    draft: Option<Vec<HomeWidget>>,
    /// The open add/edit modal.
    modal: Option<Draft>,
    save: Debouncer,
}

impl EditState {
    pub fn customizing(&self) -> bool {
        self.draft.is_some()
    }

    /// Enter Customize mode on a snapshot of the persisted row, or leave it.
    pub fn toggle(&mut self, state: &TopicStore) {
        self.draft = match self.draft {
            Some(_) => None,
            None => Some(state.gui.home_widgets.clone()),
        };
        self.modal = None;
    }

    fn row<'a>(&'a self, state: &'a TopicStore) -> &'a [HomeWidget] {
        self.draft.as_deref().unwrap_or(&state.gui.home_widgets)
    }

    /// Edit the working copy and schedule the persist.
    fn apply(&mut self, state: &TopicStore, time: f64, edit: impl FnOnce(&mut Vec<HomeWidget>)) {
        let row = self
            .draft
            .get_or_insert_with(|| state.gui.home_widgets.clone());
        edit(row);
        self.save.queue(
            "home_widgets",
            DaemonCommand::SetHomeWidgets {
                widgets: row.clone(),
            },
            time,
            SAVE_DEBOUNCE,
        );
    }
}

pub struct RowCtx<'a> {
    pub state: &'a TopicStore,
    pub cmd: &'a CommandTx,
    pub history: &'a HashMap<String, VecDeque<f32>>,
    pub sensors: &'a [SensorView],
    pub time: f64,
}

/// Where strip cell `i` paints inside the strip's `area`.
fn cell_rect(area: Rect, i: usize) -> Rect {
    let w = area.width() / COLS as f32;
    Rect::from_min_size(
        Pos2::new(
            area.left() + (i % COLS) as f32 * w,
            area.top() + (i / COLS) as f32 * CELL_H,
        ),
        Vec2::new(w, CELL_H),
    )
}

fn strip_height(cells: usize) -> f32 {
    cells.div_ceil(COLS) as f32 * CELL_H
}

/// The next free widget id for `row`, so ids stay unique without a clock or RNG.
pub fn next_widget_id(row: &[HomeWidget]) -> String {
    let next = row
        .iter()
        .filter_map(|w| w.id.strip_prefix('w')?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1);
    format!("w{next}")
}

pub fn show(ui: &mut egui::Ui, edit: &mut EditState, ctx: RowCtx) {
    edit.save.flush(ctx.cmd, ctx.time);
    let customizing = edit.customizing();
    let row = edit.row(ctx.state).to_vec();
    if row.is_empty() && !customizing {
        return;
    }

    let can_add = customizing && row.len() < MAX_HOME_WIDGETS;
    let cells = row.len() + usize::from(can_add);
    let (area, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), strip_height(cells)),
        Sense::hover(),
    );
    strip_frame(ui.painter(), area, cells);
    let mut moved = None;
    let mut removed = None;

    for idx in 0..cells {
        let rect = cell_rect(area, idx).shrink(1.0);
        let Some(w) = row.get(idx) else {
            if add_tile(ui, rect) {
                edit.modal = Some(Draft::adding());
            }
            continue;
        };
        let (badge, resp) = tile(ui, w, rect, customizing, &ctx);
        if !customizing {
            continue;
        }
        resp.dnd_set_drag_payload(idx);
        if let Some(src) = resp.dnd_release_payload::<usize>() {
            moved = Some((*src, idx));
        } else if resp.dnd_hover_payload::<usize>().is_some() {
            widget_view::frame(ui.painter(), rect, theme::CYAN);
        } else if resp.clicked() {
            let on_badge = resp
                .interact_pointer_pos()
                .is_some_and(|p| badge.contains(p));
            if on_badge {
                removed = Some(idx);
            } else {
                edit.modal = Some(Draft::editing(w));
            }
        }
    }

    if let Some(idx) = removed {
        edit.apply(ctx.state, ctx.time, |row| {
            row.remove(idx);
        });
    } else if let Some((from, to)) = moved.filter(|(from, to)| from != to) {
        edit.apply(ctx.state, ctx.time, |row| {
            let item = row.remove(from);
            row.insert(to.min(row.len()), item);
        });
    }

    modal(ui.ctx(), edit, &ctx);
}

/// The shared card behind the strip, with dividers between its cells.
fn strip_frame(p: &egui::Painter, area: Rect, cells: usize) {
    theme::paint_surface(p, area, theme::RADIUS_MD, theme::CARD_BG, theme::BORDER);
    for i in 0..cells {
        let rect = cell_rect(area, i);
        if i % COLS < COLS - 1 {
            p.vline(
                rect.right(),
                rect.y_range(),
                Stroke::new(1.0, theme::BORDER_SOFT),
            );
        }
        if i >= COLS {
            p.hline(
                rect.x_range(),
                rect.top(),
                Stroke::new(1.0, theme::BORDER_SOFT),
            );
        }
    }
}

fn modal(ctx_egui: &egui::Context, edit: &mut EditState, ctx: &RowCtx) {
    let Some(mut draft) = edit.modal.take() else {
        return;
    };
    match super::widget_config::show(ctx_egui, &mut draft, ctx) {
        Some(super::widget_config::Outcome::Save) => {
            edit.apply(ctx.state, ctx.time, |row| {
                match row
                    .iter_mut()
                    .find(|w| Some(&w.id) == draft.editing.as_ref())
                {
                    Some(existing) => *existing = draft.to_widget(existing.id.clone()),
                    None => row.push(draft.to_widget(next_widget_id(row))),
                }
            });
        }
        Some(super::widget_config::Outcome::Close) => {}
        None => edit.modal = Some(draft),
    }
}

/// Lay out one tile, returning its remove-badge rect (empty outside Customize)
/// and the response that carries drag and click.
fn tile(
    ui: &mut egui::Ui,
    w: &HomeWidget,
    rect: Rect,
    customizing: bool,
    ctx: &RowCtx,
) -> (Rect, egui::Response) {
    let sense = if customizing {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let resp = ui.interact(rect, egui::Id::unique(("home_widget", &w.id)), sense);
    let color = theme::widget_hue(w.color);
    let resolved = widget_view::resolve(w, ctx.sensors, ctx.state);
    let p = ui.painter();
    widget_view::paint(p, rect, w, resolved.as_ref(), ctx.history);
    if !customizing {
        return (Rect::NOTHING, resp);
    }
    widget_view::frame(p, rect, a(color, 0.5));
    let badge = Rect::from_min_size(
        Pos2::new(rect.right() - BADGE - 8.0, rect.top() + 8.0),
        Vec2::splat(BADGE),
    );
    p.rect_filled(badge, theme::RADIUS_SM, theme::INNER_BG);
    p.rect_stroke(
        badge,
        theme::RADIUS_SM,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    p.text(
        badge.center(),
        Align2::CENTER_CENTER,
        "×",
        theme::body_md(),
        theme::OFFLINE_TEXT,
    );
    (badge, resp)
}

fn add_tile(ui: &mut egui::Ui, rect: Rect) -> bool {
    let resp = ui.interact(rect, egui::Id::unique("home_widget_add"), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if hovered {
        theme::CYAN
    } else {
        theme::TEXT_FAINT
    };
    let p = ui.painter();
    if hovered {
        widget_view::frame(p, rect, theme::CYAN);
    }
    p.text(
        Pos2::new(rect.center().x, rect.center().y - 10.0),
        Align2::CENTER_CENTER,
        "+",
        theme::title(),
        color,
    );
    p.text(
        Pos2::new(rect.center().x, rect.center().y + 14.0),
        Align2::CENTER_CENTER,
        t!("home.widget_add"),
        theme::body_sm(),
        color,
    );
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::HomeWidgetKind;

    fn widget(id: &str) -> HomeWidget {
        HomeWidget {
            id: id.into(),
            kind: HomeWidgetKind::Chart {
                sensor_id: "cpu".into(),
            },
            color: 0,
            label: String::new(),
        }
    }

    #[test]
    fn customize_mode_snapshots_the_persisted_row_and_releases_it_on_exit() {
        let persisted = widget("w1");
        let state = TopicStore {
            gui: halod_shared::types::GuiConfig {
                home_widgets: vec![persisted.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut edit = EditState::default();
        assert!(!edit.customizing());
        assert_eq!(edit.row(&state), std::slice::from_ref(&persisted));

        edit.toggle(&state);
        assert!(edit.customizing());
        edit.apply(&state, 0.0, |row| row.clear());
        assert!(edit.row(&state).is_empty(), "edits hit the working copy");

        edit.toggle(&state);
        assert_eq!(
            edit.row(&state),
            std::slice::from_ref(&persisted),
            "leaving Customize follows the bus again"
        );
    }

    #[test]
    fn edits_coalesce_into_one_debounced_save() {
        let state = TopicStore::default();
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut edit = EditState::default();
        edit.toggle(&state);
        for i in 0..3 {
            edit.apply(&state, i as f64 * 0.01, |row| {
                row.push(widget(&format!("w{i}")));
            });
        }
        edit.save.flush(&cmd, 0.02);
        assert!(rx.try_recv().is_err(), "still inside the debounce window");

        edit.save.flush(&cmd, 0.02 + SAVE_DEBOUNCE);
        let sent = rx.try_recv().expect("one save after the quiet period");
        let DaemonCommand::SetHomeWidgets { widgets } = sent else {
            panic!("expected SetHomeWidgets");
        };
        assert_eq!(widgets.len(), 3, "the save carries the final row");
        assert!(rx.try_recv().is_err(), "edits coalesce into a single save");
    }

    #[test]
    fn strip_cells_tile_three_per_row_without_overlap() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, strip_height(5)));
        let rects: Vec<_> = (0..5).map(|i| cell_rect(area, i)).collect();
        assert_eq!(rects[2].right(), area.right());
        assert_eq!(rects[3].left(), area.left());
        assert_eq!(rects[3].top(), rects[0].bottom());
        assert!(rects.iter().all(|r| area.contains_rect(*r)));
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(a.intersect(*b).area() <= 0.0);
            }
        }
    }

    #[test]
    fn strip_height_rounds_up_to_whole_rows() {
        assert_eq!(strip_height(0), 0.0);
        assert_eq!(strip_height(1), CELL_H);
        assert_eq!(strip_height(3), CELL_H);
        assert_eq!(strip_height(4), CELL_H * 2.0);
    }

    #[test]
    fn next_widget_id_never_collides_with_an_existing_one() {
        assert_eq!(next_widget_id(&[widget("w1"), widget("w7")]), "w8");
        assert_eq!(next_widget_id(&[]), "w1");
    }
}

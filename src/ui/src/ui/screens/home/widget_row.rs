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
use super::widget_view::{self, TILE_H};
use super::GAP;

/// Half-columns per row: four full-width tiles, or eight gauges.
const UNITS: usize = 8;
const BADGE: f32 = 20.0;
const SAVE_DEBOUNCE: f64 = 0.25;
const SAVE_KEY: &str = "home_widgets";

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
            SAVE_KEY,
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

/// A tile's placement on the half-column grid.
#[derive(Debug, PartialEq, Eq)]
pub struct Cell {
    pub col: usize,
    pub row: usize,
    pub size: widget_view::Size,
}

/// First-fit placement of `sizes` on a [`UNITS`]-wide grid, scanning top-left
/// to bottom-right. Tall gauges leave a short slot beside them, which the next
/// tile that fits backfills, so the row packs densely without reordering.
pub fn pack(sizes: &[widget_view::Size]) -> Vec<Cell> {
    let mut grid: Vec<[bool; UNITS]> = Vec::new();
    let mut cells = Vec::with_capacity(sizes.len());
    for &size in sizes {
        let (col, row) = 'placed: {
            for row in 0.. {
                while grid.len() < row + size.rows {
                    grid.push([false; UNITS]);
                }
                for col in 0..=UNITS - size.cols {
                    let free = grid[row..row + size.rows]
                        .iter()
                        .all(|r| r[col..col + size.cols].iter().all(|taken| !taken));
                    if free {
                        break 'placed (col, row);
                    }
                }
            }
            unreachable!("an empty row always fits a tile no wider than the grid")
        };
        for r in &mut grid[row..row + size.rows] {
            r[col..col + size.cols].fill(true);
        }
        cells.push(Cell { col, row, size });
    }
    cells
}

/// Total height of a packed grid, in painted pixels.
fn grid_height(cells: &[Cell]) -> f32 {
    let rows = cells
        .iter()
        .map(|c| c.row + c.size.rows)
        .max()
        .unwrap_or_default();
    span_len(rows, TILE_H)
}

/// The painted extent of `n` grid steps of `step` pixels, gaps included.
fn span_len(n: usize, step: f32) -> f32 {
    (n as f32 * step + GAP * (n as f32 - 1.0)).max(0.0)
}

/// Where a placed tile paints inside the grid's `area`.
fn cell_rect(area: Rect, cell: &Cell) -> Rect {
    let unit = (area.width() - GAP * (UNITS as f32 - 1.0)) / UNITS as f32;
    Rect::from_min_size(
        Pos2::new(
            area.left() + cell.col as f32 * (unit + GAP),
            area.top() + cell.row as f32 * (TILE_H + GAP),
        ),
        Vec2::new(
            span_len(cell.size.cols, unit),
            span_len(cell.size.rows, TILE_H),
        ),
    )
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
    let sizes: Vec<widget_view::Size> = row
        .iter()
        .map(|w| widget_view::size(&w.kind))
        .chain(can_add.then_some(widget_view::Size::FULL))
        .collect();
    let cells = pack(&sizes);
    let (area, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), grid_height(&cells)),
        Sense::hover(),
    );
    let mut moved = None;
    let mut removed = None;

    for (idx, cell) in cells.iter().enumerate() {
        let rect = cell_rect(area, cell);
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
    let resp = ui.interact(rect, egui::Id::new(("home_widget", &w.id)), sense);
    let color = theme::widget_hue(w.color);
    let resolved = widget_view::resolve(w, ctx.sensors, ctx.state);
    let p = ui.painter();
    widget_view::paint(p, rect, w, resolved.as_ref(), ctx.history);
    widget_view::frame(
        p,
        rect,
        if customizing {
            a(color, 0.5)
        } else {
            theme::BORDER
        },
    );

    if !customizing {
        return (Rect::NOTHING, resp);
    }
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
    let resp = ui.interact(rect, egui::Id::new("home_widget_add"), Sense::click());
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
    widget_view::frame(p, rect, if hovered { theme::CYAN } else { theme::BORDER });
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

    use widget_view::Size;

    fn at(col: usize, row: usize) -> (usize, usize) {
        (col, row)
    }

    fn placements(sizes: &[Size]) -> Vec<(usize, usize)> {
        pack(sizes).iter().map(|c| at(c.col, c.row)).collect()
    }

    #[test]
    fn four_full_tiles_fill_a_row_and_the_fifth_wraps() {
        assert_eq!(
            placements(&[Size::FULL; 5]),
            vec![at(0, 0), at(2, 0), at(4, 0), at(6, 0), at(0, 1)]
        );
    }

    #[test]
    fn gauges_pack_four_across_and_span_two_rows() {
        let cells = pack(&[Size::GAUGE; 5]);
        let cols: Vec<_> = cells.iter().map(|c| at(c.col, c.row)).collect();
        assert_eq!(cols[0], at(0, 0));
        assert_eq!(cols[3], at(6, 0));
        // The fifth starts a new band, two rows below — gauges are double height.
        assert_eq!(cols[4], at(0, 2));
    }

    #[test]
    fn a_short_tile_backfills_the_row_beside_a_tall_gauge() {
        // The gauge holds columns 0-1 across both rows; charts fill the rest of
        // row 0, then wrap into row 1 beside it rather than below the whole band.
        let sizes = [Size::GAUGE, Size::FULL, Size::FULL, Size::FULL, Size::FULL];
        assert_eq!(
            placements(&sizes),
            vec![at(0, 0), at(2, 0), at(4, 0), at(6, 0), at(2, 1)]
        );
    }

    #[test]
    fn a_gauge_is_a_square_tile_two_rows_tall() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let cells = pack(&[Size::FULL, Size::GAUGE]);
        let full = cell_rect(area, &cells[0]);
        let gauge = cell_rect(area, &cells[1]);
        assert_eq!(gauge.width(), full.width());
        assert!((gauge.height() - (full.height() * 2.0 + GAP)).abs() < 0.01);
        // Near-square: the two-row height is within a tile gap of the width.
        assert!(
            (gauge.width() - gauge.height()).abs() <= GAP * 2.0,
            "{gauge:?}"
        );
        // Four full tiles plus their gaps consume the row exactly.
        assert!((full.width() * 4.0 + GAP * 3.0 - area.width()).abs() < 0.01);
    }

    #[test]
    fn grid_height_covers_the_tallest_column() {
        assert_eq!(grid_height(&pack(&[Size::FULL])), TILE_H);
        assert_eq!(grid_height(&pack(&[Size::GAUGE])), TILE_H * 2.0 + GAP);
        assert_eq!(
            grid_height(&pack(&[Size::GAUGE, Size::FULL])),
            TILE_H * 2.0 + GAP,
            "a short tile beside a gauge must not shrink the band"
        );
    }

    #[test]
    fn next_widget_id_never_collides_with_an_existing_one() {
        assert_eq!(next_widget_id(&[widget("w1"), widget("w7")]), "w8");
        assert_eq!(next_widget_id(&[]), "w1");
    }
}

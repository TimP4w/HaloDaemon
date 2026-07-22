// SPDX-License-Identifier: GPL-3.0-or-later
//! The add / edit dialog for a Home dashboard widget: type, source, label,
//! accent, and (for a gauge) the range its ring maps onto.

use halod_shared::types::{HomeWidget, HomeWidgetKind, MAX_BATTERY_WIDGET_ENTRIES};

use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;

use super::widget_row::RowCtx;
use super::widget_view::{self, TILE_H};
use super::GAP;

/// Preview tiles are drawn at the dashboard's four-column width so the layout
/// the user approves is the layout they get.
const PREVIEW_W: f32 = 260.0;
const PREVIEW_ID: &str = "preview";

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Chart,
    Gauge,
    Battery,
}

impl Kind {
    const ALL: [Self; 3] = [Self::Chart, Self::Gauge, Self::Battery];

    fn name(self) -> String {
        match self {
            Self::Chart => t!("home.widget_chart"),
            Self::Gauge => t!("home.widget_gauge"),
            Self::Battery => t!("home.widget_battery"),
        }
        .to_string()
    }

    fn description(self) -> String {
        match self {
            Self::Chart => t!("home.widget_chart_desc"),
            Self::Gauge => t!("home.widget_gauge_desc"),
            Self::Battery => t!("home.widget_battery_desc"),
        }
        .to_string()
    }
}

/// The in-progress widget the dialog edits. `editing` is the id of the widget
/// being changed, or `None` when adding.
pub struct Draft {
    pub editing: Option<String>,
    pub kind: Kind,
    pub sensor_id: String,
    pub batteries: Vec<String>,
    pub min: f32,
    pub max: f32,
    pub label: String,
    pub color: u8,
}

pub enum Outcome {
    Save,
    Close,
}

/// The gauge range a sensor's unit implies, so a new gauge is useful before the
/// user touches min/max.
pub fn default_range(unit: &str) -> (f32, f32) {
    match unit {
        "°F" => (32.0, 212.0),
        "RPM" => (0.0, 3000.0),
        "MHz" => (0.0, 6000.0),
        "h" => (0.0, 24.0),
        _ => (0.0, 100.0),
    }
}

impl Draft {
    pub fn adding() -> Self {
        Self {
            editing: None,
            kind: Kind::Chart,
            sensor_id: String::new(),
            batteries: Vec::new(),
            min: 0.0,
            max: 100.0,
            label: String::new(),
            color: 0,
        }
    }

    pub fn editing(w: &HomeWidget) -> Self {
        let mut draft = Self {
            editing: Some(w.id.clone()),
            label: w.label.clone(),
            color: w.color,
            ..Self::adding()
        };
        match &w.kind {
            HomeWidgetKind::Chart { sensor_id } => {
                draft.kind = Kind::Chart;
                draft.sensor_id = sensor_id.clone();
            }
            HomeWidgetKind::Gauge {
                sensor_id,
                min,
                max,
            } => {
                draft.kind = Kind::Gauge;
                draft.sensor_id = sensor_id.clone();
                draft.min = *min as f32;
                draft.max = *max as f32;
            }
            HomeWidgetKind::Battery { batteries } => {
                draft.kind = Kind::Battery;
                draft.batteries = batteries.clone();
            }
        }
        draft
    }

    pub fn to_widget(&self, id: String) -> HomeWidget {
        let kind = match self.kind {
            Kind::Chart => HomeWidgetKind::Chart {
                sensor_id: self.sensor_id.clone(),
            },
            Kind::Gauge => HomeWidgetKind::Gauge {
                sensor_id: self.sensor_id.clone(),
                min: self.min as f64,
                max: self.max as f64,
            },
            Kind::Battery => HomeWidgetKind::Battery {
                batteries: self.batteries.clone(),
            },
        };
        HomeWidget {
            id,
            kind,
            color: self.color,
            label: self.label.trim().to_string(),
        }
    }

    /// Whether the draft describes a widget that can resolve to something.
    pub fn complete(&self) -> bool {
        match self.kind {
            Kind::Chart => !self.sensor_id.is_empty(),
            Kind::Gauge => !self.sensor_id.is_empty() && self.min < self.max,
            Kind::Battery => !self.batteries.is_empty(),
        }
    }

    /// Adopt the sensor's implied range when the user picks a source for a
    /// gauge that still carries the default one.
    fn pick_sensor(&mut self, id: String, unit: &str) {
        self.sensor_id = id;
        let (min, max) = default_range(unit);
        if self.editing.is_none() {
            self.min = min;
            self.max = max;
        }
    }
}

pub fn show(ctx: &egui::Context, draft: &mut Draft, row: &RowCtx) -> Option<Outcome> {
    let adding = draft.editing.is_none();
    let title = if adding {
        t!("home.widget_add_title")
    } else {
        t!("home.widget_edit_title")
    };
    let mut outcome = None;
    let complete = draft.complete();
    let dismissed = widgets::dialog_with_subtitle(
        ctx,
        "home_widget_config",
        &title,
        &t!("home.widget_add_subtitle"),
        460.0,
        |ui| body(ui, draft, row),
        |ui| {
            let size = egui::vec2(120.0, 36.0);
            let confirm = if adding {
                t!("home.widget_add")
            } else {
                t!("home.save")
            };
            let clicked = if complete {
                widgets::button(ui, &confirm, ButtonKind::Primary, size).clicked()
            } else {
                widgets::button_disabled(ui, &confirm, ButtonKind::Primary, size);
                false
            };
            if clicked {
                outcome = Some(Outcome::Save);
            }
            if widgets::button(ui, &t!("home.cancel"), ButtonKind::Ghost, size).clicked() {
                outcome = Some(Outcome::Close);
            }
        },
    );
    if dismissed {
        return Some(Outcome::Close);
    }
    outcome
}

fn body(ui: &mut egui::Ui, draft: &mut Draft, row: &RowCtx) {
    preview(ui, draft, row);
    ui.add_space(theme::SPACE_9);
    widgets::caps_label(ui, &t!("home.widget_type"));
    ui.add_space(theme::SPACE_5);
    widgets::pill_strip(ui, |ui| {
        for kind in Kind::ALL {
            if widgets::pill(ui, &kind.name(), draft.kind == kind) {
                draft.kind = kind;
            }
        }
    });
    ui.add_space(theme::SPACE_3);
    ui.label(
        egui::RichText::new(draft.kind.description())
            .font(theme::body_sm())
            .color(theme::TEXT_FAINT),
    );

    ui.add_space(theme::SPACE_9);
    match draft.kind {
        Kind::Chart | Kind::Gauge => sensor_picker(ui, draft, row),
        Kind::Battery => battery_picker(ui, draft, row),
    }

    if draft.kind == Kind::Gauge {
        ui.add_space(theme::SPACE_9);
        widgets::caps_label(ui, &t!("home.widget_range"));
        ui.add_space(theme::SPACE_5);
        let half = (ui.available_width() - theme::SPACE_6) / 2.0;
        widgets::split_columns(ui, half, theme::SPACE_6, |left, right| {
            widgets::num_input_row(left, &t!("home.widget_min"), &mut draft.min, -1e6..=1e6);
            widgets::num_input_row(right, &t!("home.widget_max"), &mut draft.max, -1e6..=1e6);
        });
    }

    ui.add_space(theme::SPACE_9);
    widgets::caps_label(ui, &t!("home.widget_label"));
    ui.add_space(theme::SPACE_5);
    let hint = default_label(draft, row);
    ui.add_sized(
        egui::vec2(ui.available_width(), 32.0),
        egui::TextEdit::singleline(&mut draft.label).hint_text(hint),
    );

    ui.add_space(theme::SPACE_9);
    widgets::caps_label(ui, &t!("home.widget_color"));
    ui.add_space(theme::SPACE_5);
    if let Some(picked) =
        widgets::palette_swatch_row(ui, "widget_color", &theme::WIDGET_HUES, draft.color)
    {
        draft.color = picked;
    }
}

/// The tile exactly as the dashboard will draw it, updating as the draft
/// changes — the same painter, so the preview cannot drift from the real thing.
fn preview(ui: &mut egui::Ui, draft: &Draft, row: &RowCtx) {
    let widget = draft.to_widget(PREVIEW_ID.to_string());
    // Same footprint the dashboard gives it, so the preview is to scale.
    let size = widget_view::size(&widget.kind);
    let width = (PREVIEW_W * size.cols as f32 / widget_view::Size::FULL.cols as f32)
        .min(ui.available_width());
    let height = TILE_H * size.rows as f32 + GAP * (size.rows as f32 - 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let resolved = widget_view::resolve(&widget, row.sensors, row.state);
    let p = ui.painter();
    widget_view::paint(p, rect, &widget, resolved.as_ref(), row.history);
    widget_view::frame(p, rect, theme::BORDER);
}

fn default_label(draft: &Draft, row: &RowCtx) -> String {
    match draft.kind {
        Kind::Battery => t!("home.widget_battery").to_string(),
        _ => row
            .sensors
            .iter()
            .find(|s| s.id == draft.sensor_id)
            .map(|s| s.label.clone())
            .unwrap_or_default(),
    }
}

fn sensor_picker(ui: &mut egui::Ui, draft: &mut Draft, row: &RowCtx) {
    widgets::caps_label(ui, &t!("home.widget_sensor"));
    ui.add_space(theme::SPACE_5);
    let options: Vec<(String, String)> = row
        .sensors
        .iter()
        .map(|s| (s.id.clone(), s.label.clone()))
        .collect();
    if let Some(id) =
        widgets::combo_picker_full(ui, "widget_sensor", &options, &draft.sensor_id, None)
    {
        let unit = row
            .sensors
            .iter()
            .find(|s| s.id == id)
            .map_or("", |s| s.unit);
        draft.pick_sensor(id, unit);
    }
}

fn battery_picker(ui: &mut egui::Ui, draft: &mut Draft, row: &RowCtx) {
    widgets::caps_label(
        ui,
        &t!("home.widget_batteries", max = MAX_BATTERY_WIDGET_ENTRIES),
    );
    ui.add_space(theme::SPACE_5);
    let options = widget_view::battery_options(row.state);
    if options.is_empty() {
        ui.label(
            egui::RichText::new(t!("home.widget_no_batteries"))
                .font(theme::body_sm())
                .color(theme::TEXT_FAINT),
        );
        return;
    }
    for (reference, name) in options {
        let mut on = draft.batteries.contains(&reference);
        let full = draft.batteries.len() >= MAX_BATTERY_WIDGET_ENTRIES;
        let toggled = ui
            .add_enabled(on || !full, egui::Checkbox::new(&mut on, name))
            .changed();
        if toggled {
            if on {
                draft.batteries.push(reference);
            } else {
                draft.batteries.retain(|b| b != &reference);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauge(min: f64, max: f64) -> HomeWidget {
        HomeWidget {
            id: "w1".into(),
            kind: HomeWidgetKind::Gauge {
                sensor_id: "cpu".into(),
                min,
                max,
            },
            color: 3,
            label: "Package".into(),
        }
    }

    #[test]
    fn draft_round_trips_a_widget() {
        let w = gauge(20.0, 90.0);
        let back = Draft::editing(&w).to_widget(w.id.clone());
        assert_eq!(back, w);
    }

    #[test]
    fn draft_trims_the_label_override() {
        let mut draft = Draft::adding();
        draft.sensor_id = "cpu".into();
        draft.label = "  ".into();
        assert!(draft.to_widget("w1".into()).label.is_empty());
    }

    #[test]
    fn complete_requires_a_source_and_an_ordered_gauge_range() {
        let mut draft = Draft::adding();
        assert!(!draft.complete());
        draft.sensor_id = "cpu".into();
        assert!(draft.complete());
        draft.kind = Kind::Gauge;
        draft.max = draft.min;
        assert!(!draft.complete());
        draft.max = draft.min + 1.0;
        assert!(draft.complete());
        draft.kind = Kind::Battery;
        assert!(!draft.complete());
        draft.batteries.push("mouse/main".into());
        assert!(draft.complete());
    }

    #[test]
    fn picking_a_sensor_seeds_the_range_only_while_adding() {
        let mut draft = Draft::adding();
        draft.pick_sensor("fan".into(), "RPM");
        assert_eq!((draft.min, draft.max), default_range("RPM"));

        let mut existing = Draft::editing(&gauge(20.0, 90.0));
        existing.pick_sensor("fan".into(), "RPM");
        assert_eq!((existing.min, existing.max), (20.0, 90.0));
    }
}

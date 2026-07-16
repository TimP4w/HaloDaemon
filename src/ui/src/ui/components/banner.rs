// SPDX-License-Identifier: GPL-3.0-or-later
//! Alert/status banner: one builder collapsing the tinted amber/red/neutral
//! `Frame` copies scattered across the plugin and integration screens.

use egui::{Color32, FontId, Sense, Stroke, Vec2};

use super::{button, button_loading, ButtonKind};
use crate::ui::theme;

pub enum BannerKind {
    Neutral,
    Warn,
    Danger,
}

/// `(accent, fill, stroke)` for a banner kind.
pub fn banner_palette(kind: &BannerKind) -> (Color32, Color32, Color32) {
    match kind {
        BannerKind::Neutral => (theme::TEXT, theme::INNER_BG, theme::BORDER),
        BannerKind::Warn => tinted(theme::STAT_AMBER),
        BannerKind::Danger => tinted(theme::TRAFFIC_RED),
    }
}

fn tinted(accent: Color32) -> (Color32, Color32, Color32) {
    (accent, theme::a(accent, 0.10), theme::a(accent, 0.35))
}

/// Header row height: an action's own height, else the title line height.
pub fn banner_row_height(action: Option<Vec2>, title_line_h: f32) -> f32 {
    action.map_or(title_line_h, |s| s.y)
}

pub struct BannerAction<'a> {
    label: &'a str,
    kind: ButtonKind,
    size: Vec2,
    loading: bool,
}

impl<'a> BannerAction<'a> {
    pub fn new(label: &'a str, kind: ButtonKind, size: Vec2) -> Self {
        Self {
            label,
            kind,
            size,
            loading: false,
        }
    }

    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
}

pub struct Banner<'a> {
    kind: BannerKind,
    title: &'a str,
    title_color: Option<Color32>,
    title_font: Option<FontId>,
    subtitle: Option<&'a str>,
    dot: Option<Color32>,
    action: Option<BannerAction<'a>>,
}

impl<'a> Banner<'a> {
    pub fn neutral(title: &'a str) -> Self {
        Self::new(BannerKind::Neutral, title)
    }
    pub fn warn(title: &'a str) -> Self {
        Self::new(BannerKind::Warn, title)
    }
    pub fn danger(title: &'a str) -> Self {
        Self::new(BannerKind::Danger, title)
    }

    fn new(kind: BannerKind, title: &'a str) -> Self {
        Self {
            kind,
            title,
            title_color: None,
            title_font: None,
            subtitle: None,
            dot: None,
            action: None,
        }
    }

    pub fn title_color(mut self, color: Color32) -> Self {
        self.title_color = Some(color);
        self
    }
    pub fn title_font(mut self, font: FontId) -> Self {
        self.title_font = Some(font);
        self
    }
    pub fn subtitle(mut self, subtitle: &'a str) -> Self {
        self.subtitle = Some(subtitle);
        self
    }
    pub fn dot(mut self, color: Color32) -> Self {
        self.dot = Some(color);
        self
    }
    pub fn action(mut self, action: BannerAction<'a>) -> Self {
        self.action = Some(action);
        self
    }

    /// Render; returns `true` when a non-loading action was clicked.
    pub fn show(self, ui: &mut egui::Ui) -> bool {
        self.render(ui, |_| {}).1
    }

    /// Render the header, then `body` inside the same frame.
    pub fn show_with<R>(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
        self.render(ui, body).0
    }

    /// [`Self::show_with`] for banners that carry both a body and an action;
    /// returns whether the action was clicked.
    pub fn show_action_with(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) -> bool {
        self.render(ui, body).1
    }

    fn render<R>(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> (R, bool) {
        let (accent, _, _) = banner_palette(&self.kind);
        let title_color = self.title_color.unwrap_or(accent);
        let title_font = self.title_font.clone().unwrap_or_else(theme::subhead);
        banner_frame(ui, &self.kind, |ui| {
            ui.set_width(ui.available_width());
            let action_size = self.action.as_ref().map(|a| a.size);
            let title_line_h = ui
                .painter()
                .layout_no_wrap(self.title.to_owned(), title_font.clone(), title_color)
                .size()
                .y;
            let row_h = banner_row_height(action_size, title_line_h);
            let mut clicked = false;
            egui::Sides::new().height(row_h).show(
                ui,
                |ui| {
                    if let Some(dot) = self.dot {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, dot);
                    }
                    ui.label(
                        egui::RichText::new(self.title)
                            .font(title_font.clone())
                            .color(title_color),
                    );
                },
                |ui| {
                    if let Some(a) = &self.action {
                        if a.loading {
                            button_loading(ui, a.label, a.kind, a.size);
                        } else if button(ui, a.label, a.kind, a.size).clicked() {
                            clicked = true;
                        }
                    }
                },
            );
            if let Some(sub) = self.subtitle {
                ui.add_space(theme::SPACE_1);
                ui.label(
                    egui::RichText::new(sub)
                        .font(theme::body_sm())
                        .color(theme::TEXT_DIM),
                );
            }
            (body(ui), clicked)
        })
    }
}

/// A fully-custom banner shell (frame + tint) whose interior is entirely
/// `body`'s to paint.
pub fn banner_frame<R>(
    ui: &mut egui::Ui,
    kind: &BannerKind,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let (_, fill, stroke) = banner_palette(kind);
    egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(theme::PAD_BANNER)
        .show(ui, body)
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_tints_match_and_are_ordered() {
        assert_eq!(
            banner_palette(&BannerKind::Neutral),
            (theme::TEXT, theme::INNER_BG, theme::BORDER)
        );
        for kind in [BannerKind::Warn, BannerKind::Danger] {
            let accent = match kind {
                BannerKind::Warn => theme::STAT_AMBER,
                BannerKind::Danger => theme::TRAFFIC_RED,
                BannerKind::Neutral => unreachable!(),
            };
            let (a, fill, stroke) = banner_palette(&kind);
            assert_eq!((a, fill, stroke), tinted(accent));
            assert!(fill.a() < stroke.a());
        }
    }

    #[test]
    fn row_height_prefers_action_else_title() {
        assert_eq!(banner_row_height(Some(Vec2::new(120.0, 32.0)), 99.0), 32.0);
        assert_eq!(banner_row_height(None, 17.0), 17.0);
    }

    #[test]
    fn renders_title_and_subtitle_within_frame() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let mut h = 0.0;
        let _ = ctx.run_ui(input, |ui| {
            h = ui
                .vertical(|ui| {
                    Banner::warn("x").subtitle("y").show(ui);
                })
                .response
                .rect
                .height();
        });
        assert!(h >= 2.0 * 11.0);
    }
}

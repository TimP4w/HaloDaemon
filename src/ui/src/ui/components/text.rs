// SPDX-License-Identifier: GPL-3.0-or-later
//! Text field, back-link, value row, and empty-state placeholder.

use egui::{Align2, Color32, Pos2, Sense, Stroke, Vec2};

use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::ui::theme;

/// A text input with comfortable default padding and font — use this instead of
/// raw `egui::TextEdit::singleline()` for every free-text field.
pub fn text_field(
    ui: &mut egui::Ui,
    buf: &mut String,
    hint: &str,
    desired_width: f32,
) -> egui::Response {
    text_field_ex(ui, buf, hint, desired_width, false)
}

pub fn text_field_ex(
    ui: &mut egui::Ui,
    buf: &mut String,
    hint: &str,
    desired_width: f32,
    password: bool,
) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(buf)
            .hint_text(hint)
            .desired_width(desired_width)
            .margin(theme::PAD_FIELD)
            .font(theme::body(14.0))
            .password(password),
    )
}

/// A search input with a leading magnifier, `width` wide and 36 px tall.
pub fn search_field(ui: &mut egui::Ui, buf: &mut String, hint: &str, width: f32) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::hover());
    let focused = ui
        .memory(|m| m.focused())
        .is_some_and(|id| id == ui.make_persistent_id(("search", hint)));
    theme::paint_surface(
        ui.painter(),
        rect,
        theme::RADIUS_SM,
        theme::INNER_BG,
        if focused {
            theme::CYAN
        } else {
            theme::BORDER_INNER
        },
    );
    let icon = Pos2::new(rect.left() + 18.0, rect.center().y - 1.0);
    let stroke = Stroke::new(1.4, theme::TEXT_MUT);
    ui.painter().circle_stroke(icon, 4.2, stroke);
    ui.painter()
        .line_segment([icon + Vec2::splat(3.2), icon + Vec2::splat(6.2)], stroke);
    let field = egui::Rect::from_min_max(
        Pos2::new(rect.left() + 30.0, rect.top()),
        Pos2::new(rect.right() - 8.0, rect.bottom()),
    );
    ui.new_child(
        egui::UiBuilder::new()
            .max_rect(field)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    )
    .add(
        egui::TextEdit::singleline(buf)
            .id(ui.make_persistent_id(("search", hint)))
            .frame(egui::Frame::NONE)
            .desired_width(f32::INFINITY)
            .font(theme::body_md())
            .hint_text(hint),
    )
}

/// A "‹ label" link for the top of a sub-page, navigating back to the parent
/// page. Shared by every drill-down page (device detail, Effect Designer, …).
pub fn back_link(ui: &mut egui::Ui, label: &str, anchor: Option<AnchorId>) -> bool {
    // Draw a vector chevron (<) instead of a Unicode arrow so the glyph is
    // guaranteed to render (InterTight doesn't cover U+2190).
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::body_md(), theme::TEXT_DIM);
    let chevron_w = 10.0;
    let gap = 6.0;
    let total_size = Vec2::new(chevron_w + gap + galley.size().x, galley.size().y + 6.0);
    let (rect, resp) = ui.allocate_exact_size(total_size, Sense::click());
    if let Some(anchor) = anchor {
        tour::anchor(ui.ctx(), anchor, rect);
    }
    let p = ui.painter();
    let cy = rect.center().y + 0.5;
    let s = Stroke::new(1.4, theme::TEXT_DIM);
    let tip = Pos2::new(rect.left() + 1.0, cy);
    p.line_segment([Pos2::new(rect.left() + chevron_w * 0.6, cy - 3.5), tip], s);
    p.line_segment([tip, Pos2::new(rect.left() + chevron_w * 0.6, cy + 3.5)], s);
    p.galley(
        Pos2::new(rect.left() + chevron_w + gap, rect.top() + 3.0),
        galley,
        theme::TEXT_DIM,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.add_space(theme::SPACE_6);
    resp.clicked()
}

/// Truncate `text` with a trailing ellipsis until `measure` (the painted width
/// of a candidate string) reports it fits `max_w`. `measure` is injected so
/// this stays testable without an egui context.
pub fn truncate_to_width(text: &str, max_w: f32, measure: impl Fn(&str) -> f32) -> String {
    if text.is_empty() || measure(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for end in (0..chars.len()).rev() {
        let candidate: String = chars[..end].iter().collect::<String>() + "…";
        if measure(&candidate) <= max_w {
            return candidate;
        }
    }
    "…".to_string()
}

pub fn text_link(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let size = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::body_md(), theme::LINK)
        .size();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let color = if resp.hovered() {
        theme::ACCENT_TEXT
    } else {
        theme::LINK
    };
    ui.painter()
        .text(rect.min, Align2::LEFT_TOP, label, theme::body_md(), color);
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, color));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A definition-list row: muted key in a fixed 180 px column, value
/// left-aligned beside it, hairline below (device Info details).
pub fn detail_row(ui: &mut egui::Ui, key: &str, value: &str) {
    const KEY_W: f32 = 180.0;
    let w = ui.available_width();
    let galley = ui.painter().layout(
        value.to_string(),
        theme::body_lg(),
        theme::TEXT,
        (w - KEY_W).max(40.0),
    );
    let h = galley.size().y.max(18.0) + 24.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let p = ui.painter();
    p.text(
        Pos2::new(rect.left(), rect.top() + 12.0),
        Align2::LEFT_TOP,
        key,
        theme::body_lg(),
        theme::TEXT_DIM,
    );
    p.galley(
        Pos2::new(rect.left() + KEY_W, rect.top() + 12.0),
        galley,
        theme::TEXT,
    );
    p.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, theme::BORDER_SOFT),
    );
}

/// A label/value table row with a bottom hairline (Info rows, cooling readings).
/// Long values are truncated with an ellipsis so they never overflow the card,
/// and the full value is shown on hover.
/// Muted field label above a control, with the standard small gap.
pub fn field_label(ui: &mut egui::Ui, label: &str) {
    ui.label(
        egui::RichText::new(label)
            .font(theme::body_md())
            .color(theme::TEXT_DIM),
    );
    ui.add_space(theme::SPACE_3);
}

/// A 72 px list row with a divider above it (except the first in its parent):
/// title and subtitle left, `control` laid right-to-left on the right.
pub fn list_row<R>(
    ui: &mut egui::Ui,
    title: &str,
    sub: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let first = ui.min_rect().height() <= 0.5;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
    if !first {
        ui.painter()
            .hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::DIVIDER));
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(0.0, 12.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let out = control(&mut child);
    let text_right = child.min_rect().left() - 16.0;
    let clip = egui::Rect::from_min_max(
        rect.min,
        Pos2::new(text_right.max(rect.left()), rect.bottom()),
    );
    let p = ui.painter().with_clip_rect(clip);
    p.text(
        Pos2::new(rect.left(), rect.center().y - 10.0),
        Align2::LEFT_CENTER,
        title,
        theme::medium(14.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(rect.left(), rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        sub,
        theme::body_md(),
        theme::TEXT_DIM,
    );
    out
}

/// "Title + muted description on the left, control on the right" — the
/// standard setting-row shape shared by preference and toggle rows. Returns
/// the control closure's result.
pub fn setting_row<R>(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width() - 60.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::title())
                        .color(theme::TEXT),
                );
                ui.add_space(theme::SPACE_2);
                if !desc.is_empty() {
                    ui.label(
                        egui::RichText::new(desc)
                            .font(theme::body_md())
                            .color(theme::TEXT_MUT),
                    );
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
    .inner
}

pub fn value_row(ui: &mut egui::Ui, label: &str, value: &str, value_color: Color32) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let label_font = theme::body_md();
    let value_font = theme::value();
    let p = ui.painter();
    let label_w = p
        .layout_no_wrap(label.to_string(), label_font.clone(), theme::TEXT_MUT)
        .rect
        .width();
    // Leave a gap between the label and value so they never touch.
    let value_max_w = (rect.width() - label_w - 12.0).max(0.0);
    let shown = truncate_to_width(value, value_max_w, |s| {
        p.layout_no_wrap(s.to_string(), value_font.clone(), value_color)
            .rect
            .width()
    });
    p.text(
        Pos2::new(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        label_font,
        theme::TEXT_MUT,
    );
    p.text(
        Pos2::new(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        &shown,
        value_font,
        value_color,
    );
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );
    if shown != value {
        resp.on_hover_text(value);
    }
}

/// A centered empty-state panel — a dimmed title with an optional fainter
/// subtitle — matching the Home page's "no devices" placeholder. Shared by every
/// page so empty views read the same everywhere instead of a stray top-left line.
pub fn empty_state(ui: &mut egui::Ui, title: &str, subtitle: Option<&str>) {
    ui.add_space(theme::SPACE_16);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(title)
                .font(theme::title())
                .color(theme::TEXT_DIM),
        );
        if let Some(sub) = subtitle {
            ui.add_space(theme::SPACE_2);
            ui.label(
                egui::RichText::new(sub)
                    .font(theme::body_md())
                    .color(theme::TEXT_FAINT),
            );
        }
    });
    ui.add_space(theme::SPACE_16);
}

#[cfg(test)]
mod tests {
    use super::{list_row, truncate_to_width};
    use crate::ui::theme;

    fn list_divider_count(rows: usize) -> usize {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(700.0, 900.0),
            )),
            ..Default::default()
        };
        let out = crate::ui::run_headless(&ctx, input, |ui| {
            crate::ui::components::card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for _ in 0..rows {
                    list_row(ui, "", "", |_| ());
                }
            });
        });
        out.shapes
            .iter()
            .filter(|s| matches!(s.shape, egui::Shape::LineSegment { .. }))
            .count()
    }

    #[test]
    fn list_rows_draw_no_trailing_divider() {
        assert_eq!(list_divider_count(1), 0);
        assert_eq!(list_divider_count(3), 2);
        assert_eq!(list_divider_count(7), 6);
    }

    #[test]
    fn truncate_to_width_keeps_short_text_untouched() {
        let measure = |s: &str| s.chars().count() as f32 * 10.0;
        assert_eq!(truncate_to_width("short", 100.0, measure), "short");
        assert_eq!(truncate_to_width("", 5.0, measure), "");
    }

    #[test]
    fn truncate_to_width_shortens_with_ellipsis_until_it_fits() {
        // 10px/char; "hello world" is 110px and must shrink to fit 55px,
        // i.e. at most 5 chars including the trailing ellipsis.
        let measure = |s: &str| s.chars().count() as f32 * 10.0;
        let out = truncate_to_width("hello world", 55.0, measure);
        assert_eq!(out, "hell…");
        assert!(measure(&out) <= 55.0);
    }

    #[test]
    fn truncate_to_width_falls_back_to_ellipsis_when_nothing_fits() {
        let measure = |_: &str| 1000.0;
        assert_eq!(truncate_to_width("anything", 5.0, measure), "…");
    }
}

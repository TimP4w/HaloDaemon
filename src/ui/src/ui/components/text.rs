// SPDX-License-Identifier: GPL-3.0-or-later
//! Text field, back-link, value row, and empty-state placeholder.

use egui::{Align2, Color32, Pos2, Sense, Stroke, Vec2};

use crate::ui::theme;

/// A text input with comfortable default padding and font — use this instead of
/// raw `egui::TextEdit::singleline()` for every free-text field.
pub fn text_field(
    ui: &mut egui::Ui,
    buf: &mut String,
    hint: &str,
    desired_width: f32,
) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(buf)
            .hint_text(hint)
            .desired_width(desired_width)
            .margin(egui::vec2(12.0, 9.0))
            .font(theme::body(14.0)),
    )
}

/// A "‹ label" link for the top of a sub-page, navigating back to the parent
/// page. Shared by every drill-down page (device detail, Effect Designer, …).
pub fn back_link(ui: &mut egui::Ui, label: &str) -> bool {
    // Draw a vector chevron (<) instead of a Unicode arrow so the glyph is
    // guaranteed to render (InterTight doesn't cover U+2190).
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::body(11.5), theme::TEXT_MUT);
    let chevron_w = 10.0;
    let gap = 6.0;
    let total_size = Vec2::new(chevron_w + gap + galley.size().x, galley.size().y + 6.0);
    let (rect, resp) = ui.allocate_exact_size(total_size, Sense::click());
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::DeviceBackLink,
        rect,
    );
    let p = ui.painter();
    let cy = rect.center().y + 0.5;
    let s = Stroke::new(1.4, theme::TEXT_MUT);
    let tip = Pos2::new(rect.left() + 1.0, cy);
    p.line_segment([Pos2::new(rect.left() + chevron_w * 0.6, cy - 3.5), tip], s);
    p.line_segment([tip, Pos2::new(rect.left() + chevron_w * 0.6, cy + 3.5)], s);
    p.galley(
        Pos2::new(rect.left() + chevron_w + gap, rect.top() + 3.0),
        galley,
        theme::TEXT_MUT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.add_space(12.0);
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

/// A label/value table row with a bottom hairline (Info rows, cooling readings).
/// Long values are truncated with an ellipsis so they never overflow the card,
/// and the full value is shown on hover.
pub fn value_row(ui: &mut egui::Ui, label: &str, value: &str, value_color: Color32) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let label_font = theme::body(12.0);
    let value_font = theme::mono_semibold(12.0);
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
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(title)
                .font(theme::semibold(15.0))
                .color(theme::TEXT_DIM),
        );
        if let Some(sub) = subtitle {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(sub)
                    .font(theme::body(12.0))
                    .color(theme::TEXT_FAINT),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::truncate_to_width;

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

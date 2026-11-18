// SPDX-License-Identifier: GPL-3.0-or-later

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};

use crate::ui::theme;

#[derive(Debug, Clone, PartialEq)]
pub enum SelectRow {
    Header(String),
    Divider,
    Item(SelectItem),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub id: String,
    pub label: String,
    pub meta: String,
    pub selected: bool,
    pub enabled: bool,
}

const MENU_WIDTH: f32 = 280.0;
const MENU_MAX_HEIGHT: f32 = 420.0;
const ITEM_HEIGHT: f32 = 34.0;
const HEADER_HEIGHT: f32 = 26.0;
const ROW_HOVER: egui::Color32 = theme::hex(0x262b33);
const ROW_SELECTED: egui::Color32 = theme::hex(0x1f232a);
const TEXT_DISABLED: egui::Color32 = theme::hex(0x6b727c);

pub fn grouped_select(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    selected_text: &str,
    width: f32,
    rows: &[SelectRow],
) -> Option<String> {
    let mut pick = None;
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(selected_text)
        .icon(super::select_chevron)
        .width(width)
        .height(MENU_MAX_HEIGHT)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            ui.set_min_width(MENU_WIDTH.max(width));
            ui.spacing_mut().item_spacing.y = 1.0;
            for row in rows {
                match row {
                    SelectRow::Header(label) => header_row(ui, label),
                    SelectRow::Divider => divider_row(ui),
                    SelectRow::Item(item) => {
                        if item_row(ui, item).clicked() {
                            pick = Some(item.id.clone());
                            ui.close();
                        }
                    }
                }
            }
        });
    pick
}

fn header_row(ui: &mut egui::Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), HEADER_HEIGHT),
        Sense::hover(),
    );
    ui.painter().text(
        Pos2::new(rect.left() + 10.0, rect.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        label.to_uppercase(),
        theme::medium(11.0),
        theme::TEXT_MUT,
    );
}

fn divider_row(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 11.0), Sense::hover());
    ui.painter().hline(
        (rect.left() + 4.0)..=(rect.right() - 4.0),
        rect.center().y,
        Stroke::new(1.0, theme::TRACK),
    );
}

fn item_row(ui: &mut egui::Ui, item: &SelectItem) -> egui::Response {
    let sense = if item.enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ITEM_HEIGHT), sense);
    let hovered = item.enabled && resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fill = if hovered {
        ROW_HOVER
    } else if item.selected {
        ROW_SELECTED
    } else {
        egui::Color32::TRANSPARENT
    };
    let p = ui.painter();
    p.rect_filled(rect, 6.0, fill);
    if item.selected {
        paint_check(p, Pos2::new(rect.left() + 17.0, rect.center().y));
    }
    let meta_w = if item.meta.is_empty() {
        0.0
    } else {
        p.text(
            Pos2::new(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            &item.meta,
            theme::body_sm(),
            theme::TEXT_MUT,
        )
        .width()
            + 10.0
    };
    let text_left = rect.left() + 34.0;
    let clip = Rect::from_min_max(
        Pos2::new(text_left, rect.top()),
        Pos2::new(rect.right() - 10.0 - meta_w, rect.bottom()),
    );
    p.with_clip_rect(clip).text(
        Pos2::new(text_left, rect.center().y),
        Align2::LEFT_CENTER,
        &item.label,
        theme::body_md(),
        if item.enabled {
            theme::TEXT
        } else {
            TEXT_DISABLED
        },
    );
    resp
}

fn paint_check(p: &egui::Painter, c: Pos2) {
    p.add(egui::Shape::line(
        vec![
            c + Vec2::new(-3.5, 0.3),
            c + Vec2::new(-1.0, 2.7),
            c + Vec2::new(3.5, -2.5),
        ],
        Stroke::new(1.6, theme::LINK),
    ));
}

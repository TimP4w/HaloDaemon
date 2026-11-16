// SPDX-License-Identifier: GPL-3.0-or-later
//! Profile button (title-bar), dropdown, profile-settings page, and
//! multi-select process-picker modal.

use crate::ui::components as widgets;
use std::collections::HashSet;

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Color32, Id, Order, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::capability;
use halod_shared::types::{Notification, RunningApp, DEFAULT_PROFILE_NAME};

use crate::domain::state::Page;
use crate::runtime::app_icons::{get_icon, IconCache};
use crate::runtime::ipc::CommandTx;
use crate::ui::components::ButtonKind;
use crate::ui::theme::{self, a};

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ProfileUi {
    pub dropdown_open: bool,
    pub add_modal_open: bool,
    pub add_name_buf: String,
    pub picking_for: Option<String>,
    pub pick_filter: String,
    pub pick_selected: HashSet<String>,
    pub settings_name_buf: String,
    pub renaming: bool,
    /// Profile a delete is pending confirmation for; drives the confirm dialog.
    pub confirm_delete: Option<String>,
    /// Last profile the settings page was seeded for (detect profile changes).
    pub settings_seeded_for: String,
    /// Icon cache, populated off-thread.
    pub icon_cache: IconCache,
    /// Profile a switch is pending for; cleared on its "Profile switched" notification.
    pub switching_to: Option<String>,
}

impl ProfileUi {
    pub fn release_textures(&mut self) {
        if let Ok(mut cache) = self.icon_cache.lock() {
            cache.clear();
        }
    }
}

pub fn observe_notifications(st: &mut ProfileUi, notifications: &[Notification]) {
    use halod_shared::types::NotificationCode;
    if notifications
        .iter()
        .any(|n| matches!(n.code, NotificationCode::ProfileSwitched { .. }))
    {
        st.switching_to = None;
    }
}

/// Translated capability label, keyed on the stable capability *state key* the
/// daemon ships (`capability::RGB` etc.) — never on English. Unknown keys
/// (capabilities without a curated label) fall back to title-casing the key.
fn capability_label(state_key: &str) -> std::borrow::Cow<'static, str> {
    match state_key {
        capability::RGB => t!("capability.rgb"),
        capability::DPI => t!("capability.dpi"),
        capability::FAN_CURVE => t!("capability.fan_curve"),
        capability::LCD => t!("capability.lcd"),
        capability::KEY_REMAP => t!("capability.key_remap"),
        capability::EQUALIZER => t!("capability.equalizer"),
        capability::CHOICE => t!("capability.choice"),
        capability::RANGE => t!("capability.range"),
        capability::BOOLEAN => t!("capability.boolean"),
        other => std::borrow::Cow::Owned(title_case_key(other)),
    }
}

/// Title-case an underscore-separated state key (`button_mapping` → `Button
/// Mapping`) for capabilities that have no curated translation.
fn title_case_key(key: &str) -> String {
    let mut s = String::with_capacity(key.len());
    for (i, word) in key.split('_').enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            s.extend(first.to_uppercase());
            s.push_str(chars.as_str());
        }
    }
    s
}

fn scalar_override_value(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => Some("—".to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn override_diff_lines(default: &serde_json::Value, value: &serde_json::Value) -> Vec<String> {
    let (Some(d), Some(v)) = (default.as_object(), value.as_object()) else {
        return Vec::new();
    };
    let keys: std::collections::BTreeSet<&String> = d.keys().chain(v.keys()).collect();
    keys.into_iter()
        .filter(|k| d.get(*k) != v.get(*k))
        .filter_map(|k| {
            let from = scalar_override_value(d.get(k).unwrap_or(&serde_json::Value::Null))?;
            let to = scalar_override_value(v.get(k).unwrap_or(&serde_json::Value::Null))?;
            Some(format!("{}: {from} → {to}", title_case_key(k)))
        })
        .collect()
}

/// Draw a process icon at `rect`. Falls back to a letter chip if no icon is available.
fn draw_process_icon(
    ui: &egui::Ui,
    rect: egui::Rect,
    icon_name: &str,
    display_name: &str,
    cache: &IconCache,
) {
    if let Some(tex) = get_icon(cache, ui.ctx(), icon_name) {
        let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
        ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
    } else {
        // Fallback: colored letter chip
        let letter = display_name
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .next()
            .unwrap_or('?');
        ui.painter()
            .rect_filled(rect, rect.height() * 0.25, theme::TRACK);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            letter.to_string(),
            theme::semibold(rect.height() * 0.55),
            theme::TEXT_DIM,
        );
    }
}

// ── Title-bar profile button ──────────────────────────────────────────────────

/// Draw the profile pill button inside the title bar painter area.
/// Returns the rect it occupies (needed for dropdown positioning).
pub fn title_button(
    ui: &mut egui::Ui,
    state: &TopicStore,
    st: &mut ProfileUi,
    right_x: f32,
    cy: f32,
) -> Rect {
    let switching = st.switching_to.is_some();

    let p = ui.painter();
    let profile_name = if state.profiles.active.is_empty() {
        "default"
    } else {
        &state.profiles.active
    };

    let caption = t!("profile.title_caption");
    let caption_width = p
        .layout_no_wrap(caption.to_string(), theme::body_md(), Color32::WHITE)
        .size()
        .x;
    let name_width = p
        .layout_no_wrap(
            profile_name.to_string(),
            theme::semibold(13.0),
            Color32::WHITE,
        )
        .size()
        .x;
    let pill_w = 12.0 + caption_width + 10.0 + name_width + 10.0 + 12.0 + 12.0;
    let pill_h = 32.0;
    let pill_rect = Rect::from_min_size(
        Pos2::new(right_x - pill_w, cy - pill_h / 2.0),
        Vec2::new(pill_w, pill_h),
    );

    let resp = ui.interact(pill_rect, Id::unique("profile_btn"), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let bg = if hovered || st.dropdown_open {
        theme::ROW_ACTIVE
    } else {
        theme::RAISED_BG
    };
    p.rect_filled(pill_rect, theme::RADIUS_SM, bg);
    p.rect_stroke(
        pill_rect,
        theme::RADIUS_SM,
        Stroke::new(1.0, theme::BORDER_INNER),
        egui::StrokeKind::Middle,
    );

    let mut x = pill_rect.left() + 12.0;
    p.text(
        Pos2::new(x, cy),
        Align2::LEFT_CENTER,
        caption,
        theme::body_md(),
        theme::TEXT_DIM,
    );
    x += caption_width + 10.0;
    p.text(
        Pos2::new(x, cy),
        Align2::LEFT_CENTER,
        profile_name,
        theme::semibold(13.0),
        theme::TEXT,
    );
    x += name_width + 10.0;
    if switching {
        let spin_rect = Rect::from_center_size(Pos2::new(x + 6.0, cy), Vec2::splat(10.0));
        egui::Spinner::new()
            .size(10.0)
            .color(theme::TEXT_FAINT)
            .paint_at(ui, spin_rect);
    } else {
        crate::ui::components::paint_chevron(
            p,
            Pos2::new(x + 6.0, cy),
            st.dropdown_open,
            theme::TEXT_DIM,
        );
    }

    if resp.clicked() {
        st.dropdown_open = !st.dropdown_open;
    }

    pill_rect
}

// ── Dropdown ─────────────────────────────────────────────────────────────────

/// Render the profile dropdown as a Foreground Area below `btn_rect`.
/// Must be called every frame while open; handles close-on-outside-click.
pub fn title_dropdown(
    ctx: &egui::Context,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut ProfileUi,
    page: &mut Page,
    btn_rect: Rect,
) {
    if !st.dropdown_open {
        return;
    }

    let drop_w = 280.0;
    let is_switching = st.switching_to.is_some();

    let mut switch_to: Option<String> = None;
    let mut navigate_settings: Option<String> = None;
    let mut remove: Option<String> = None;
    let mut open_add = false;
    let mut nav_to_settings_page = false;

    let frame = egui::Frame::NONE
        .fill(theme::MODAL_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_LG)
        .inner_margin(egui::Margin::same(8))
        .shadow(egui::epaint::Shadow {
            offset: [0, 18],
            blur: 50,
            spread: 0,
            color: a(Color32::BLACK, 0.55),
        });

    // Anchored below the button, right edges aligned. `open_bool` +
    // `CloseOnClickOutside` lets egui own the dismissal — no manual hit-testing.
    egui::Popup::new(
        Id::unique("profile_dropdown"),
        ctx.clone(),
        btn_rect,
        egui::LayerId::new(Order::Foreground, Id::unique("profile_dropdown")),
    )
    .align(egui::RectAlign::BOTTOM_END)
    .gap(6.0)
    .width(drop_w)
    .frame(frame)
    .open_bool(&mut st.dropdown_open)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| {
        ui.set_width(drop_w - 16.0);

        let active = if state.profiles.active.is_empty() {
            DEFAULT_PROFILE_NAME
        } else {
            &state.profiles.active
        };

        for profile in &state.profiles.available {
            let is_active = profile == active;
            let is_default = profile == DEFAULT_PROFILE_NAME;
            let is_switch_target = st.switching_to.as_deref() == Some(profile.as_str());

            let (row_rect, row_resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
            let hovered = row_resp.hovered() && !is_switching;

            if hovered {
                ui.painter()
                    .rect_filled(row_rect, 8.0, a(Color32::WHITE, 0.04));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            let dot_x = row_rect.left() + 14.0;
            let dot_y = row_rect.center().y;
            if is_switch_target {
                let spin_rect = Rect::from_center_size(Pos2::new(dot_x, dot_y), Vec2::splat(10.0));
                egui::Spinner::new()
                    .size(10.0)
                    .color(theme::CYAN)
                    .paint_at(ui, spin_rect);
            } else if is_active {
                theme::glow(ui.painter(), Pos2::new(dot_x, dot_y), 5.0, theme::CYAN, 0.5);
                ui.painter()
                    .circle_filled(Pos2::new(dot_x, dot_y), 3.5, theme::CYAN);
            } else {
                ui.painter().circle_stroke(
                    Pos2::new(dot_x, dot_y),
                    3.5,
                    Stroke::new(1.0, theme::TEXT_FAINT2),
                );
            }

            let name_col = if is_active {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            };
            let name_font = if is_active {
                theme::heading()
            } else {
                theme::body_lg()
            };
            ui.painter().text(
                Pos2::new(dot_x + 14.0, dot_y),
                Align2::LEFT_CENTER,
                profile,
                name_font,
                name_col,
            );

            // Right-side action icons (⚙ and ×)
            let icon_y = row_rect.center().y;
            let gear_x = row_rect.right() - 14.0;
            let del_x = if !is_default {
                row_rect.right() - 36.0
            } else {
                row_rect.right() - 14.0
            };

            // Gear → navigate to profile settings page
            let gear_rect = Rect::from_center_size(Pos2::new(gear_x, icon_y), Vec2::splat(20.0));
            let gear = |p: &egui::Painter, r: Rect, c: Color32| {
                p.text(r.center(), Align2::CENTER_CENTER, "⚙", theme::body(16.0), c);
            };
            if widgets::glyph_button_at(ui, gear_rect, Id::unique(("pdrop_gear", profile)), gear)
                .clicked()
            {
                navigate_settings = Some(profile.clone());
            }

            let del_rect = Rect::from_center_size(Pos2::new(del_x, icon_y), Vec2::splat(20.0));
            if !is_default
                && widgets::glyph_button_at(
                    ui,
                    del_rect,
                    Id::unique(("pdrop_del", profile)),
                    widgets::paint_close_glyph,
                )
                .clicked()
            {
                remove = Some(profile.clone());
            }

            // Click row body (left of icons) → switch to profile
            let body_rect =
                Rect::from_min_max(row_rect.min, Pos2::new(del_x - 6.0, row_rect.max.y));
            let body_resp = ui.interact(
                body_rect,
                Id::unique(("pdrop_row", profile)),
                Sense::click(),
            );
            if body_resp.clicked() && !is_active && !is_switching {
                switch_to = Some(profile.clone());
            }
        }

        ui.add_space(theme::SPACE_2);
        ui.painter().line_segment(
            [
                Pos2::new(ui.max_rect().left(), ui.cursor().top()),
                Pos2::new(ui.max_rect().right(), ui.cursor().top()),
            ],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.add_space(theme::SPACE_2);

        // Bottom actions row
        ui.horizontal(|ui| {
            let add_w = (drop_w - 16.0 - 8.0) / 2.0;
            if widgets::button(
                ui,
                &t!("profile.new_profile_btn"),
                widgets::ButtonKind::Secondary,
                Vec2::new(add_w, 34.0),
            )
            .clicked()
            {
                open_add = true;
            }
            ui.add_space(theme::SPACE_4);
            if widgets::button(
                ui,
                &t!("profile.profile_settings_btn"),
                widgets::ButtonKind::Secondary,
                Vec2::new(add_w, 34.0),
            )
            .clicked()
            {
                nav_to_settings_page = true;
            }
        });
    });

    // Apply deferred actions.
    if let Some(name) = switch_to {
        st.switching_to = Some(name.clone());
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::SwitchProfile { name },
        );
    }
    if let Some(name) = remove {
        // Defer the actual delete to a confirm dialog; close the dropdown so the
        // modal isn't obscured by it.
        st.confirm_delete = Some(name);
        st.dropdown_open = false;
    }
    if let Some(name) = navigate_settings {
        *page = Page::Profile(name);
        st.dropdown_open = false;
    }
    if open_add {
        st.add_modal_open = true;
        st.add_name_buf.clear();
        st.dropdown_open = false;
    }
    if nav_to_settings_page {
        let active = if state.profiles.active.is_empty() {
            DEFAULT_PROFILE_NAME.to_string()
        } else {
            state.profiles.active.clone()
        };
        *page = Page::Profile(active);
        st.dropdown_open = false;
    }
}

// ── "New profile" modal ───────────────────────────────────────────────────────

/// A trimmed name is usable only if non-empty and not already taken
/// (case-insensitive), so create/rename never silently clobbers a profile.
fn name_available(state: &TopicStore, trimmed: &str) -> bool {
    !trimmed.is_empty()
        && !state
            .profiles
            .available
            .iter()
            .any(|p| p.eq_ignore_ascii_case(trimmed))
}

pub fn add_modal(ctx: &egui::Context, state: &TopicStore, cmd: &CommandTx, st: &mut ProfileUi) {
    if !st.add_modal_open {
        return;
    }

    let mut enter_submit = false;
    let mut create = false;
    let mut cancel = false;

    let name_ok = name_available(state, st.add_name_buf.trim());
    let closed = widgets::dialog(
        ctx,
        "add_profile",
        &t!("profile.new_profile_title"),
        380.0,
        |ui| {
            let te = egui::TextEdit::singleline(&mut st.add_name_buf)
                .desired_width(f32::INFINITY)
                .hint_text(t!("profile.profile_name_hint").to_string())
                .margin(egui::vec2(12.0, 11.0))
                .font(theme::body_lg());
            let te_resp = ui.add(te);
            te_resp.request_focus();

            // Enter key submits
            if te_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && name_ok {
                enter_submit = true;
            }

            if !name_ok && !st.add_name_buf.trim().is_empty() {
                ui.add_space(theme::SPACE_3);
                ui.label(
                    egui::RichText::new(t!("profile.name_taken"))
                        .font(theme::body_sm())
                        .color(theme::TRAFFIC_RED),
                );
            }
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("profile.create"),
                ButtonKind::Primary,
                egui::vec2(100.0, 34.0),
            )
            .clicked()
                && name_ok
            {
                create = true;
            }
            ui.add_space(theme::SPACE_4);
            if widgets::button(
                ui,
                &t!("profile.cancel"),
                ButtonKind::Secondary,
                egui::vec2(80.0, 34.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );

    if (create || enter_submit) && name_available(state, st.add_name_buf.trim()) {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::AddProfile {
                name: st.add_name_buf.trim().to_string(),
            },
        );
        st.add_modal_open = false;
        st.add_name_buf.clear();
    }
    if closed || cancel {
        st.add_modal_open = false;
        st.add_name_buf.clear();
    }
}

// ── Delete-profile confirm dialog ─────────────────────────────────────────────

/// Confirm dialog shown before a profile is removed. Rendered every frame; a
/// no-op until a delete is pending. Must be called from any page that can open
/// the profile dropdown.
pub fn delete_confirm_modal(ctx: &egui::Context, cmd: &CommandTx, st: &mut ProfileUi) {
    let Some(name) = st.confirm_delete.clone() else {
        return;
    };
    if let Some(name) = widgets::confirm_delete_dialog(
        ctx,
        "delete_profile",
        &t!("profile.delete_title"),
        &t!("profile.delete_confirm", name = name),
        &t!("profile.delete"),
        &mut st.confirm_delete,
    ) {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::RemoveProfile { name },
        );
    }
}

// ── Profile settings page ─────────────────────────────────────────────────────

fn seed_if_profile_changed(st: &mut ProfileUi, profile: &str) {
    if st.settings_seeded_for != profile {
        st.settings_name_buf = profile.to_string();
        st.settings_seeded_for = profile.to_string();
    }
}

/// Full profile settings page. `profile` is the name from `Page::Profile(name)`.
pub fn show(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut ProfileUi,
    profile: &str,
    page: &mut Page,
    running_apps: &[RunningApp],
) {
    seed_if_profile_changed(st, profile);

    let is_active = state.profiles.active == profile
        || (state.profiles.active.is_empty() && profile == DEFAULT_PROFILE_NAME);
    let is_default = profile == DEFAULT_PROFILE_NAME;

    widgets::page_frame(ui, |ui| {
        widgets::split_columns(ui, 260.0, 24.0, |nav, ui| {
            profile_nav(nav, state, st, profile, page);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if profile_header(ui, state, cmd, st, profile, page, is_active) {
                        return;
                    }
                    ui.add_space(theme::SPACE_8);
                    auto_activate_card(ui, state, cmd, st, profile);
                    ui.add_space(theme::SPACE_8);
                    overrides_section(ui, state, cmd, profile, is_default);
                });
        });
    });

    // Process picker modal (rendered on top, outside scroll area)
    process_picker(ui.ctx(), state, cmd, st, profile, running_apps);
}

enum HeaderAction {
    Delete,
    Rename,
    Activate,
}

/// Name, active tag and description with the profile's actions. Returns
/// `true` when a rename navigated away this frame.
fn profile_header(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut ProfileUi,
    profile: &str,
    page: &mut Page,
    is_active: bool,
) -> bool {
    let is_default = profile == DEFAULT_PROFILE_NAME;
    let switching = st.switching_to.is_some();
    let switching_here = st.switching_to.as_deref() == Some(profile);
    let mut renamed = false;
    let heading = egui::Sides::new().height(52.0).show(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if st.renaming {
                        let te = ui.add(
                            egui::TextEdit::singleline(&mut st.settings_name_buf)
                                .font(theme::semibold(22.0))
                                .desired_width(320.0),
                        );
                        if !te.has_focus() && !te.lost_focus() {
                            te.request_focus();
                        }
                        if te.lost_focus() {
                            st.renaming = false;
                            let new_name = st.settings_name_buf.trim().to_string();
                            let commit = !ui.input(|i| i.key_pressed(egui::Key::Escape));
                            if commit && name_available(state, &new_name) && new_name != profile {
                                crate::runtime::ipc::send(
                                    cmd,
                                    halod_shared::commands::DaemonCommand::RenameProfile {
                                        old_name: profile.to_string(),
                                        new_name: new_name.clone(),
                                    },
                                );
                                *page = Page::Profile(new_name);
                                renamed = true;
                            } else {
                                st.settings_name_buf = profile.to_string();
                            }
                        }
                    } else {
                        ui.label(
                            egui::RichText::new(profile)
                                .font(theme::semibold(22.0))
                                .color(theme::TEXT),
                        );
                    }
                    if is_active {
                        ui.add_space(theme::SPACE_1);
                        widgets::tag(ui, &t!("profile.active"), widgets::TagTone::Accent);
                    }
                });
                ui.add_space(theme::SPACE_1);
                ui.label(
                    egui::RichText::new(if is_default {
                        t!("profile.default_desc")
                    } else {
                        t!("profile.profile_desc")
                    })
                    .font(theme::body_lg())
                    .color(theme::TEXT_DIM),
                );
            })
            .response
            .rect
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_6;
            let size = egui::vec2(0.0, 36.0);
            let mut action = None;
            if !is_default {
                if widgets::button(ui, &t!("profile.delete_profile"), ButtonKind::Danger, size)
                    .clicked()
                {
                    action = Some(HeaderAction::Delete);
                }
                if widgets::button(ui, &t!("profile.rename"), ButtonKind::Secondary, size).clicked()
                {
                    action = Some(HeaderAction::Rename);
                }
            }
            if is_active {
                return action;
            }
            if switching_here {
                widgets::button_loading(ui, &t!("profile.set_active"), ButtonKind::Primary, size);
            } else if widgets::button_enabled(
                ui,
                &t!("profile.set_active"),
                ButtonKind::Primary,
                size,
                !switching,
            ) {
                action = Some(HeaderAction::Activate);
            }
            action
        },
    );
    match heading.1 {
        Some(HeaderAction::Delete) => st.confirm_delete = Some(profile.to_string()),
        Some(HeaderAction::Rename) => {
            st.settings_name_buf = profile.to_string();
            st.renaming = true;
        }
        Some(HeaderAction::Activate) => {
            st.switching_to = Some(profile.to_string());
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SwitchProfile {
                    name: profile.to_string(),
                },
            );
        }
        None => {}
    }
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::ProfileHeader,
        heading.0,
    );
    renamed
}

fn profile_nav(
    ui: &mut egui::Ui,
    state: &TopicStore,
    st: &mut ProfileUi,
    current: &str,
    page: &mut Page,
) {
    ui.label(
        egui::RichText::new(t!("profile.profiles_title"))
            .font(theme::display())
            .color(theme::TEXT),
    );
    ui.add_space(theme::SPACE_5);
    ui.spacing_mut().item_spacing.y = theme::SPACE_2;
    for name in &state.profiles.available {
        let note = if state.profiles.active == *name {
            t!("profile.active")
        } else if name == DEFAULT_PROFILE_NAME {
            t!("profile.fallback")
        } else {
            Default::default()
        };
        let (rect, resp) = widgets::hover_row(ui, 38.0, name == current);
        ui.painter().text(
            Pos2::new(rect.left() + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            theme::body_lg(),
            if name == current {
                theme::NAV_TEXT_ACTIVE
            } else {
                theme::TEXT_SOFT
            },
        );
        ui.painter().text(
            Pos2::new(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            note,
            theme::body_sm(),
            theme::TEXT_MUT,
        );
        if resp.clicked() {
            *page = Page::Profile(name.clone());
        }
    }
    ui.add_space(theme::SPACE_4);
    if widgets::button(
        ui,
        &t!("profile.new_profile"),
        ButtonKind::Secondary,
        Vec2::new(ui.available_width(), 36.0),
    )
    .clicked()
    {
        st.add_modal_open = true;
    }
}

/// A profile page section: the shared card with its title and optional lede.
fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    lede: Option<&str>,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    widgets::card_with_margin(ui, egui::Margin::symmetric(20, 18), |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            egui::RichText::new(title)
                .font(theme::heading())
                .color(theme::TEXT),
        );
        if let Some(lede) = lede {
            ui.add_space(theme::SPACE_1);
            ui.label(
                egui::RichText::new(lede)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
        }
        ui.add_space(theme::SPACE_7);
        body(ui)
    })
}

fn names_after_removing(names: &[String], proc_name: &str) -> Vec<String> {
    names.iter().filter(|p| *p != proc_name).cloned().collect()
}

fn merge_process_names(existing: &[String], selected: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = existing.to_vec();
    for p in selected {
        if !merged.contains(p) {
            merged.push(p.clone());
        }
    }
    merged
}

fn auto_activate_card(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut ProfileUi,
    profile: &str,
) {
    // Collect all data before entering the closure so we can split-borrow st.
    let rule_processes: Vec<(usize, Vec<String>)> = state
        .profiles
        .app_rules
        .iter()
        .enumerate()
        .filter(|(_, r)| r.profile == profile)
        .map(|(i, r)| (i, r.process_names.clone()))
        .collect();

    // Flat list of (rule_index, proc_name, icon_name) for the chip strip.
    let chips: Vec<(usize, String, String)> = rule_processes
        .iter()
        .flat_map(|(rule_idx, names)| {
            names.iter().map(|proc| {
                let icon = state
                    .process_icons
                    .get(proc.as_str())
                    .cloned()
                    .unwrap_or_default();
                (*rule_idx, proc.clone(), icon)
            })
        })
        .collect();

    let all_proc_names: HashSet<String> = chips.iter().map(|(_, p, _)| p.clone()).collect();

    // Deferred mutations applied after the closure.
    let mut remove_process: Option<(usize, String)> = None;
    let mut open_picker = false;

    // Split-borrow st fields the closure needs.
    let ProfileUi {
        icon_cache,
        pick_selected,
        pick_filter,
        picking_for,
        ..
    } = st;

    section(
        ui,
        &t!("profile.auto_activate_title"),
        Some(&t!("profile.auto_activate_desc")),
        |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(theme::SPACE_6);
                for (rule_idx, proc, icon_name) in &chips {
                    let icon = match get_icon(icon_cache, ui.ctx(), icon_name) {
                        Some(tex) => widgets::AppIcon::Texture(tex.id()),
                        None => widgets::AppIcon::Letter(
                            proc.chars()
                                .next()
                                .unwrap_or('?')
                                .to_uppercase()
                                .next()
                                .unwrap_or('?'),
                            theme::widget_hue(proc.bytes().fold(0u8, |h, b| h.wrapping_add(b))),
                        ),
                    };
                    if widgets::app_tile(ui, proc, icon) {
                        remove_process = Some((*rule_idx, proc.clone()));
                    }
                }
                let add = widgets::add_app_tile(ui, &t!("profile.add_processes"));
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::ProfileAddProcess,
                    add.rect,
                );
                if add.clicked() {
                    open_picker = true;
                }
            });
        },
    );

    // Apply deferred mutations. Re-check the rule still belongs to this profile
    // so a concurrent daemon update can't make the snapshot index target a
    // different rule.
    if let Some((rule_idx, proc_name)) = remove_process {
        if let Some(rule) = state
            .profiles
            .app_rules
            .get(rule_idx)
            .filter(|r| r.profile == profile)
        {
            let new_names = names_after_removing(&rule.process_names, &proc_name);
            if new_names.is_empty() {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::RemoveAppRule { index: rule_idx },
                );
            } else {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::UpdateAppRule {
                        index: rule_idx,
                        process_names: new_names,
                        profile: profile.to_string(),
                        enabled: rule.enabled,
                    },
                );
            }
        }
    }
    // Seed only on the closed→open transition so re-clicking the button while
    // the picker is open keeps an in-progress selection.
    if open_picker && picking_for.is_none() {
        *pick_selected = all_proc_names;
        pick_filter.clear();
        *picking_for = Some(profile.to_string());
        crate::runtime::ipc::send(cmd, halod_shared::commands::DaemonCommand::ListRunningApps);
    }
}

fn overrides_section(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    profile: &str,
    is_default: bool,
) {
    use halod_shared::commands::OverrideTarget;
    if is_default {
        return;
    }
    let empty = halod_shared::types::ProfileOverrides::default();
    let overrides = state.profiles.overrides.get(profile).unwrap_or(&empty);
    let mut rows: Vec<(String, String, OverrideTarget)> = Vec::new();
    for (device_id, keys) in &overrides.device_capabilities {
        let device_name = state
            .devices
            .iter()
            .find(|d| &d.id == device_id)
            .map_or(device_id.as_str(), |d| d.name.as_str());
        for (key, diff) in keys {
            let what = std::iter::once(capability_label(key).into_owned())
                .chain(override_diff_lines(&diff.default, &diff.value))
                .collect::<Vec<_>>()
                .join(" · ");
            rows.push((
                device_name.to_string(),
                what,
                OverrideTarget::DeviceCapability {
                    device_id: device_id.clone(),
                    state_key: key.clone(),
                },
            ));
        }
    }
    if overrides.canvas {
        rows.push((
            t!("profile.canvas_effects").into_owned(),
            String::new(),
            OverrideTarget::Canvas,
        ));
    }

    let mut remove_target = None;
    section(ui, &t!("profile.overrides_title"), None, |ui| {
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::ProfileOverrides,
            ui.max_rect(),
        );
        if rows.is_empty() {
            ui.label(
                egui::RichText::new(t!("profile.overrides_empty"))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            return;
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        for (title, what, target) in rows {
            widgets::divider(ui);
            ui.add_space(theme::SPACE_6);
            egui::Sides::new().height(36.0).show(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .font(theme::medium(14.0))
                                .color(theme::TEXT),
                        );
                        if !what.is_empty() {
                            ui.label(
                                egui::RichText::new(what)
                                    .font(theme::body_sm())
                                    .color(theme::TEXT_DIM),
                            );
                        }
                    });
                },
                |ui| {
                    if widgets::button(
                        ui,
                        &t!("profile.revert"),
                        ButtonKind::Secondary,
                        egui::vec2(0.0, 30.0),
                    )
                    .clicked()
                    {
                        remove_target = Some(target);
                    }
                },
            );
            ui.add_space(theme::SPACE_6);
        }
    });
    if let Some(target) = remove_target {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::RemoveProfileOverride {
                profile: profile.to_string(),
                target,
            },
        );
    }
}

// ── Process picker modal ──────────────────────────────────────────────────────

pub fn process_picker(
    ctx: &egui::Context,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut ProfileUi,
    profile: &str,
    running_apps: &[RunningApp],
) {
    let picking_profile = match &st.picking_for {
        Some(p) if p == profile => p.clone(),
        _ => return,
    };

    let mut confirm = false;
    let mut cancel = false;

    let closed = widgets::modal_frame_raw(
        ctx,
        "picker",
        &t!("profile.select_processes_title"),
        440.0,
        520.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("profile.select_processes_desc"))
                    .font(theme::body_md())
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(theme::SPACE_6);

            // Search box (taller for an easier hit target).
            let search_te = egui::TextEdit::singleline(&mut st.pick_filter)
                .desired_width(f32::INFINITY)
                .hint_text(t!("profile.filter_processes_hint").to_string())
                .margin(egui::vec2(10.0, 9.0))
                .font(theme::body_lg());
            ui.add(search_te);
            ui.add_space(theme::SPACE_4);

            // Union of running apps + already-selected processes.
            let mut entries: Vec<(String, String, String)> = running_apps
                .iter()
                .map(|a| {
                    (
                        a.process_name.clone(),
                        a.display_name.clone(),
                        a.icon_name.clone(),
                    )
                })
                .collect();
            for proc in &st.pick_selected {
                if !entries.iter().any(|(p, _, _)| p == proc) {
                    entries.push((proc.clone(), proc.clone(), String::new()));
                }
            }
            entries.sort_by(|a, b| a.1.cmp(&b.1));

            let filter = st.pick_filter.to_lowercase();
            if entries.is_empty() {
                ui.label(
                    egui::RichText::new(t!("profile.no_processes"))
                        .font(theme::body_md())
                        .color(theme::TEXT_FAINT),
                );
            }

            // Reserve room for the pinned button bar (~60px) so only the list scrolls.
            let list_height = (ui.available_height() - 60.0).max(60.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(list_height)
                .show(ui, |ui| {
                    for (proc_name, display, icon_name) in &entries {
                        if !filter.is_empty()
                            && !display.to_lowercase().contains(&filter)
                            && !proc_name.to_lowercase().contains(&filter)
                        {
                            continue;
                        }

                        let selected = st.pick_selected.contains(proc_name.as_str());
                        let (row, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 40.0),
                            Sense::click(),
                        );
                        let hovered = resp.hovered();
                        if hovered || selected {
                            ui.painter().rect_filled(
                                row,
                                6.0,
                                if selected {
                                    a(theme::CYAN, 0.08)
                                } else {
                                    a(Color32::WHITE, 0.03)
                                },
                            );
                        }
                        if hovered {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        // Checkbox circle
                        let check_center = Pos2::new(row.left() + 16.0, row.center().y);
                        if selected {
                            ui.painter().circle_filled(check_center, 7.0, theme::CYAN);
                            ui.painter().text(
                                check_center,
                                Align2::CENTER_CENTER,
                                "✔",
                                theme::bold(9.0),
                                theme::ON_ACCENT,
                            );
                        } else {
                            ui.painter().circle_stroke(
                                check_center,
                                7.0,
                                Stroke::new(1.5, theme::BORDER),
                            );
                        }

                        // App icon (32×32, left of text)
                        let icon_rect = Rect::from_min_size(
                            Pos2::new(row.left() + 32.0, row.center().y - 16.0),
                            Vec2::splat(32.0),
                        );
                        draw_process_icon(ui, icon_rect, icon_name, display, &st.icon_cache);

                        // Display name + process name (right of icon), clipped to the
                        // row so long names can't spill past the modal edge.
                        let text_x = icon_rect.right() + 10.0;
                        let text_clip = Rect::from_min_max(
                            Pos2::new(text_x, row.top()),
                            Pos2::new(row.right() - 12.0, row.bottom()),
                        );
                        let p = ui.painter().with_clip_rect(text_clip);
                        p.text(
                            Pos2::new(text_x, row.center().y - 5.0),
                            Align2::LEFT_CENTER,
                            display,
                            theme::subhead(),
                            if selected {
                                theme::TEXT
                            } else {
                                theme::TEXT_DIM
                            },
                        );
                        p.text(
                            Pos2::new(text_x, row.center().y + 7.0),
                            Align2::LEFT_CENTER,
                            proc_name,
                            theme::value_xs(),
                            theme::TEXT_FAINT,
                        );

                        if resp.clicked() {
                            if selected {
                                st.pick_selected.remove(proc_name.as_str());
                            } else {
                                st.pick_selected.insert(proc_name.clone());
                            }
                        }
                    }
                });

            ui.add_space(theme::SPACE_6);
            ui.separator();
            ui.add_space(theme::SPACE_4);

            let n = st.pick_selected.len();
            let btn_label = if n > 0 {
                t!("profile.add_selected_n", n = n).to_string()
            } else {
                t!("profile.add_selected").to_string()
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, &btn_label, ButtonKind::Primary, egui::vec2(160.0, 34.0))
                    .clicked()
                    && n > 0
                {
                    confirm = true;
                }
                ui.add_space(theme::SPACE_4);
                if widgets::button(
                    ui,
                    &t!("profile.cancel"),
                    ButtonKind::Secondary,
                    egui::vec2(80.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            });
        },
    );

    if confirm {
        let selected: Vec<String> = st.pick_selected.iter().cloned().collect();
        if !selected.is_empty() {
            // Find an existing rule for this profile to merge into, or create new one.
            let existing_rule = state
                .profiles
                .app_rules
                .iter()
                .enumerate()
                .find(|(_, r)| r.profile == picking_profile);
            if let Some((idx, rule)) = existing_rule {
                let merged = merge_process_names(&rule.process_names, &selected);
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::UpdateAppRule {
                        index: idx,
                        process_names: merged,
                        profile: picking_profile.clone(),
                        enabled: rule.enabled,
                    },
                );
            } else {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::AddAppRule {
                        process_names: selected,
                        profile: picking_profile.clone(),
                        enabled: true,
                    },
                );
            }
        }
        st.picking_for = None;
        st.pick_selected.clear();
    }
    if cancel || closed {
        st.picking_for = None;
        st.pick_selected.clear();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::AppRule;

    #[test]
    fn capability_label_translates_known_keys_and_title_cases_unknown() {
        // Curated keys resolve to their catalog copy (default `en` locale)...
        assert_eq!(capability_label(capability::FAN_CURVE), "Fan curve");
        assert_eq!(capability_label(capability::RGB), "RGB");
        assert_eq!(capability_label(capability::BOOLEAN), "Toggles");
        // ...unknown/dynamic state keys fall back to title-casing.
        assert_eq!(capability_label("button_mapping"), "Button Mapping");
        assert_eq!(capability_label("brightness"), "Brightness");
    }

    #[test]
    fn override_diff_lines_reports_only_changed_fields() {
        let default = serde_json::json!({ "nc_mode": 0, "boost": true });
        let value = serde_json::json!({ "nc_mode": 2, "boost": true });
        assert_eq!(
            override_diff_lines(&default, &value),
            vec!["Nc Mode: 0 → 2".to_string()]
        );
    }

    #[test]
    fn override_diff_lines_marks_fields_missing_on_either_side() {
        let default = serde_json::json!({ "a": 1 });
        let value = serde_json::json!({ "b": 2 });
        assert_eq!(
            override_diff_lines(&default, &value),
            vec!["A: 1 → —".to_string(), "B: — → 2".to_string()]
        );
    }

    #[test]
    fn override_diff_lines_skips_composite_values_like_fan_curves() {
        let default =
            serde_json::json!({ "fan1": { "points": [[20, 25], [40, 30]] }, "boost": false });
        let value =
            serde_json::json!({ "fan1": { "points": [[20, 40], [40, 55]] }, "boost": true });
        assert_eq!(
            override_diff_lines(&default, &value),
            vec!["Boost: false → true".to_string()]
        );
    }

    #[test]
    fn override_diff_lines_empty_for_non_object_state() {
        let value = serde_json::json!({ "nc_mode": 2 });
        assert!(override_diff_lines(&serde_json::Value::Null, &value).is_empty());
        assert!(override_diff_lines(&value.clone(), &value).is_empty());
    }

    fn state_with_profiles(profiles: &[&str], active: &str, rules: Vec<AppRule>) -> TopicStore {
        TopicStore {
            profiles: halod_shared::types::ProfileState {
                active: active.to_string(),
                available: profiles.iter().map(|s| s.to_string()).collect(),
                app_rules: rules,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn notification(code: halod_shared::types::NotificationCode) -> Notification {
        Notification {
            code,
            show_native: false,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn observe_notifications_clears_on_profile_switched() {
        use halod_shared::types::NotificationCode;
        let mut st = ProfileUi {
            switching_to: Some("Gaming".to_string()),
            ..Default::default()
        };
        observe_notifications(
            &mut st,
            &[notification(NotificationCode::FanStalled {
                fan: "cpu".into(),
            })],
        );
        assert_eq!(st.switching_to.as_deref(), Some("Gaming"));
        observe_notifications(
            &mut st,
            &[notification(NotificationCode::ProfileSwitched {
                profile: "Gaming".into(),
            })],
        );
        assert!(st.switching_to.is_none());
    }

    #[test]
    fn name_available_rejects_empty_and_case_insensitive_duplicates() {
        let state = state_with_profiles(&["Gaming", "Work"], "Gaming", vec![]);
        assert!(!name_available(&state, ""));
        assert!(!name_available(&state, "Gaming"));
        assert!(!name_available(&state, "gaming"));
        assert!(name_available(&state, "Streaming"));
    }

    #[test]
    fn seed_resets_on_profile_change() {
        let mut st = ProfileUi::default();
        seed_if_profile_changed(&mut st, "Gaming");
        assert_eq!(st.settings_name_buf, "Gaming");
        seed_if_profile_changed(&mut st, "Work");
        assert_eq!(st.settings_name_buf, "Work");
    }

    #[test]
    fn process_chip_removal_drops_rule_when_empty() {
        let remaining = names_after_removing(&["game.exe".to_string()], "game.exe");
        assert!(
            remaining.is_empty(),
            "rule should be removed when no processes remain"
        );
    }

    #[test]
    fn picker_merges_into_existing_rule() {
        let merged = merge_process_names(&["game.exe".to_string()], &["other.exe".to_string()]);
        assert_eq!(merged, vec!["game.exe", "other.exe"]);
    }

    #[test]
    fn picker_merge_does_not_duplicate_existing_process() {
        let merged = merge_process_names(&["game.exe".to_string()], &["game.exe".to_string()]);
        assert_eq!(merged, vec!["game.exe"]);
    }
}

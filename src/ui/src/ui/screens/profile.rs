// SPDX-License-Identifier: GPL-3.0-or-later
//! Profile button (title-bar), dropdown, profile-settings page, and
//! multi-select process-picker modal.

use crate::ui::components as widgets;
use std::collections::{HashMap, HashSet};

use egui::{Align2, Color32, Id, Order, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::capability;
use halod_shared::types::{AppState, Notification, RunningApp, DEFAULT_PROFILE_NAME};

use crate::domain::state::Page;
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
    /// Profile a delete is pending confirmation for; drives the confirm dialog.
    pub confirm_delete: Option<String>,
    /// Last profile the settings page was seeded for (detect profile changes).
    pub settings_seeded_for: String,
    /// Icon cache, populated off-thread.
    pub icon_cache: IconCache,
    /// Profile a switch is pending for; cleared on its "Profile switched" notification.
    pub switching_to: Option<String>,
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

/// State of a background icon load. `Failed` is terminal so a missing icon is
/// not re-resolved every frame.
pub enum IconState {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

pub type IconCache = std::sync::Arc<std::sync::Mutex<HashMap<String, IconState>>>;

// ── Icon loading ─────────────────────────────────────────────────────────────

/// Resolve an icon identifier (theme name or absolute path) to an image file.
/// Returns `None` if the icon cannot be found.
fn resolve_icon_path(name: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_owned());
    }
    #[cfg(unix)]
    return find_xdg_icon(name);
    #[cfg(not(unix))]
    None
}

/// Parse the pixel size from a hicolor theme directory name
/// ("48x48" -> 48, "scalable" -> 0).
#[cfg(unix)]
fn parse_icon_dir_size(dir_name: &str) -> u32 {
    dir_name
        .split('x')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Accumulates the best icon candidate across the filesystem walk, applying the
/// raster > svg > pixmap precedence (larger raster wins).
#[cfg(unix)]
#[derive(Default)]
struct IconRanker {
    best_raster: Option<(u32, std::path::PathBuf)>,
    svg: Option<std::path::PathBuf>,
    pixmap: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl IconRanker {
    /// Offer a raster candidate of the given pixel size; keeps it if it is the
    /// largest seen so far.
    fn offer_raster(&mut self, size: u32, path: std::path::PathBuf) {
        if self.best_raster.as_ref().is_none_or(|(s, _)| size > *s) {
            self.best_raster = Some((size, path));
        }
    }

    /// Offer an SVG candidate; the first one seen is kept.
    fn offer_svg(&mut self, path: std::path::PathBuf) {
        if self.svg.is_none() {
            self.svg = Some(path);
        }
    }

    /// Offer a pixmaps-dir candidate; the first one seen is kept.
    fn offer_pixmap(&mut self, path: std::path::PathBuf) {
        if self.pixmap.is_none() {
            self.pixmap = Some(path);
        }
    }

    /// Resolve to the winning path: largest raster, else svg, else pixmap.
    fn winner(self) -> Option<std::path::PathBuf> {
        self.best_raster
            .map(|(_, p)| p)
            .or(self.svg)
            .or(self.pixmap)
    }
}

/// Walk XDG_DATA_DIRS icon directories for `name`, returning the best match.
/// Prefers the largest raster (PNG/JPEG) so it stays crisp; falls back to a
/// scalable SVG (rasterized at load time) and finally the pixmaps directory.
#[cfg(unix)]
fn find_xdg_icon(name: &str) -> Option<std::path::PathBuf> {
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());

    let mut ranker = IconRanker::default();

    for base in data_dirs.split(':') {
        if base.is_empty() {
            continue;
        }
        let hicolor = std::path::PathBuf::from(base).join("icons/hicolor");
        if let Ok(entries) = std::fs::read_dir(&hicolor) {
            for entry in entries.flatten() {
                let apps_dir = entry.path().join("apps");
                for ext in ["png", "jpg", "jpeg"] {
                    let candidate = apps_dir.join(format!("{name}.{ext}"));
                    if candidate.exists() {
                        let size = parse_icon_dir_size(&entry.file_name().to_string_lossy());
                        ranker.offer_raster(size, candidate);
                    }
                }
                let candidate = apps_dir.join(format!("{name}.svg"));
                if candidate.exists() {
                    ranker.offer_svg(candidate);
                }
            }
        }
        for ext in ["png", "jpg", "jpeg", "svg"] {
            let p = std::path::PathBuf::from(base)
                .join("pixmaps")
                .join(format!("{name}.{ext}"));
            if p.exists() {
                ranker.offer_pixmap(p);
                break;
            }
        }
    }
    ranker.winner()
}

/// Rasterize an SVG file to RGBA at roughly `target` pixels on the long edge.
fn rasterize_svg(path: &std::path::Path, target: u32) -> Option<egui::ColorImage> {
    use resvg::{tiny_skia, usvg};

    let data = std::fs::read(path).ok()?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size();
    let long_edge = size.width().max(size.height()) as f32;
    if long_edge <= 0.0 {
        return None;
    }
    let scale = target as f32 / long_edge;
    let w = (size.width() as f32 * scale).ceil() as u32;
    let h = (size.height() as f32 * scale).ceil() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [pixmap.width() as usize, pixmap.height() as usize],
        pixmap.data(),
    ))
}

/// Texture size icons are normalized to. Larger than the on-screen render size
/// (16–32px) so the final linear downscale stays crisp on HiDPI.
const ICON_PX: u32 = 64;

/// Load an icon into an egui texture. Handles both raster and SVG sources.
/// Raster sources are downscaled with a high-quality filter so a large source
/// (e.g. a 1024² PNG) doesn't alias when drawn at 16–32px.
fn load_icon_texture(ctx: &egui::Context, name: &str) -> Option<egui::TextureHandle> {
    let path = resolve_icon_path(name)?;
    let color_image = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        rasterize_svg(&path, ICON_PX)?
    } else {
        let mut img = image::open(&path).ok()?;
        if img.width().max(img.height()) > ICON_PX {
            img = img.resize(ICON_PX, ICON_PX, image::imageops::FilterType::Lanczos3);
        }
        let rgba = img.into_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        egui::ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw())
    };
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR))
}

/// Get a ready icon texture, kicking off a background load on first request.
/// Returns `None` while loading, on failure, or for an unknown icon — the call
/// never blocks the UI thread on filesystem or decode work.
pub fn get_icon(
    cache: &IconCache,
    ctx: &egui::Context,
    icon_name: &str,
) -> Option<egui::TextureHandle> {
    if icon_name.is_empty() {
        return None;
    }
    let mut map = cache.lock().unwrap();
    if let Some(state) = map.get(icon_name) {
        return match state {
            IconState::Ready(tex) => Some(tex.clone()),
            IconState::Loading | IconState::Failed => None,
        };
    }
    map.insert(icon_name.to_string(), IconState::Loading);
    drop(map);
    spawn_icon_load(cache.clone(), ctx.clone(), icon_name.to_string());
    None
}

/// Resolve and decode an icon off the UI thread, then store the result and
/// request a repaint so the loaded texture appears on the next frame.
fn spawn_icon_load(cache: IconCache, ctx: egui::Context, name: String) {
    std::thread::spawn(move || {
        let state = match load_icon_texture(&ctx, &name) {
            Some(tex) => IconState::Ready(tex),
            None => IconState::Failed,
        };
        if let Ok(mut map) = cache.lock() {
            map.insert(name, state);
        }
        ctx.request_repaint();
    });
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
            .rect_filled(rect, rect.height() * 0.25, theme::hex(0x1c2535));
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
    state: &AppState,
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

    // Measure the text components to size the pill.
    let label_width = p
        .layout_no_wrap(
            t!("profile.profile_label").to_string(),
            theme::body(9.0),
            Color32::WHITE,
        )
        .size()
        .x;
    let name_width = p
        .layout_no_wrap(
            profile_name.to_string(),
            theme::semibold(12.0),
            Color32::WHITE,
        )
        .size()
        .x;
    // dot(6) + gap(9) + "PROFILE"(lw) + gap(9) + name(nw) + gap(8) + chevron(8) + padding(22)
    let pill_w = 6.0 + 9.0 + label_width + 9.0 + name_width + 8.0 + 8.0 + 22.0;
    let pill_h = 28.0;
    let pill_rect = Rect::from_min_size(
        Pos2::new(right_x - pill_w, cy - pill_h / 2.0),
        Vec2::new(pill_w, pill_h),
    );

    let resp = ui.interact(pill_rect, Id::new("profile_btn"), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let bg = if hovered || st.dropdown_open {
        theme::hex(0x14192a)
    } else {
        theme::hex(0x10141d)
    };
    p.rect_filled(pill_rect, 9.0, bg);
    p.rect_stroke(
        pill_rect,
        9.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );

    let mut x = pill_rect.left() + 11.0;
    let dot = Pos2::new(x + 3.0, cy);
    theme::glow(p, dot, 5.0, theme::CYAN, 0.6);
    p.circle_filled(dot, 3.0, theme::CYAN);
    x += 6.0 + 9.0;
    p.text(
        Pos2::new(x, cy),
        Align2::LEFT_CENTER,
        t!("profile.profile_label"),
        theme::body(9.0),
        theme::TEXT_FAINT,
    );
    x += label_width + 9.0;
    p.text(
        Pos2::new(x, cy),
        Align2::LEFT_CENTER,
        profile_name,
        theme::semibold(12.0),
        theme::TEXT,
    );
    x += name_width + 8.0;
    if switching {
        let spin_rect = Rect::from_center_size(Pos2::new(x + 4.0, cy), Vec2::splat(10.0));
        egui::Spinner::new()
            .size(10.0)
            .color(theme::TEXT_FAINT)
            .paint_at(ui, spin_rect);
    } else {
        p.text(
            Pos2::new(x + 4.0, cy),
            Align2::CENTER_CENTER,
            if st.dropdown_open { "▴" } else { "▾" },
            theme::body(8.0),
            theme::TEXT_FAINT,
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
    state: &AppState,
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
        .fill(theme::hex(0x0d1119))
        .stroke(Stroke::new(1.0, theme::hex(0x232a39)))
        .corner_radius(12.0)
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
        Id::new("profile_dropdown"),
        ctx.clone(),
        btn_rect,
        egui::LayerId::new(Order::Foreground, Id::new("profile_dropdown")),
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
                theme::semibold(13.0)
            } else {
                theme::body(13.0)
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
            let gear_resp =
                ui.interact(gear_rect, Id::new(("pdrop_gear", profile)), Sense::click());
            let gear_col = if gear_resp.hovered() {
                theme::TEXT
            } else {
                theme::TEXT_FAINT
            };
            if gear_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.painter().text(
                gear_rect.center(),
                Align2::CENTER_CENTER,
                "⚙",
                theme::body(13.0),
                gear_col,
            );
            if gear_resp.clicked() {
                navigate_settings = Some(profile.clone());
            }

            // Delete × (non-default profiles only)
            if !is_default {
                let del_rect = Rect::from_center_size(Pos2::new(del_x, icon_y), Vec2::splat(20.0));
                let del_resp =
                    ui.interact(del_rect, Id::new(("pdrop_del", profile)), Sense::click());
                let del_col = if del_resp.hovered() {
                    theme::OFFLINE
                } else {
                    theme::TEXT_FAINT
                };
                if del_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                ui.painter().text(
                    del_rect.center(),
                    Align2::CENTER_CENTER,
                    "×",
                    theme::semibold(16.0),
                    del_col,
                );
                if del_resp.clicked() {
                    remove = Some(profile.clone());
                }
            }

            // Click row body (left of icons) → switch to profile
            let body_rect =
                Rect::from_min_max(row_rect.min, Pos2::new(del_x - 6.0, row_rect.max.y));
            let body_resp = ui.interact(body_rect, Id::new(("pdrop_row", profile)), Sense::click());
            if body_resp.clicked() && !is_active && !is_switching {
                switch_to = Some(profile.clone());
            }
        }

        ui.add_space(4.0);
        ui.painter().line_segment(
            [
                Pos2::new(ui.max_rect().left(), ui.cursor().top()),
                Pos2::new(ui.max_rect().right(), ui.cursor().top()),
            ],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.add_space(4.0);

        // Bottom actions row
        ui.horizontal(|ui| {
            let add_w = (drop_w - 16.0 - 8.0) / 2.0;
            if widgets::button(
                ui,
                &t!("profile.new_profile_btn"),
                widgets::ButtonKind::Ghost,
                Vec2::new(add_w, 34.0),
            )
            .clicked()
            {
                open_add = true;
            }
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("profile.profile_settings_btn"),
                widgets::ButtonKind::Ghost,
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
        crate::domain::actions::profiles::switch_profile(cmd, &name);
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
fn name_available(state: &AppState, trimmed: &str) -> bool {
    !trimmed.is_empty()
        && !state
            .profiles
            .available
            .iter()
            .any(|p| p.eq_ignore_ascii_case(trimmed))
}

pub fn add_modal(ctx: &egui::Context, state: &AppState, cmd: &CommandTx, st: &mut ProfileUi) {
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
                .font(theme::body(13.0));
            let te_resp = ui.add(te);
            te_resp.request_focus();

            // Enter key submits
            if te_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && name_ok {
                enter_submit = true;
            }

            if !name_ok && !st.add_name_buf.trim().is_empty() {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(t!("profile.name_taken"))
                        .font(theme::body(11.0))
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
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("profile.cancel"),
                ButtonKind::Ghost,
                egui::vec2(80.0, 34.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );

    if (create || enter_submit) && name_available(state, st.add_name_buf.trim()) {
        crate::domain::actions::profiles::add_profile(cmd, st.add_name_buf.trim());
        st.add_modal_open = false;
        st.add_name_buf.clear();
    }
    if closed || cancel {
        st.add_modal_open = false;
        st.add_name_buf.clear();
    }
}

// ── Delete-profile confirm dialog ─────────────────────────────────────────────

/// Resolve a pending delete against the dialog's confirm/dismiss outcome —
/// delegates to the shared widget helper.
use widgets::resolve_delete_confirm;

/// Confirm dialog shown before a profile is removed. Rendered every frame; a
/// no-op until a delete is pending. Must be called from any page that can open
/// the profile dropdown.
pub fn delete_confirm_modal(ctx: &egui::Context, cmd: &CommandTx, st: &mut ProfileUi) {
    let Some(name) = st.confirm_delete.clone() else {
        return;
    };

    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ctx,
        "delete_profile",
        &t!("profile.delete_title"),
        380.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("profile.delete_confirm", name = name))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("profile.delete"),
                ButtonKind::Danger,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("profile.cancel"),
                ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );

    if let Some(name) = resolve_delete_confirm(&mut st.confirm_delete, confirm, cancel || dismissed)
    {
        crate::domain::actions::profiles::remove_profile(cmd, &name);
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
    state: &AppState,
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

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                ui.add_space(36.0);
                ui.vertical(|ui| {
                    ui.set_max_width(760.0);

                    // Back button
                    let back_resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(t!("profile.back"))
                                .font(theme::body(12.0))
                                .color(theme::TEXT_FAINT),
                        )
                        .sense(Sense::click()),
                    );
                    if back_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if back_resp.clicked() {
                        *page = Page::Home;
                    }
                    ui.add_space(16.0);

                    // Profile name heading (editable for non-default)
                    let heading = ui.horizontal(|ui| {
                        if is_default {
                            ui.label(
                                egui::RichText::new(profile)
                                    .font(theme::bold(22.0))
                                    .color(theme::TEXT),
                            );
                        } else {
                            let te = egui::TextEdit::singleline(&mut st.settings_name_buf)
                                .font(theme::bold(22.0))
                                .desired_width(400.0)
                                .frame(egui::Frame::NONE)
                                .text_color(theme::TEXT);
                            let te_resp = ui.add(te);
                            if te_resp.lost_focus() {
                                let new_name = st.settings_name_buf.trim().to_string();
                                if name_available(state, &new_name) && new_name != profile {
                                    crate::domain::actions::profiles::rename_profile(
                                        cmd, profile, &new_name,
                                    );
                                    // Optimistic nav; main.rs redirects to Home if the name
                                    // disappears.
                                    *page = Page::Profile(new_name);
                                    return;
                                }
                                st.settings_name_buf = profile.to_string();
                            }
                        }

                        ui.add_space(12.0);
                        if is_active {
                            // Active badge
                            let badge_rect =
                                Rect::from_min_size(ui.cursor().min, Vec2::new(60.0, 22.0));
                            ui.allocate_rect(badge_rect, Sense::hover());
                            ui.painter()
                                .rect_filled(badge_rect, 6.0, a(theme::CYAN, 0.15));
                            ui.painter().rect_stroke(
                                badge_rect,
                                6.0,
                                Stroke::new(1.0, a(theme::CYAN, 0.35)),
                                egui::StrokeKind::Middle,
                            );
                            ui.painter().text(
                                badge_rect.center(),
                                Align2::CENTER_CENTER,
                                t!("profile.active"),
                                theme::semibold(11.0),
                                theme::CYAN,
                            );
                        } else if st.switching_to.as_deref() == Some(profile) {
                            widgets::button_loading(
                                ui,
                                &t!("profile.set_active"),
                                ButtonKind::Ghost,
                                egui::vec2(90.0, 28.0),
                            );
                        } else if st.switching_to.is_none() {
                            if widgets::button(
                                ui,
                                &t!("profile.set_active"),
                                ButtonKind::Ghost,
                                egui::vec2(90.0, 28.0),
                            )
                            .clicked()
                            {
                                st.switching_to = Some(profile.to_string());
                                crate::domain::actions::profiles::switch_profile(cmd, profile);
                            }
                        } else {
                            widgets::button_disabled(
                                ui,
                                &t!("profile.set_active"),
                                ButtonKind::Ghost,
                                egui::vec2(90.0, 28.0),
                            );
                        }
                    });
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::ProfileHeader,
                        heading.response.rect,
                    );

                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(if is_default {
                            t!("profile.default_desc")
                        } else {
                            t!("profile.profile_desc")
                        })
                        .font(theme::body(12.0))
                        .color(theme::TEXT_FAINT),
                    );
                    ui.add_space(24.0);

                    // ── AUTO-ACTIVATE section ────────────────────────────────
                    auto_activate_card(ui, state, cmd, st, profile);
                    ui.add_space(20.0);

                    // ── OVERRIDES section ────────────────────────────────────
                    overrides_section(ui, state, cmd, is_active, is_default);
                });
                ui.add_space(36.0);
            });
        });

    // Process picker modal (rendered on top, outside scroll area)
    process_picker(ui.ctx(), state, cmd, st, profile, running_apps);
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
    state: &AppState,
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

    widgets::card_titled(
        ui,
        &t!("profile.auto_activate_title"),
        |_| {},
        |ui| {
            ui.label(
                egui::RichText::new(t!("profile.auto_activate_desc"))
                    .font(theme::body(12.0))
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(10.0);

            if !chips.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for (rule_idx, proc, icon_name) in &chips {
                        let chip_resp = process_chip(ui, proc, icon_name, icon_cache);
                        if chip_resp.clicked() {
                            remove_process = Some((*rule_idx, proc.clone()));
                        }
                    }
                });
                ui.add_space(8.0);
            }

            let add_btn_rect = Rect::from_min_size(ui.cursor().min, Vec2::new(150.0, 32.0));
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::ProfileAddProcess,
                add_btn_rect,
            );
            if widgets::button(
                ui,
                &t!("profile.add_processes"),
                ButtonKind::Ghost,
                egui::vec2(150.0, 32.0),
            )
            .clicked()
            {
                open_picker = true;
            }
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
                crate::domain::actions::profiles::remove_app_rule(cmd, rule_idx);
            } else {
                crate::domain::actions::profiles::update_app_rule(
                    cmd,
                    rule_idx,
                    new_names,
                    profile,
                    rule.enabled,
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
        crate::domain::actions::profiles::list_running_apps(cmd);
    }
}

/// A small removable process chip with icon. Returns `Response` — `.clicked()` means the × was hit.
fn process_chip(
    ui: &mut egui::Ui,
    proc_name: &str,
    icon_name: &str,
    cache: &IconCache,
) -> egui::Response {
    let font = theme::body(11.5);
    let text_galley =
        ui.painter()
            .layout_no_wrap(proc_name.to_string(), font.clone(), Color32::WHITE);
    let text_w = text_galley.size().x;
    let icon_size = 16.0;
    // chip: 7px + icon(16) + 5px + text + 6px + ×(10) + 7px
    let chip_w = 7.0 + icon_size + 5.0 + text_w + 6.0 + 10.0 + 7.0;
    let chip_h = 26.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(chip_w, chip_h), Sense::click());
    let hovered = resp.hovered();

    let bg = if hovered {
        a(Color32::WHITE, 0.08)
    } else {
        a(Color32::WHITE, 0.05)
    };
    ui.painter().rect_filled(rect, 7.0, bg);
    ui.painter().rect_stroke(
        rect,
        7.0,
        Stroke::new(1.0, theme::BORDER_SOFT),
        egui::StrokeKind::Middle,
    );

    let icon_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 7.0, rect.center().y - icon_size / 2.0),
        Vec2::splat(icon_size),
    );
    draw_process_icon(ui, icon_rect, icon_name, proc_name, cache);

    ui.painter().text(
        Pos2::new(icon_rect.right() + 5.0, rect.center().y),
        Align2::LEFT_CENTER,
        proc_name,
        font,
        theme::TEXT_DIM,
    );

    let x_col = if hovered {
        theme::OFFLINE
    } else {
        theme::TEXT_FAINT
    };
    ui.painter().text(
        Pos2::new(rect.right() - 10.0, rect.center().y),
        Align2::CENTER_CENTER,
        "×",
        theme::semibold(13.0),
        x_col,
    );
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn overrides_section(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    is_active: bool,
    is_default: bool,
) {
    if is_default {
        return;
    }

    widgets::card_titled(
        ui,
        &t!("profile.overrides_title"),
        |_| {},
        |ui| {
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::ProfileOverrides,
                ui.max_rect(),
            );
            if !is_active {
                ui.label(
                    egui::RichText::new(t!("profile.overrides_inactive"))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_FAINT),
                );
                return;
            }

            let overrides = &state.profiles.overrides;
            if overrides.device_capabilities.is_empty() && !overrides.canvas {
                ui.label(
                    egui::RichText::new(t!("profile.overrides_empty"))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_FAINT),
                );
                return;
            }

            let mut remove_target: Option<halod_shared::commands::OverrideTarget> = None;

            // Canvas override
            if overrides.canvas {
                ui.add_space(4.0);
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(t!("profile.canvas_effects"))
                                .font(theme::semibold(13.0))
                                .color(theme::TEXT),
                        );
                    },
                    |ui| {
                        if widgets::button(
                            ui,
                            &t!("profile.revert"),
                            ButtonKind::Ghost,
                            egui::vec2(70.0, 26.0),
                        )
                        .clicked()
                        {
                            remove_target = Some(halod_shared::commands::OverrideTarget::Canvas);
                        }
                    },
                );
                ui.add_space(4.0);
            }

            // Per-device capability overrides
            for (device_id, keys) in &overrides.device_capabilities {
                let device_name = state
                    .devices
                    .iter()
                    .find(|d| &d.id == device_id)
                    .map(|d| d.name.as_str())
                    .unwrap_or(device_id.as_str());

                ui.add_space(4.0);
                widgets::caps_label(ui, device_name);
                ui.add_space(4.0);

                for key in keys {
                    let label = capability_label(key);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(label.as_ref())
                                .font(theme::body(13.0))
                                .color(theme::TEXT_DIM),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if widgets::button(
                                ui,
                                &t!("profile.revert"),
                                ButtonKind::Ghost,
                                egui::vec2(70.0, 26.0),
                            )
                            .clicked()
                            {
                                remove_target = Some(
                                    halod_shared::commands::OverrideTarget::DeviceCapability {
                                        device_id: device_id.clone(),
                                        state_key: key.clone(),
                                    },
                                );
                            }
                        });
                    });
                }
            }

            if let Some(target) = remove_target {
                crate::domain::actions::profiles::remove_profile_override(cmd, target);
            }
        },
    );
}

// ── Process picker modal ──────────────────────────────────────────────────────

pub fn process_picker(
    ctx: &egui::Context,
    state: &AppState,
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
                    .font(theme::body(12.0))
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(12.0);

            // Search box (taller for an easier hit target).
            let search_te = egui::TextEdit::singleline(&mut st.pick_filter)
                .desired_width(f32::INFINITY)
                .hint_text(t!("profile.filter_processes_hint").to_string())
                .margin(egui::vec2(10.0, 9.0))
                .font(theme::body(13.0));
            ui.add(search_te);
            ui.add_space(8.0);

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
                        .font(theme::body(12.0))
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
                                theme::hex(0x0a0d13),
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
                            theme::semibold(12.0),
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
                            theme::mono(9.5),
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

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);

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
                ui.add_space(8.0);
                if widgets::button(
                    ui,
                    &t!("profile.cancel"),
                    ButtonKind::Ghost,
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
                crate::domain::actions::profiles::update_app_rule(
                    cmd,
                    idx,
                    merged,
                    &picking_profile,
                    rule.enabled,
                );
            } else {
                crate::domain::actions::profiles::add_app_rule(
                    cmd,
                    selected,
                    &picking_profile,
                    true,
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
    use halod_shared::types::{AppRule, AppState};

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

    fn state_with_profiles(profiles: &[&str], active: &str, rules: Vec<AppRule>) -> AppState {
        AppState {
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
    fn resolve_delete_confirm_only_deletes_on_confirm() {
        // Dialog still open: nothing happens, pending preserved.
        let mut pending = Some("Gaming".to_string());
        assert_eq!(resolve_delete_confirm(&mut pending, false, false), None);
        assert_eq!(pending.as_deref(), Some("Gaming"));

        // Dismissed (cancel or backdrop): pending cleared, no delete.
        assert_eq!(resolve_delete_confirm(&mut pending, false, true), None);
        assert_eq!(pending, None);

        // Confirmed: returns the name and clears pending.
        pending = Some("Gaming".to_string());
        assert_eq!(
            resolve_delete_confirm(&mut pending, true, false).as_deref(),
            Some("Gaming")
        );
        assert_eq!(pending, None);
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
    fn get_icon_never_blocks_and_reflects_cache_state() {
        let ctx = egui::Context::default();
        let cache: IconCache = Default::default();

        // Empty name resolves to nothing and registers no load.
        assert!(get_icon(&cache, &ctx, "").is_none());
        assert!(cache.lock().unwrap().is_empty());

        // Pending and failed entries yield None without spawning a load.
        cache.lock().unwrap().insert("a".into(), IconState::Loading);
        cache.lock().unwrap().insert("b".into(), IconState::Failed);
        assert!(get_icon(&cache, &ctx, "a").is_none());
        assert!(get_icon(&cache, &ctx, "b").is_none());

        // A ready entry yields its texture.
        let tex = ctx.load_texture("t", egui::ColorImage::example(), Default::default());
        cache
            .lock()
            .unwrap()
            .insert("c".into(), IconState::Ready(tex.clone()));
        assert_eq!(get_icon(&cache, &ctx, "c").map(|t| t.id()), Some(tex.id()));
    }

    #[test]
    fn get_icon_first_request_registers_a_background_load() {
        let ctx = egui::Context::default();
        let cache: IconCache = Default::default();
        assert!(get_icon(&cache, &ctx, "halod-no-such-icon-zzz").is_none());
        assert!(cache.lock().unwrap().contains_key("halod-no-such-icon-zzz"));
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

    #[cfg(unix)]
    #[test]
    fn parse_icon_dir_size_extracts_dimension() {
        assert_eq!(parse_icon_dir_size("48x48"), 48);
        assert_eq!(parse_icon_dir_size("256x256"), 256);
        assert_eq!(parse_icon_dir_size("scalable"), 0);
        assert_eq!(parse_icon_dir_size(""), 0);
    }

    #[cfg(unix)]
    #[test]
    fn icon_ranker_precedence() {
        use std::path::PathBuf;

        // Raster always wins over svg and pixmap.
        let mut r = IconRanker::default();
        r.offer_svg(PathBuf::from("a.svg"));
        r.offer_pixmap(PathBuf::from("a.png"));
        r.offer_raster(48, PathBuf::from("48.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("48.png")));

        // Largest raster wins regardless of offer order.
        let mut r = IconRanker::default();
        r.offer_raster(48, PathBuf::from("48.png"));
        r.offer_raster(256, PathBuf::from("256.png"));
        r.offer_raster(32, PathBuf::from("32.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("256.png")));

        // Svg wins over pixmap when no raster present; first svg kept.
        let mut r = IconRanker::default();
        r.offer_svg(PathBuf::from("first.svg"));
        r.offer_svg(PathBuf::from("second.svg"));
        r.offer_pixmap(PathBuf::from("p.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("first.svg")));

        // Pixmap is the last resort; first pixmap kept.
        let mut r = IconRanker::default();
        r.offer_pixmap(PathBuf::from("first.png"));
        r.offer_pixmap(PathBuf::from("second.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("first.png")));

        // Nothing offered yields None.
        assert_eq!(IconRanker::default().winner(), None);
    }
}

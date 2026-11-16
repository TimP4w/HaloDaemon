// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform-adaptive title bar: window controls (compact flat buttons, or
//! full-height caption buttons on Windows), the logo, and the profile
//! button. Also hosts the daemon-down overlay and the native-drag
//! pointer-release workaround shared by both window backends.

use crate::domain::topic_store::TopicStore;
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
use crate::ui::screens::profile;
use crate::ui::theme::{self, a};

/// Window-chrome style for the title-bar controls, auto-detected per OS so the
/// buttons match platform convention.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChromeStyle {
    /// Compact flat buttons (the non-Windows default).
    Flat,
    /// Windows-style flat, full-height caption buttons.
    Windows,
}

impl ChromeStyle {
    /// The native style for the host OS.
    pub fn detect() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Flat
        }
    }
}

/// The three window controls.
#[derive(Clone, Copy, PartialEq, Debug)]
enum WinCtl {
    Minimize,
    Maximize,
    Close,
}

impl WinCtl {
    fn id(self) -> &'static str {
        match self {
            WinCtl::Minimize => "wc_min",
            WinCtl::Maximize => "wc_max",
            WinCtl::Close => "wc_close",
        }
    }

    fn perform(self, ctx: &egui::Context) {
        match self {
            WinCtl::Minimize => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
            WinCtl::Maximize => {
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            WinCtl::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }
}

/// Which side of the title bar the window controls sit on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Side {
    // Only produced by the GNOME (Linux) button-layout parse; the layout code
    // still matches it on every platform, so it is never constructed on Windows.
    #[allow(dead_code)]
    Left,
    Right,
}

/// The default control order (left-to-right) when no desktop preference applies:
/// minimize, maximize, close.
const DEFAULT_ORDER: [WinCtl; 3] = [WinCtl::Minimize, WinCtl::Maximize, WinCtl::Close];

/// Resolved window-control chrome: the button style, the ordered controls to draw
/// (left-to-right; empty when hidden), and which side of the bar they sit on.
#[derive(Clone, Debug, PartialEq)]
pub struct ChromeConfig {
    style: ChromeStyle,
    buttons: Vec<WinCtl>,
    side: Side,
}

impl ChromeConfig {
    /// Resolve the chrome for this session: the native style/side/order (honoring
    /// the GNOME `button-layout` preference where available), with the buttons
    /// cleared when the user has hidden the window controls.
    fn resolve(hide: bool) -> Self {
        let mut cfg = native_chrome();
        if hide {
            cfg.buttons.clear();
        }
        cfg
    }
}

/// Shared, live-updated native chrome. Seeded on first read from the OS; on GNOME
/// a background `gsettings monitor` thread rewrites it whenever the user changes
/// their `button-layout` preference, so the title bar reflects it without a
/// restart (the bar repaints every frame for the logo animation, so the next
/// frame picks up the change).
static NATIVE_CHROME: std::sync::OnceLock<std::sync::RwLock<ChromeConfig>> =
    std::sync::OnceLock::new();

/// The current native chrome for the host. On GNOME this tracks the live
/// `button-layout` gsetting (order + side + which buttons); everywhere else it is
/// the platform default (minimize, maximize, close on the right).
fn native_chrome() -> ChromeConfig {
    let lock = NATIVE_CHROME.get_or_init(|| {
        let cfg = detect_native_chrome();
        #[cfg(target_os = "linux")]
        spawn_gnome_chrome_watcher();
        std::sync::RwLock::new(cfg)
    });
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Watch GNOME's `button-layout` for changes and refresh [`NATIVE_CHROME`] live.
/// No-op off GNOME (or when `gsettings` is unavailable); the watcher thread runs
/// for the lifetime of the process.
#[cfg(target_os = "linux")]
fn spawn_gnome_chrome_watcher() {
    if gnome_button_layout().is_none() {
        return;
    }
    std::thread::spawn(|| {
        use std::io::{BufRead, BufReader};
        let mut child = match std::process::Command::new("gsettings")
            .args([
                "monitor",
                "org.gnome.desktop.wm.preferences",
                "button-layout",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        // Each printed line signals a change; re-read the setting for the new value.
        for _ in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(lock) = NATIVE_CHROME.get() {
                *lock.write().unwrap() = detect_native_chrome();
            }
        }
    });
}

fn detect_native_chrome() -> ChromeConfig {
    let style = ChromeStyle::detect();
    #[cfg(target_os = "linux")]
    if let Some(layout) = gnome_button_layout() {
        let (side, buttons) = parse_gnome_button_layout(&layout);
        return ChromeConfig {
            style,
            buttons,
            side,
        };
    }
    ChromeConfig {
        style,
        buttons: DEFAULT_ORDER.to_vec(),
        side: Side::Right,
    }
}

/// Parse a GNOME `button-layout` value (e.g. `"appmenu:minimize,maximize,close"`)
/// into the side the window controls sit on and their left-to-right order. Tokens
/// that aren't window controls (`appmenu`, `menu`, `icon`, `spacer`, …) are
/// ignored. A layout with controls on the right wins the side; otherwise the left
/// group is used. An empty result means "no controls" — a valid preference.
// GNOME desktop layout parsing — only used by the Linux native-chrome detection
// (and its cross-platform unit tests).
#[cfg(any(target_os = "linux", test))]
fn parse_gnome_button_layout(layout: &str) -> (Side, Vec<WinCtl>) {
    let (left, right) = layout.split_once(':').unwrap_or((layout, ""));
    let group = |g: &str| -> Vec<WinCtl> {
        g.split(',')
            .filter_map(|t| match t.trim() {
                "minimize" => Some(WinCtl::Minimize),
                "maximize" => Some(WinCtl::Maximize),
                "close" => Some(WinCtl::Close),
                _ => None,
            })
            .collect()
    };
    let right_group = group(right);
    if !right_group.is_empty() {
        (Side::Right, right_group)
    } else {
        (Side::Left, group(left))
    }
}

/// Read GNOME's `button-layout` preference, or `None` when not on a GNOME-family
/// desktop (or `gsettings` is unavailable). The value is unquoted before return.
#[cfg(target_os = "linux")]
fn gnome_button_layout() -> Option<String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let is_gnome = desktop.split(':').any(|d| {
        d.eq_ignore_ascii_case("gnome")
            || d.eq_ignore_ascii_case("unity")
            || d.eq_ignore_ascii_case("gnome-flashback")
    });
    if !is_gnome {
        return None;
    }
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.wm.preferences", "button-layout"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    // gsettings prints the value quoted, e.g. `'appmenu:minimize,maximize,close'`.
    Some(s.trim().trim_matches(['\'', '"']).to_string())
}

/// Lay out the window controls within the title bar `bar`. Returns the ordered
/// (control, hit-rect) pairs and the horizontal span `(min_x, max_x)` the group
/// occupies, or `None` when there are no controls (so other chrome — the logo and
/// profile button — can be kept clear of them).
type WindowControlRects = Vec<(WinCtl, Rect)>;
type HorizontalSpan = Option<(f32, f32)>;

fn window_controls_layout(bar: Rect, cfg: &ChromeConfig) -> (WindowControlRects, HorizontalSpan) {
    let n = cfg.buttons.len();
    if n == 0 {
        return (Vec::new(), None);
    }
    let rects: Vec<(WinCtl, Rect)> = match cfg.style {
        ChromeStyle::Flat => {
            const W: f32 = 36.0;
            const H: f32 = 32.0;
            const GAP: f32 = 2.0;
            const EDGE: f32 = 12.0;
            let top = bar.center().y - H / 2.0;
            cfg.buttons
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    let left = match cfg.side {
                        Side::Right => {
                            bar.right() - EDGE - (n - i) as f32 * W - (n - 1 - i) as f32 * GAP
                        }
                        Side::Left => bar.left() + EDGE + i as f32 * (W + GAP),
                    };
                    (
                        c,
                        Rect::from_min_size(Pos2::new(left, top), Vec2::new(W, H)),
                    )
                })
                .collect()
        }
        ChromeStyle::Windows => {
            const W: f32 = 46.0;
            // Full-height columns, contiguous in list order, flush to the right edge.
            cfg.buttons
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    let right = bar.right() - W * (n - 1 - i) as f32;
                    (
                        c,
                        Rect::from_min_max(
                            Pos2::new(right - W, bar.top()),
                            Pos2::new(right, bar.bottom()),
                        ),
                    )
                })
                .collect()
        }
    };
    let span = span_of(&rects);
    (rects, span)
}

/// The horizontal extent `(min_left, max_right)` covered by a set of control rects.
fn span_of(rects: &[(WinCtl, Rect)]) -> Option<(f32, f32)> {
    let left = rects.iter().map(|(_, r)| r.left()).reduce(f32::min)?;
    let right = rects.iter().map(|(_, r)| r.right()).reduce(f32::max)?;
    Some((left, right))
}

/// Paint a compact flat window-control button: a hover fill behind a stroked
/// glyph, so the glyphs stay crisp regardless of the font.
fn paint_flat(p: &egui::Painter, rect: Rect, ctl: WinCtl, hovered: bool, maximized: bool) {
    if hovered {
        p.rect_filled(rect, theme::RADIUS_XS + 2.0, theme::ROW_ACTIVE);
    }
    let stroke = Stroke::new(1.3, theme::TEXT_DIM);
    let c = rect.center();
    match ctl {
        WinCtl::Minimize => {
            p.line_segment(
                [Pos2::new(c.x - 4.0, c.y), Pos2::new(c.x + 4.0, c.y)],
                stroke,
            );
        }
        WinCtl::Maximize if maximized => {
            let d = 1.5;
            let front =
                Rect::from_min_size(Pos2::new(c.x - 3.5, c.y - 3.5 + d), Vec2::splat(7.0 - d));
            p.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
            let tl = Pos2::new(front.left() + d, front.top() - d);
            let tr = Pos2::new(front.right() + d, front.top() - d);
            let br = Pos2::new(front.right() + d, front.bottom() - d);
            p.line_segment([tl, tr], stroke);
            p.line_segment([tr, br], stroke);
        }
        WinCtl::Maximize => {
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(7.0)),
                0.0,
                stroke,
                egui::StrokeKind::Middle,
            );
        }
        WinCtl::Close => {
            let s = 3.0;
            p.line_segment(
                [Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)],
                stroke,
            );
            p.line_segment(
                [Pos2::new(c.x + s, c.y - s), Pos2::new(c.x - s, c.y + s)],
                stroke,
            );
        }
    }
}

/// Paint a Windows-style flat, full-height caption button with a stroked glyph.
/// The close button flushes to the window's rounded top-right corner and turns
/// red on hover; minimize/maximize get a subtle overlay.
fn paint_windows(p: &egui::Painter, rect: Rect, ctl: WinCtl, hovered: bool, maximized: bool) {
    if hovered {
        let (fill, radius) = match ctl {
            WinCtl::Close => (
                theme::OFFLINE,
                egui::CornerRadius {
                    nw: 0,
                    ne: 12,
                    sw: 0,
                    se: 0,
                },
            ),
            _ => (a(Color32::WHITE, 0.06), egui::CornerRadius::same(0)),
        };
        p.rect_filled(rect, radius, fill);
    }
    let col = if hovered && ctl == WinCtl::Close {
        Color32::WHITE
    } else {
        theme::TEXT_MUT
    };
    let stroke = Stroke::new(1.0, col);
    let c = rect.center();
    let s = 5.0_f32; // half glyph extent
    match ctl {
        WinCtl::Minimize => {
            p.line_segment([Pos2::new(c.x - s, c.y), Pos2::new(c.x + s, c.y)], stroke);
        }
        WinCtl::Maximize if maximized => {
            // "Restore" glyph: a front square with a second square peeking behind.
            let d = 2.0;
            let front =
                Rect::from_min_size(Pos2::new(c.x - s, c.y - s + d), Vec2::splat(s * 2.0 - d));
            p.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
            let tl = Pos2::new(front.left() + d, front.top() - d);
            let tr = Pos2::new(front.right() + d, front.top() - d);
            let br = Pos2::new(front.right() + d, front.bottom() - d);
            p.line_segment([tl, tr], stroke);
            p.line_segment([tr, br], stroke);
        }
        WinCtl::Maximize => {
            let sq = Rect::from_center_size(c, Vec2::splat(s * 2.0));
            p.rect_stroke(sq, 0.0, stroke, egui::StrokeKind::Middle);
        }
        WinCtl::Close => {
            p.line_segment(
                [Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)],
                stroke,
            );
            p.line_segment(
                [Pos2::new(c.x + s, c.y - s), Pos2::new(c.x - s, c.y + s)],
                stroke,
            );
        }
    }
}

fn title_bar_chrome(
    ui: &mut egui::Ui,
    state: &TopicStore,
) -> (ChromeConfig, Option<(f32, f32)>, f32) {
    let rect = ui.max_rect();

    // Drag the whole bar to move the window; double-click toggles maximize.
    let drag = ui.interact(
        rect,
        egui::Id::unique("title_drag"),
        Sense::click_and_drag(),
    );
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        arm_pointer_release_workaround(ui.ctx());
    }
    if drag.double_clicked() {
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }

    let p = ui.painter();
    // bottom divider
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::DIVIDER),
    );

    let cy = rect.center().y;

    // Window controls (minimize / maximize / close). Style/order/side follow the
    // desktop: flat caption buttons on Windows, compact flat buttons elsewhere; on
    // GNOME the `button-layout` preference decides which buttons show, their order
    // and side. The user can hide them entirely (tiling window managers).
    let cfg = ChromeConfig::resolve(state.gui.hide_window_controls);
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    let (buttons, ctl_span) = window_controls_layout(rect, &cfg);
    for (ctl, btn_rect) in &buttons {
        let resp = ui.interact(*btn_rect, egui::Id::unique(ctl.id()), Sense::click());
        match cfg.style {
            ChromeStyle::Flat => paint_flat(p, *btn_rect, *ctl, resp.hovered(), maximized),
            ChromeStyle::Windows => paint_windows(p, *btn_rect, *ctl, resp.hovered(), maximized),
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            ctl.perform(ui.ctx());
        }
    }

    let logo_left = match (cfg.side, ctl_span) {
        (Side::Left, Some((_, right))) => right + 20.0,
        _ => rect.left() + 20.0,
    };
    let logo_size = crate::ui::components::logo_size(p, 18.0, 14.0);
    crate::ui::components::paint_logo(
        p,
        ui.ctx(),
        Pos2::new(logo_left, cy - logo_size.y / 2.0),
        18.0,
        14.0,
    );

    (cfg, ctl_span, logo_left + logo_size.x)
}

/// The title bar without the profile button — window controls and logo only.
/// Used by the discovery radar overlay, which has no active profile to show.
pub fn title_bar_plain(ui: &mut egui::Ui, state: &TopicStore) {
    let _ = title_bar_chrome(ui, state);
}

pub fn title_bar(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    profile_ui: &mut profile::ProfileUi,
    page: &mut Page,
    udev_status: Option<&halod_shared::types::UdevRulesStatus>,
    toasts: &mut crate::ui::components::toast::Toasts,
) {
    let rect = ui.max_rect();
    let cy = rect.center().y;
    let (cfg, ctl_span, logo_right) = title_bar_chrome(ui, state);
    super::nav::nav_tabs(ui, logo_right + 20.0, cy, state, page, udev_status);

    // Profile button — right edge sits clear of any right-side window controls,
    // otherwise it hugs the right edge.
    let profile_right = match (cfg.side, ctl_span) {
        (Side::Right, Some((left, _))) => left - 20.0,
        _ => rect.right() - 12.0,
    };
    let btn_rect = profile::title_button(ui, state, profile_ui, profile_right, cy);
    super::bell::bell(ui, btn_rect.left() - 20.0, cy, toasts);
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::HomeProfile,
        btn_rect,
    );

    // Dropdown rendered as a Foreground Area (must be called every frame)
    profile::title_dropdown(ui.ctx(), state, cmd, profile_ui, page, btn_rect);

    // Delete-profile confirm dialog (no-op until a delete is pending).
    profile::delete_confirm_modal(ui.ctx(), cmd, profile_ui);
}

/// Full-content scrim shown over the main panel while the daemon is unreachable:
/// a dimmed backdrop and a centered card explaining the state with a button to
/// start the daemon. Drawn last so it sits above whatever page is behind it.
pub fn daemon_overlay(ui: &mut egui::Ui) {
    let area = ui.max_rect();
    // Swallow all pointer input over the content area so clicks/drags don't fall
    // through to the page rendered behind the scrim. Registered before the card,
    // so its buttons are allocated on top and stay clickable.
    ui.interact(
        area,
        ui.scope_id().with("daemon_overlay_barrier"),
        Sense::click_and_drag(),
    );
    ui.painter().rect_filled(area, 0.0, a(theme::MAIN_BG, 0.82));
    let card = Rect::from_center_size(area.center(), Vec2::new(512.0, 230.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::NONE
            .fill(theme::MODAL_BG)
            .stroke(Stroke::new(1.0, theme::BORDER_INNER))
            .corner_radius(theme::RADIUS_XL)
            .inner_margin(egui::Margin::same(26))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(t!("shell.daemon_not_running"))
                        .font(theme::semibold(20.0))
                        .color(theme::TEXT),
                );
                ui.add_space(theme::SPACE_5);
                ui.label(
                    egui::RichText::new(t!("shell.daemon_unreachable"))
                        .font(theme::body_lg())
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(theme::SPACE_5);
                ui.label(
                    egui::RichText::new(t!("shell.daemon_reconnecting"))
                        .font(theme::body_md())
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(theme::SPACE_10);
                ui.horizontal(|ui| {
                    let size = Vec2::new(0.0, 36.0);
                    use crate::ui::components::{button, ButtonKind};
                    if button(ui, &t!("shell.start_daemon"), ButtonKind::Primary, size).clicked() {
                        crate::domain::lifecycle::ensure_daemon_up();
                    }
                    if button(ui, &t!("shell.open_log"), ButtonKind::Secondary, size).clicked() {
                        crate::ui::screens::settings::open_path(
                            &halod_shared::app::config_dir()
                                .join(crate::ui::screens::settings::LOG_FILE)
                                .display()
                                .to_string(),
                        );
                    }
                });
            });
    });
}

/// Memory key for [`arm_pointer_release_workaround`].
fn pointer_release_id() -> egui::Id {
    egui::Id::unique("halod_native_drag_pointer_release")
}

/// Workaround for egui#7959: a native window drag (`StartDrag`) or resize
/// (`BeginResize`) leaves egui's primary pointer button stuck "down" because the
/// window manager swallows the button-release while it owns the move/resize loop
/// (notably on Wayland). egui then thinks a drag is still in progress, so every
/// *other* subsequent drag/click does nothing. We arm a synthetic release here;
/// [`take_pending_pointer_release`] injects it on the next frame's raw input.
pub fn arm_pointer_release_workaround(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(pointer_release_id(), true));
}

/// Consumes the flag set by [`arm_pointer_release_workaround`], returning whether
/// a synthetic pointer release should be injected this frame.
pub fn take_pending_pointer_release(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(pointer_release_id()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_matches_host_os() {
        let s = ChromeStyle::detect();
        if cfg!(target_os = "windows") {
            assert_eq!(s, ChromeStyle::Windows);
        } else {
            assert_eq!(s, ChromeStyle::Flat);
        }
    }

    fn cfg(style: ChromeStyle, side: Side, buttons: &[WinCtl]) -> ChromeConfig {
        ChromeConfig {
            style,
            side,
            buttons: buttons.to_vec(),
        }
    }

    #[test]
    fn windows_controls_are_full_height_contiguous_and_flush_right() {
        let bar = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 46.0));
        let (buttons, span) =
            window_controls_layout(bar, &cfg(ChromeStyle::Windows, Side::Right, &DEFAULT_ORDER));
        let rect_of = |c: WinCtl| buttons.iter().find(|(k, _)| *k == c).unwrap().1;
        let (min, max, close) = (
            rect_of(WinCtl::Minimize),
            rect_of(WinCtl::Maximize),
            rect_of(WinCtl::Close),
        );
        // Close is rightmost, flush to the bar's right edge.
        assert_eq!(close.right(), bar.right());
        // Every button spans the full title-bar height and is one column wide.
        for (_, r) in &buttons {
            assert_eq!(r.top(), bar.top());
            assert_eq!(r.bottom(), bar.bottom());
            assert_eq!(r.width(), 46.0);
        }
        // Contiguous (no gaps or overlaps), left-to-right: min | max | close.
        assert_eq!(min.right(), max.left());
        assert_eq!(max.right(), close.left());
        assert_eq!(span, Some((bar.right() - 138.0, bar.right())));
    }

    #[test]
    fn flat_controls_are_minimize_maximize_close_left_to_right() {
        let bar = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 46.0));
        let (buttons, span) =
            window_controls_layout(bar, &cfg(ChromeStyle::Flat, Side::Right, &DEFAULT_ORDER));
        // Order left-to-right is minimize, maximize, close.
        let order: Vec<WinCtl> = buttons.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            order,
            vec![WinCtl::Minimize, WinCtl::Maximize, WinCtl::Close]
        );
        // Left-to-right in x, and all within the bar.
        for w in buttons.windows(2) {
            assert!(w[0].1.center().x < w[1].1.center().x);
        }
        for (_, r) in &buttons {
            assert!(r.right() <= bar.right());
        }
        // Close hugs the right edge; span's right matches the rightmost button.
        assert_eq!(span.unwrap().1, buttons.last().unwrap().1.right());
    }

    #[test]
    fn left_side_flat_controls_start_at_the_left_edge_in_order() {
        let bar = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 46.0));
        let order = [WinCtl::Close, WinCtl::Minimize, WinCtl::Maximize];
        let (buttons, span) =
            window_controls_layout(bar, &cfg(ChromeStyle::Flat, Side::Left, &order));
        assert_eq!(
            buttons.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
            order.to_vec()
        );
        // Leftmost button sits near the left edge; ordered left-to-right.
        assert!(span.unwrap().0 >= bar.left());
        for w in buttons.windows(2) {
            assert!(w[0].1.center().x < w[1].1.center().x);
        }
    }

    #[test]
    fn hidden_controls_produce_no_buttons_and_no_span() {
        let bar = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 46.0));
        let (buttons, span) =
            window_controls_layout(bar, &cfg(ChromeStyle::Flat, Side::Right, &[]));
        assert!(buttons.is_empty());
        assert_eq!(span, None);
    }

    #[test]
    fn gnome_button_layout_default_puts_controls_on_the_right() {
        let (side, buttons) = parse_gnome_button_layout("appmenu:minimize,maximize,close");
        assert_eq!(side, Side::Right);
        assert_eq!(
            buttons,
            vec![WinCtl::Minimize, WinCtl::Maximize, WinCtl::Close]
        );
    }

    #[test]
    fn gnome_button_layout_honors_left_placement_and_order() {
        let (side, buttons) = parse_gnome_button_layout("close,minimize,maximize:appmenu");
        assert_eq!(side, Side::Left);
        assert_eq!(
            buttons,
            vec![WinCtl::Close, WinCtl::Minimize, WinCtl::Maximize]
        );
    }

    #[test]
    fn gnome_button_layout_hides_and_drops_unknown_tokens() {
        // No window controls at all → empty (a valid "hidden" preference).
        assert_eq!(parse_gnome_button_layout("appmenu:").1, Vec::new());
        assert_eq!(parse_gnome_button_layout("icon:menu").1, Vec::new());
        // A subset is honored; unknown tokens (spacer/appmenu) are ignored.
        let (side, buttons) = parse_gnome_button_layout(":spacer,maximize,close");
        assert_eq!(side, Side::Right);
        assert_eq!(buttons, vec![WinCtl::Maximize, WinCtl::Close]);
        // No colon at all is treated as the left group.
        assert_eq!(
            parse_gnome_button_layout("minimize,close"),
            (Side::Left, vec![WinCtl::Minimize, WinCtl::Close])
        );
    }

    #[test]
    fn pointer_release_workaround_is_armed_then_consumed_once() {
        let ctx = egui::Context::default();
        // Nothing armed by default.
        assert!(!take_pending_pointer_release(&ctx));
        // Arming makes exactly one take succeed; the flag is one-shot.
        arm_pointer_release_workaround(&ctx);
        assert!(take_pending_pointer_release(&ctx));
        assert!(!take_pending_pointer_release(&ctx));
    }
}

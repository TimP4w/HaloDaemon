// SPDX-License-Identifier: GPL-3.0-or-later
//! Main navigation tabs shown in the title bar, with their attention badges.

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Color32, Pos2, Rect, Sense, Vec2};
use halod_shared::types::{FanCurveStatus, PluginIssueKind, UdevRulesStatus};

use crate::domain::models::device as model;
use crate::domain::models::udev::udev_rules_need_action;
use crate::domain::state::Page;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::ui::screens::plugin_integration;
use crate::ui::theme::{self, a};

const NAV: [Page; 5] = [
    Page::Home,
    Page::Lighting,
    Page::Cooling,
    Page::Plugins,
    Page::Settings,
];

/// Plugins with an issue, an available update, or a setup still to do.
pub fn plugins_needing_action(state: &TopicStore) -> usize {
    state
        .plugins
        .plugins
        .iter()
        .filter(|p| {
            p.health.issue.is_some()
                || plugin_integration::awaits_setup(p)
                || crate::ui::screens::plugins::update_for(p, &state.plugins.available).is_some()
        })
        .count()
}

/// Whether the Plugins attention badge includes a real failure. Load warnings,
/// updates, and pending setups remain amber; connect/runtime/load failures use
/// the same red severity color as their plugin issue banner.
fn plugins_have_errors(state: &TopicStore) -> bool {
    state.plugins.plugins.iter().any(|plugin| {
        plugin
            .health
            .issue
            .as_ref()
            .is_some_and(|issue| issue.kind != PluginIssueKind::LoadWarning)
    })
}

/// Count fan curves with an actionable status, such as no temperature sensor,
/// a sensor malfunction, a stalled fan, or a failed write. Healthy curves do
/// not add noise to the Cooling navigation item.
pub fn cooling_needing_action(state: &TopicStore) -> usize {
    state
        .cooling
        .fan_curves
        .iter()
        .filter(|curve| {
            // Hidden and disabled devices are intentionally out of scope for
            // the global Cooling attention hint. A missing device has no
            // visibility state, so its NoDevice status remains actionable.
            if state
                .devices
                .iter()
                .find(|device| device.id == curve.device_id)
                .is_some_and(model::is_hidden)
            {
                return false;
            }
            !matches!(
                &curve.status,
                FanCurveStatus::Ok | FanCurveStatus::Disconnected
            )
        })
        .count()
}

/// Pure layout for the top-bar tabs: one rect per label width, laid left to
/// right from `left` with a fixed gap, vertically centred on `cy`.
fn tab_rects(left: f32, cy: f32, label_widths: &[f32]) -> Vec<Rect> {
    let mut x = left;
    label_widths
        .iter()
        .map(|w| {
            let rect = Rect::from_min_size(
                Pos2::new(x, cy - TAB_H / 2.0),
                Vec2::new(w + 2.0 * TAB_PAD_X, TAB_H),
            );
            x = rect.right() + TAB_GAP;
            rect
        })
        .collect()
}

const TAB_H: f32 = 32.0;
const TAB_PAD_X: f32 = 12.0;
const TAB_GAP: f32 = 2.0;

/// The main navigation tabs painted into the title bar, starting at `left`.
/// Returns the right edge of the last tab.
pub fn nav_tabs(
    ui: &mut egui::Ui,
    left: f32,
    cy: f32,
    state: &TopicStore,
    page: &mut Page,
    udev_status: Option<&UdevRulesStatus>,
) -> f32 {
    let plugin_actions = plugins_needing_action(state)
        + usize::from(udev_status.is_some_and(udev_rules_need_action));
    let plugin_errors = plugins_have_errors(state);
    let cooling_actions = cooling_needing_action(state);

    let labels: Vec<_> = NAV.iter().map(|target| nav_label(target)).collect();
    let font = |active: bool| {
        if active {
            theme::medium(14.0)
        } else {
            theme::body(14.0)
        }
    };
    let widths: Vec<f32> = NAV
        .iter()
        .zip(&labels)
        .map(|(target, l)| {
            ui.painter()
                .layout_no_wrap(l.to_string(), font(nav_active(page, target)), theme::TEXT)
                .size()
                .x
        })
        .collect();
    let rects = tab_rects(left, cy, &widths);
    for ((target, label), rect) in NAV.iter().zip(&labels).zip(&rects) {
        let resp = ui.interact(
            *rect,
            ui.scope_id().with(("nav", label.as_ref())),
            Sense::click(),
        );
        let active = nav_active(page, target);
        if active {
            ui.painter()
                .rect_filled(*rect, theme::RADIUS_SM, theme::ROW_ACTIVE);
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(*rect, theme::RADIUS_SM, a(theme::ROW_ACTIVE, 0.6));
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font(active),
            if active || resp.hovered() {
                theme::NAV_TEXT_ACTIVE
            } else {
                theme::NAV_TEXT
            },
        );
        let (badge, color) = match target {
            Page::Plugins if plugin_errors => (plugin_actions, theme::TRAFFIC_RED),
            Page::Plugins => (plugin_actions, theme::STAT_AMBER),
            Page::Cooling => (cooling_actions, theme::STAT_AMBER),
            _ => (0, theme::STAT_AMBER),
        };
        if badge > 0 {
            draw_badge(ui, *rect, badge, color);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            *page = target.clone();
        }
        let anchor = match target {
            Page::Lighting => AnchorId::HomeSidebarLighting,
            Page::Cooling => AnchorId::HomeSidebarCooling,
            Page::Plugins => AnchorId::HomeSidebarPlugins,
            Page::Settings => AnchorId::HomeSidebarSettings,
            _ => AnchorId::HomeSidebarHome,
        };
        tour::anchor(ui.ctx(), anchor, *rect);
    }
    rects.last().map_or(left, |r| r.right())
}

/// Device pages and the effect designer live under Home and Lighting.
fn nav_active(page: &Page, target: &Page) -> bool {
    match page {
        Page::Device(_) | Page::Profile(_) => *target == Page::Home,
        Page::EffectDesigner => *target == Page::Lighting,
        _ => page == target,
    }
}

fn nav_label(page: &Page) -> std::borrow::Cow<'static, str> {
    match page {
        Page::Lighting => t!("shell.nav_lighting"),
        Page::Cooling => t!("shell.nav_cooling"),
        Page::Plugins => t!("shell.nav_plugins"),
        Page::Settings => t!("shell.nav_settings"),
        _ => t!("shell.nav_home"),
    }
}

/// Count pill on a tab's top-right corner.
fn draw_badge(ui: &mut egui::Ui, tab: Rect, count: usize, color: Color32) {
    let label = if count > 9 {
        "9+".to_string()
    } else {
        count.to_string()
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label, theme::semibold(10.0), theme::ON_ACCENT);
    let w = (galley.size().x + 8.0).max(16.0);
    let pill = Rect::from_center_size(
        Pos2::new(tab.right() - 2.0, tab.top() + 2.0),
        Vec2::new(w, 16.0),
    );
    ui.painter().rect_filled(pill, 8.0, color);
    ui.painter().galley(
        pill.center() - galley.size() / 2.0,
        galley,
        theme::ON_ACCENT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::PluginInfo;

    fn plugin(id: &str, enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.parse().unwrap(),
            name: id.into(),
            translations: Default::default(),
            path: String::new(),
            plugin_type: Default::default(),
            experimental: false,
            capabilities: vec![],
            platforms: vec![],
            platform_supported: true,
            effect_names: vec![],
            enabled,
            author: String::new(),
            version: String::new(),
            description: String::new(),
            targets: vec![],
            license: String::new(),
            devices: vec![],
            logo: None,
            effect_thumbnails: vec![],
            repo: "official".into(),
            provenance: Default::default(),
            uses: Default::default(),
            matches: Default::default(),
            provides: vec![],
            consumes: vec![],
            data_records: vec![],
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
            integration_enabled: true,
            integration_configured: true,
            integration_requires_setup: false,
            integration_state: halod_shared::types::IntegrationLifecycleState::Configured,
            integration_setup: None,
            active: enabled,
            requirements: vec![],
            activation_blocker: None,
            health: Default::default(),
        }
    }

    #[test]
    fn tabs_are_laid_out_left_to_right_without_overlap() {
        let rects = tab_rects(100.0, 26.0, &[40.0, 60.0, 30.0]);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].left(), 100.0);
        for pair in rects.windows(2) {
            assert!(pair[1].left() >= pair[0].right());
        }
        assert!(rects.iter().all(|r| (r.center().y - 26.0).abs() < 0.01));
    }

    #[test]
    fn device_and_profile_pages_highlight_home() {
        assert!(nav_active(&Page::Device("x".parse().unwrap()), &Page::Home));
        assert!(nav_active(&Page::EffectDesigner, &Page::Lighting));
        assert!(!nav_active(&Page::Cooling, &Page::Home));
    }

    #[test]
    fn plugins_needing_action_counts_issues_and_updates() {
        let mut state = TopicStore::default();
        let mut failing = plugin("failing", true);
        failing.health.issue = Some(halod_shared::types::PluginIssue {
            kind: PluginIssueKind::RuntimeError,
            detail: "boom".into(),
            context: None,
            timestamp_ms: 0,
        });
        let mut outdated = plugin("outdated", false);
        outdated.version = "1.0.0".into();
        state.plugins.plugins = vec![plugin("ok", true), failing, outdated];
        state.plugins.available = vec![halod_shared::types::AvailablePlugin {
            matches: Default::default(),
            repo: "official".into(),
            id: "outdated".parse().unwrap(),
            name: "Outdated".into(),
            version: "1.1.0".into(),
            api: "3.0".into(),
            compatible: true,
            uses: Default::default(),
        }];
        assert_eq!(plugins_needing_action(&state), 2);
    }

    #[test]
    fn plugins_needing_action_is_zero_when_all_clear() {
        let mut state = TopicStore::default();
        state.plugins.plugins = vec![plugin("a", true), plugin("b", true)];
        assert_eq!(plugins_needing_action(&state), 0);
    }

    #[test]
    fn integration_issues_count_toward_plugins() {
        let mut with_issue = plugin("failing", true);
        with_issue.plugin_type = halod_shared::types::PluginKind::Integration;
        with_issue.health.issue = Some(halod_shared::types::PluginIssue {
            kind: halod_shared::types::PluginIssueKind::ConnectFailed,
            detail: "boom".into(),
            context: None,
            timestamp_ms: 0,
        });
        let mut state = TopicStore::default();
        state.plugins.plugins = vec![with_issue, plugin("ok", true)];
        assert_eq!(plugins_needing_action(&state), 1);
        assert!(plugins_have_errors(&state));
    }

    #[test]
    fn an_enabled_integration_awaiting_setup_needs_action() {
        let mut waiting = plugin("waiting", true);
        waiting.plugin_type = halod_shared::types::PluginKind::Integration;
        waiting.integration_configured = false;
        let mut off = waiting.clone();
        off.id = "off".parse().unwrap();
        off.enabled = false;
        let mut state = TopicStore::default();
        state.plugins.plugins = vec![waiting, off];
        assert_eq!(plugins_needing_action(&state), 1);
        assert!(!plugins_have_errors(&state));
    }

    #[test]
    fn plugin_badge_is_red_for_errors_but_not_load_warnings() {
        let mut affected = plugin("affected", true);
        affected.health.issue = Some(halod_shared::types::PluginIssue {
            kind: PluginIssueKind::LoadWarning,
            detail: "warning".into(),
            context: None,
            timestamp_ms: 0,
        });
        let mut state = TopicStore::default();
        state.plugins.plugins = vec![affected.clone()];
        assert!(!plugins_have_errors(&state));

        affected.health.issue.as_mut().unwrap().kind = PluginIssueKind::RuntimeError;
        state.plugins.plugins = vec![affected];
        assert!(plugins_have_errors(&state));
    }

    #[test]
    fn plugins_needing_action_ignores_skipped_plugins() {
        let mut state = TopicStore::default();
        state.plugins.plugins = vec![plugin("ok", true)];
        state.plugins.skipped = vec![halod_shared::types::SkippedPlugin {
            path: "/a/broken".into(),
            reason: "bad yaml".into(),
        }];
        assert_eq!(plugins_needing_action(&state), 0);
    }

    #[test]
    fn cooling_needing_action_counts_only_unhealthy_curves() {
        let mut state = TopicStore::default();
        state.cooling.fan_curves = vec![
            halod_shared::types::WireFanCurve {
                device_id: "ok".parse().unwrap(),
                channel_id: "default".into(),
                sensor_id: Some("temp".into()),
                points: vec![],
                status: FanCurveStatus::Ok,
            },
            halod_shared::types::WireFanCurve {
                device_id: "missing-sensor".parse().unwrap(),
                channel_id: "default".into(),
                sensor_id: None,
                points: vec![],
                status: FanCurveStatus::NoSensor,
            },
            halod_shared::types::WireFanCurve {
                device_id: "disabled".parse().unwrap(),
                channel_id: "default".into(),
                sensor_id: None,
                points: vec![],
                status: FanCurveStatus::FanStalled,
            },
        ];
        let disabled = halod_shared::types::WireDevice {
            id: "disabled".parse().unwrap(),
            active_state: halod_shared::types::VisibilityState::Disabled,
            ..Default::default()
        };
        state.devices.push(disabled);
        assert_eq!(cooling_needing_action(&state), 1);
    }
}

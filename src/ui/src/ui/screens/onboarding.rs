// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::domain::topic_store::TopicStore;
use egui::{RichText, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::debug_info::DebugInfo;
use halod_shared::types::{AvailablePlugin, PluginRecommendation, PluginsState};

use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;

const WELCOME: u8 = 0;
const HEALTH: u8 = 1;
const PREFS: u8 = 2;
const SCANNING: u8 = 3;
const PLUGINS: u8 = 4;
const DONE: u8 = 5;
const OFFICIAL_REPO_SLUG: &str = "official";
const COLUMN_W: f32 = 640.0;
const LIST_MAX_H: f32 = 360.0;

/// How long the cosmetic scan animation runs before showing recommendations
/// (they are already computed at startup; this is a settle beat, not real work).
const SCAN_SECS: f64 = 2.2;

#[derive(Default)]
pub struct OnboardingUi {
    step: u8,
    scan_started: Option<f64>,
    refresh_started: Option<f64>,
    /// Plugin id → selected for installation.
    selected: HashMap<String, bool>,
    start_on_boot: Option<bool>,
}

impl OnboardingUi {
    /// The wizard opened on step `step` (0 = welcome … 5 = done).
    #[cfg(all(test, feature = "screenshots"))]
    pub fn at_step(step: u8) -> Self {
        Self {
            step,
            ..Default::default()
        }
    }
}

/// Whether the wizard was explicitly completed this frame, so the caller can
/// persist "seen" and seed the recommendation dedup set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Finished,
}

fn is_selected(st: &OnboardingUi, id: &str) -> bool {
    st.selected.get(id).copied().unwrap_or(false)
}

/// Official plugins that can be installed on this build and aren't yet.
fn plugin_candidates(plugins: &PluginsState) -> Vec<&AvailablePlugin> {
    plugins
        .available
        .iter()
        .filter(|entry| {
            entry.repo == OFFICIAL_REPO_SLUG
                && entry.compatible
                && !plugins.plugins.iter().any(|p| p.id == entry.id)
        })
        .collect()
}

fn seed_plugin_selection(
    st: &mut OnboardingUi,
    recommendations: &[PluginRecommendation],
    plugins: &PluginsState,
) {
    for entry in plugin_candidates(plugins) {
        if recommendations.iter().any(|r| r.plugin_id == entry.id) {
            // Absent already means off; inserting false now would make the
            // pending recommendation snapshot look like an explicit opt-out.
            st.selected.entry(entry.id.clone()).or_insert(true);
        }
    }
}

fn active_count(st: &OnboardingUi, plugins: &PluginsState) -> usize {
    install_targets(st, plugins).len()
}

/// The primary button label for a step (pure, tested).
fn primary_label(step: u8, active: usize) -> String {
    match step {
        WELCOME => t!("onboarding.p_get_started").to_string(),
        HEALTH => t!("onboarding.p_continue").to_string(),
        PREFS => t!("onboarding.p_scan").to_string(),
        PLUGINS => {
            if active > 0 {
                t!("onboarding.p_install", n = active).to_string()
            } else {
                t!("onboarding.p_finish").to_string()
            }
        }
        DONE => t!("onboarding.p_enter").to_string(),
        _ => t!("onboarding.p_continue").to_string(),
    }
}

/// Which setup step the rail highlights; scanning belongs to Plugins.
fn active_dot(step: u8) -> usize {
    match step {
        WELCOME => 0,
        HEALTH => 1,
        PREFS => 2,
        SCANNING | PLUGINS => 3,
        _ => 4,
    }
}

fn install_targets(st: &OnboardingUi, plugins: &PluginsState) -> Vec<String> {
    plugin_candidates(plugins)
        .into_iter()
        .filter(|entry| is_selected(st, &entry.id))
        .map(|entry| entry.id.clone())
        .collect()
}

pub fn show(
    ctx: &egui::Context,
    state: &TopicStore,
    debug: Option<&DebugInfo>,
    cmd: &CommandTx,
    st: &mut OnboardingUi,
    time: f64,
) -> Outcome {
    let recs = &state.plugins.recommendations;
    seed_plugin_selection(st, recs, &state.plugins);

    // Drive the scan timer.
    if st.step == SCANNING {
        let started = *st.scan_started.get_or_insert(time);
        if time - started >= SCAN_SECS {
            st.step = PLUGINS;
            st.scan_started = None;
        } else {
            ctx.request_repaint();
        }
    }

    let mut outcome = Outcome::Pending;
    let area = egui::Modal::default_area(egui::Id::unique("onboarding_modal"))
        .anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 40.0));
    egui::Modal::new(egui::Id::unique("onboarding_modal"))
        .area(area)
        .backdrop_color(theme::BODY)
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            ui.set_width(COLUMN_W);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (logo, _) = ui.allocate_exact_size(Vec2::new(COLUMN_W, 22.0), egui::Sense::hover());
            let size = widgets::logo_size(ui.painter(), 22.0, 16.0);
            widgets::paint_logo(
                ui.painter(),
                ui.ctx(),
                egui::pos2(logo.center().x - size.x / 2.0, logo.top()),
                22.0,
                16.0,
            );
            ui.add_space(28.0);
            widgets::progress_steps(ui, &step_labels(), active_dot(st.step));
            ui.add_space(28.0);
            egui::Frame::NONE
                .fill(theme::CARD_BG)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(16.0)
                .show(ui, |ui| {
                    ui.set_width(COLUMN_W);
                    let (title, intro) = step_copy(st.step);
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: 28,
                            right: 28,
                            top: 28,
                            bottom: 18,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(title)
                                    .font(theme::semibold(24.0))
                                    .color(theme::TEXT),
                            );
                            ui.add_space(theme::SPACE_4);
                            ui.label(
                                RichText::new(intro)
                                    .font(theme::body_lg())
                                    .color(theme::TEXT_DIM),
                            );
                        });
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(28, 0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
                            match st.step {
                                WELCOME => step_welcome(ui),
                                HEALTH => step_health(ui, debug),
                                PREFS => step_prefs(ui, st, state, cmd),
                                SCANNING => step_scanning(ui),
                                PLUGINS => step_plugins(ui, st, state, cmd, time),
                                _ => step_done(ui, st, state),
                            }
                        });
                    ui.add_space(theme::SPACE_8);
                    widgets::divider(ui);
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(28, 22))
                        .show(ui, |ui| {
                            footer(
                                ui,
                                st,
                                active_count(st, &state.plugins),
                                &mut outcome,
                                state,
                                cmd,
                            );
                        });
                });
        });
    outcome
}

/// The card title and lede for a step.
fn step_copy(step: u8) -> (String, String) {
    let (title, intro) = match step {
        WELCOME => ("onboarding.welcome_title", "onboarding.welcome_body"),
        HEALTH => ("onboarding.health_title", "onboarding.health_sub"),
        PREFS => ("onboarding.prefs_title", "onboarding.prefs_sub"),
        SCANNING | PLUGINS => (
            "onboarding.plugins_page_title",
            "onboarding.plugins_page_sub",
        ),
        _ => ("onboarding.done_title", "onboarding.done_sub"),
    };
    (t!(title).into_owned(), t!(intro).into_owned())
}

/// A recessed list panel holding a step's rows.
fn inset_list(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    widgets::inset(ui, egui::Margin::symmetric(14, 0), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        body(ui);
    });
}

fn step_labels() -> Vec<String> {
    [
        "onboarding.step_welcome",
        "onboarding.step_system",
        "onboarding.step_prefs",
        "onboarding.step_plugins",
        "onboarding.step_done",
    ]
    .into_iter()
    .map(|key| t!(key).to_string())
    .collect()
}

fn step_welcome(ui: &mut egui::Ui) {
    for (title, body, glyph) in [
        (
            "onboarding.feature_lighting",
            "onboarding.feature_lighting_body",
            Glyph::Lighting,
        ),
        (
            "onboarding.feature_cooling",
            "onboarding.feature_cooling_body",
            Glyph::Cooling,
        ),
        (
            "onboarding.feature_profiles",
            "onboarding.feature_profiles_body",
            Glyph::Profiles,
        ),
    ] {
        widgets::inset(ui, egui::Margin::same(14), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                let (tile, _) = ui.allocate_exact_size(Vec2::splat(36.0), egui::Sense::hover());
                ui.painter().rect_filled(tile, 9.0, theme::SELECTED_BG);
                paint_glyph(ui.painter(), tile.center(), glyph);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new(t!(title))
                            .font(theme::semibold(14.0))
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new(t!(body))
                            .font(theme::body_md())
                            .color(theme::TEXT_DIM),
                    );
                });
            });
        });
    }
}

#[derive(Clone, Copy)]
enum Glyph {
    Lighting,
    Cooling,
    Profiles,
}

/// The 18 px feature glyphs on the welcome step.
fn paint_glyph(p: &egui::Painter, c: egui::Pos2, glyph: Glyph) {
    let stroke = Stroke::new(1.6, theme::LINK);
    match glyph {
        Glyph::Lighting => {
            p.circle_stroke(c, 6.5, stroke);
            p.circle_stroke(c, 2.5, stroke);
        }
        Glyph::Cooling => {
            for i in 0..4 {
                let ang = std::f32::consts::FRAC_PI_2 * i as f32 + 0.6;
                p.circle_stroke(c + Vec2::angled(ang) * 3.4, 3.2, stroke);
            }
        }
        Glyph::Profiles => {
            let r = egui::Rect::from_center_size(c, Vec2::new(12.0, 10.0));
            p.rect_stroke(r, 1.0, stroke, egui::StrokeKind::Middle);
            p.line_segment(
                [c + Vec2::new(-3.0, -1.5), c + Vec2::new(3.0, -1.5)],
                stroke,
            );
            p.line_segment([c + Vec2::new(-3.0, 1.5), c + Vec2::new(1.0, 1.5)], stroke);
        }
    }
}

fn step_health(ui: &mut egui::Ui, debug: Option<&DebugInfo>) {
    let Some(info) = debug else {
        ui.horizontal(|ui| {
            widgets::spinner(ui, 18.0);
            ui.label(
                RichText::new(t!("onboarding.health_checking"))
                    .font(theme::body_md())
                    .color(theme::TEXT_FAINT),
            );
        });
        return;
    };
    let failing = crate::ui::screens::depcheck::failing(info);
    let missing_optional = info
        .dependencies
        .iter()
        .filter(|d| !d.present && !d.required)
        .count();
    if failing.iter().all(|d| !d.required) {
        let text = if missing_optional == 0 {
            t!("onboarding.health_ok")
        } else {
            t!("onboarding.health_ok_optional", n = missing_optional)
        };
        widgets::Banner::ok(&text).show(ui);
    }
    egui::ScrollArea::vertical()
        .max_height(LIST_MAX_H)
        .show(ui, |ui| {
            inset_list(ui, |ui| {
                for dep in &info.dependencies {
                    let (tag, tone) = if dep.present {
                        (t!("onboarding.check_ready"), widgets::TagTone::Ok)
                    } else if dep.required {
                        (t!("onboarding.check_missing"), widgets::TagTone::Danger)
                    } else {
                        (t!("onboarding.check_missing"), widgets::TagTone::Warn)
                    };
                    let impact = crate::ui::screens::depcheck::impact_text(dep);
                    let sub = if dep.required {
                        impact
                    } else {
                        t!("onboarding.check_optional", impact = impact).into_owned()
                    };
                    widgets::list_row(
                        ui,
                        &crate::ui::screens::depcheck::title_text(dep),
                        &sub,
                        |ui| widgets::tag(ui, &tag, tone),
                    );
                }
            });
        });
}

fn step_prefs(ui: &mut egui::Ui, st: &mut OnboardingUi, state: &TopicStore, cmd: &CommandTx) {
    inset_list(ui, |ui| {
        crate::ui::screens::settings::start_on_boot_row(ui, &mut st.start_on_boot);
        widgets::list_row(
            ui,
            &t!("settings.close_to_tray_title"),
            &t!("settings.close_to_tray_sub"),
            |ui| {
                if widgets::toggle(ui, state.gui.close_to_tray) != state.gui.close_to_tray {
                    send_ui_config(cmd, state, !state.gui.close_to_tray);
                }
            },
        );
        crate::ui::screens::settings::language_row(ui, state, cmd, &t!("onboarding.language_sub"));
    });
}

fn step_scanning(ui: &mut egui::Ui) {
    ui.add_space(theme::SPACE_10);
    ui.vertical_centered(|ui| {
        widgets::spinner(ui, 28.0);
        ui.add_space(theme::SPACE_6);
        ui.label(
            RichText::new(t!("misc.startup_title"))
                .font(theme::body_lg())
                .color(theme::TEXT_DIM),
        );
    });
    ui.add_space(theme::SPACE_10);
}

fn step_plugins(
    ui: &mut egui::Ui,
    st: &mut OnboardingUi,
    state: &TopicStore,
    cmd: &CommandTx,
    time: f64,
) {
    let candidates = plugin_candidates(&state.plugins);
    let catalog_loaded = state
        .plugins
        .available
        .iter()
        .any(|entry| entry.repo == OFFICIAL_REPO_SLUG);
    if catalog_loaded {
        st.refresh_started = None;
    } else {
        let refreshing = st
            .refresh_started
            .is_some_and(|started| time - started < 8.0)
            || state.discovery.checking_updates;
        widgets::card_with_surface(
            ui,
            egui::Margin::same(18),
            theme::CARD_BG,
            theme::BORDER,
            |ui| {
                ui.label(
                    RichText::new(if refreshing {
                        t!("onboarding.plugins_loading")
                    } else {
                        t!("onboarding.plugins_error")
                    })
                    .font(theme::body_md())
                    .color(if refreshing {
                        theme::TEXT_MUT
                    } else {
                        theme::OFFLINE
                    }),
                );
                ui.add_space(theme::SPACE_7);
                if refreshing {
                    widgets::progress_bar(ui, ui.available_width(), 8.0, None);
                } else if widgets::button(
                    ui,
                    &t!("onboarding.plugins_retry"),
                    ButtonKind::Primary,
                    Vec2::new(92.0, 34.0),
                )
                .clicked()
                {
                    st.refresh_started = Some(time);
                    ipc::send(cmd, DaemonCommand::RefreshPluginRepos);
                }
            },
        );
        return;
    }
    let recommended = t!("onboarding.recommended");
    egui::ScrollArea::vertical()
        .max_height(LIST_MAX_H)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
            for entry in candidates {
                let on = is_selected(st, &entry.id);
                let note = if state
                    .plugins
                    .recommendations
                    .iter()
                    .any(|r| r.plugin_id == entry.id)
                {
                    recommended.as_ref()
                } else {
                    ""
                };
                if widgets::check_row(
                    ui,
                    &entry.id,
                    Some(&entry.version),
                    (!note.is_empty()).then_some((note, theme::TEXT_SOFT)),
                    on,
                )
                .clicked()
                {
                    st.selected.insert(entry.id.clone(), !on);
                }
            }
        });
}

fn step_done(ui: &mut egui::Ui, st: &OnboardingUi, state: &TopicStore) {
    let devices = state
        .devices
        .iter()
        .filter(|d| crate::domain::models::device::listable(d))
        .count();
    let plugins = state.plugins.plugins.iter().filter(|p| p.enabled).count()
        + install_targets(st, &state.plugins).len();
    let sensors = crate::domain::models::sensors::sensors(state).len();
    let tiles = [
        (devices, t!("onboarding.done_devices")),
        (plugins, t!("onboarding.done_plugins")),
        (sensors, t!("onboarding.done_sensors")),
    ];
    let gap = theme::SPACE_5;
    let w = (ui.available_width() - gap * 2.0) / 3.0;
    let (row, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 78.0), egui::Sense::hover());
    let p = ui.painter();
    for (i, (n, label)) in tiles.into_iter().enumerate() {
        let tile = egui::Rect::from_min_size(
            row.min + Vec2::new(i as f32 * (w + gap), 0.0),
            Vec2::new(w, row.height()),
        );
        theme::paint_surface(p, tile, theme::RADIUS_MD, theme::INNER_BG, theme::BORDER);
        p.text(
            tile.min + Vec2::splat(14.0),
            egui::Align2::LEFT_TOP,
            n.to_string(),
            theme::semibold(28.0),
            theme::TEXT,
        );
        p.text(
            tile.left_bottom() + Vec2::new(14.0, -14.0),
            egui::Align2::LEFT_BOTTOM,
            label,
            theme::body_md(),
            theme::TEXT_DIM,
        );
    }
    ui.add_space(theme::SPACE_3);
    ui.label(
        RichText::new(t!("onboarding.done_note"))
            .font(theme::body_md())
            .color(theme::TEXT_DIM),
    );
}

fn footer(
    ui: &mut egui::Ui,
    st: &mut OnboardingUi,
    active: usize,
    outcome: &mut Outcome,
    state: &TopicStore,
    cmd: &CommandTx,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 40.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let show_back = matches!(st.step, HEALTH | PREFS | PLUGINS);
            if show_back
                && widgets::button(
                    ui,
                    &t!("onboarding.back"),
                    ButtonKind::Ghost,
                    Vec2::new(0.0, 36.0),
                )
                .clicked()
            {
                st.step = match st.step {
                    HEALTH => WELCOME,
                    PREFS => HEALTH,
                    PLUGINS => PREFS,
                    _ => WELCOME,
                };
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if st.step == SCANNING {
                    ui.label(
                        RichText::new(t!("onboarding.please_wait"))
                            .font(theme::body_md())
                            .color(theme::TEXT_FAINT),
                    );
                    return;
                }
                let label = primary_label(st.step, active);
                if widgets::button(ui, &label, ButtonKind::Primary, Vec2::new(0.0, 40.0)).clicked()
                {
                    advance_primary(st, state, cmd, outcome);
                }
                if st.step == PLUGINS
                    && active > 0
                    && widgets::button(
                        ui,
                        &t!("onboarding.skip"),
                        ButtonKind::Secondary,
                        Vec2::new(0.0, 36.0),
                    )
                    .clicked()
                {
                    st.step = DONE;
                }
            });
        },
    );
}

fn advance_primary(
    st: &mut OnboardingUi,
    state: &TopicStore,
    cmd: &CommandTx,
    outcome: &mut Outcome,
) {
    match st.step {
        WELCOME => st.step = HEALTH,
        HEALTH => st.step = PREFS,
        PREFS => {
            st.step = SCANNING;
            st.scan_started = None;
        }
        PLUGINS => {
            for id in install_targets(st, &state.plugins) {
                ipc::send(
                    cmd,
                    DaemonCommand::InstallPlugin {
                        repo: OFFICIAL_REPO_SLUG.to_owned(),
                        id,
                    },
                );
            }
            st.step = DONE;
        }
        DONE => *outcome = Outcome::Finished,
        _ => {}
    }
}

fn send_ui_config(cmd: &CommandTx, state: &TopicStore, tray: bool) {
    ipc::send(
        cmd,
        DaemonCommand::SetUiConfig {
            close_to_tray: tray,
            suppress_dependency_warning: state.gui.suppress_dependency_warning,
            hide_window_controls: state.gui.hide_window_controls,
            low_battery_notifications: state.gui.low_battery_notifications,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::PluginRecommendationMatch;

    fn rec(id: &str) -> PluginRecommendation {
        PluginRecommendation {
            plugin_id: id.into(),
            plugin_name: id.into(),
            hardware: PluginRecommendationMatch::Hid { vid: 1, pid: 2 },
            accessible: true,
        }
    }

    #[test]
    fn primary_label_tracks_step_and_selection() {
        assert!(!primary_label(WELCOME, 0).is_empty());
        assert!(!primary_label(PREFS, 0).is_empty());
        // Plugin selection label reflects the active count.
        assert_ne!(primary_label(PLUGINS, 0), primary_label(PLUGINS, 2));
    }

    #[test]
    fn active_dot_tracks_each_onboarding_page() {
        assert_eq!(active_dot(WELCOME), 0);
        assert_eq!(active_dot(HEALTH), 1);
        assert_eq!(active_dot(PREFS), 2);
        assert_eq!(active_dot(SCANNING), 3);
        assert_eq!(active_dot(PLUGINS), 3);
        assert_eq!(active_dot(DONE), 4);
        assert_eq!(step_labels().len(), 5);
    }

    fn catalog(ids: &[&str]) -> PluginsState {
        PluginsState {
            available: ids
                .iter()
                .map(|id| AvailablePlugin {
                    repo: OFFICIAL_REPO_SLUG.into(),
                    id: (*id).into(),
                    version: "1.0.0".into(),
                    api: "3.0".into(),
                    compatible: true,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn candidates_are_compatible_official_plugins_not_yet_installed() {
        let mut plugins = catalog(&["a", "b", "c"]);
        plugins.available[1].compatible = false;
        plugins.available.push(AvailablePlugin {
            repo: "other".into(),
            ..plugins.available[0].clone()
        });
        plugins.plugins = vec![serde_json::from_value(
            serde_json::json!({ "id": "c", "name": "c", "path": "", "enabled": true }),
        )
        .unwrap()];
        let ids: Vec<&str> = plugin_candidates(&plugins)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn recommendations_preselect_without_overriding_an_opt_out() {
        let mut st = OnboardingUi::default();
        let plugins = catalog(&["a", "b"]);

        seed_plugin_selection(&mut st, &[], &plugins);
        assert!(st.selected.is_empty(), "loading is not a user choice");

        seed_plugin_selection(&mut st, &[rec("a")], &plugins);
        assert_eq!(active_count(&st, &plugins), 1);

        st.selected.insert("a".into(), false);
        seed_plugin_selection(&mut st, &[rec("a")], &plugins);
        assert_eq!(active_count(&st, &plugins), 0);
    }

    #[test]
    fn finishing_the_plugin_step_installs_the_selection() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = TopicStore {
            plugins: catalog(&["a", "b"]),
            ..Default::default()
        };
        let mut st = OnboardingUi {
            step: PLUGINS,
            selected: HashMap::from([("a".into(), true)]),
            ..Default::default()
        };
        let mut outcome = Outcome::Pending;

        advance_primary(&mut st, &state, &cmd, &mut outcome);
        assert_eq!(st.step, DONE);
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::InstallPlugin { repo, id }) if repo == OFFICIAL_REPO_SLUG && id == "a"
        ));
        assert!(rx.try_recv().is_err());

        advance_primary(&mut st, &state, &cmd, &mut outcome);
        assert_eq!(outcome, Outcome::Finished);
    }
}

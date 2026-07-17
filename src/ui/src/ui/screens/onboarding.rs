// SPDX-License-Identifier: GPL-3.0-or-later
//! First-run onboarding wizard driven by live daemon state. It owns first-run
//! download consent, dependency health, plugin recommendations, initial
//! selection, and authority review so those concerns never race separate modals.

use std::collections::HashMap;

use egui::{RichText, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::debug_info::DebugInfo;
use halod_shared::types::{
    AppState, PluginDownloadConsent, PluginInfo, PluginKind, PluginRecommendation, PluginSource,
};

use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::{icons, theme};

const WELCOME: u8 = 0;
const HEALTH: u8 = 1;
const PREFS: u8 = 2;
const SCANNING: u8 = 3;
const PLUGINS: u8 = 4;
const DONE: u8 = 5;
const OFFICIAL_REPO_SLUG: &str = "official";
const BODY_HEIGHT: f32 = 420.0;

/// How long the cosmetic scan animation runs before showing recommendations
/// (they are already computed at startup; this is a settle beat, not real work).
const SCAN_SECS: f64 = 2.2;

#[derive(Default)]
pub struct OnboardingUi {
    step: u8,
    scan_started: Option<f64>,
    official_retry_started: Option<f64>,
    /// Plugin id → selected for authority review within onboarding.
    selected: HashMap<String, bool>,
}

/// Whether the wizard was explicitly completed this frame, so the caller can
/// persist "seen" and seed the recommendation dedup set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Finished,
}

fn plugins_on(state: &AppState) -> bool {
    state.gui.plugin_downloads == PluginDownloadConsent::Allowed
}

fn is_selected(st: &OnboardingUi, id: &str) -> bool {
    st.selected.get(id).copied().unwrap_or(false)
}

fn plugin_candidates(plugins: &[PluginInfo]) -> Vec<&PluginInfo> {
    plugins
        .iter()
        .filter(|plugin| {
            plugin.platform_supported
                && plugin.plugin_type != PluginKind::Integration
                && matches!(
                    &plugin.source,
                    PluginSource::Repo { slug }
                        if slug == OFFICIAL_REPO_SLUG
                )
        })
        .collect()
}

fn seed_plugin_selection(
    st: &mut OnboardingUi,
    recommendations: &[PluginRecommendation],
    plugins: &[PluginInfo],
) {
    for plugin in plugin_candidates(plugins) {
        if recommendations
            .iter()
            .any(|recommendation| recommendation.plugin_id == plugin.id)
        {
            // Only materialize recommended defaults. An absent entry already
            // means "off"; storing false before the asynchronous recommendation
            // snapshot arrives would make that temporary state indistinguishable
            // from a user explicitly switching the plugin off.
            st.selected.entry(plugin.id.clone()).or_insert(true);
        }
    }
}

fn active_count(st: &OnboardingUi, plugins: &[PluginInfo]) -> usize {
    plugin_candidates(plugins)
        .into_iter()
        .filter(|plugin| is_selected(st, &plugin.id))
        .count()
}

/// The primary button label for a step (pure, tested).
fn primary_label(step: u8, active: usize) -> String {
    match step {
        WELCOME => t!("onboarding.p_get_started").to_string(),
        HEALTH => t!("onboarding.p_continue").to_string(),
        PREFS => t!("onboarding.p_scan").to_string(),
        PLUGINS => {
            if active > 0 {
                t!("onboarding.p_grant_enable").to_string()
            } else {
                t!("onboarding.p_finish").to_string()
            }
        }
        DONE => t!("onboarding.p_enter").to_string(),
        _ => t!("onboarding.p_continue").to_string(),
    }
}

/// Which onboarding progress dot is active. The completion page keeps the
/// plugin dot active because it has no separate input.
fn active_dot(step: u8) -> usize {
    usize::from(step.min(PLUGINS))
}

/// Selected official plugins that are currently disabled.
fn enable_targets(st: &OnboardingUi, plugins: &[PluginInfo]) -> Vec<String> {
    plugin_candidates(plugins)
        .into_iter()
        .filter(|plugin| is_selected(st, &plugin.id) && !plugin.enabled)
        .map(|plugin| plugin.id.clone())
        .collect()
}

const SCAN_LINES: &[&str] = &[
    "probing usb bus…",
    "querying mdns _wled._udp.local",
    "reading smbus sensors",
    "matching signed driver manifests",
];

pub fn show(
    ctx: &egui::Context,
    state: &AppState,
    debug: Option<&DebugInfo>,
    cmd: &CommandTx,
    st: &mut OnboardingUi,
    time: f64,
) -> Outcome {
    let recs = &state.plugins.recommendations;
    seed_plugin_selection(st, recs, &state.plugins.plugins);

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
    let _response = egui::Modal::new(egui::Id::new("onboarding_modal"))
        .frame(
            egui::Frame::NONE
                .fill(theme::MODAL_BG)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(theme::RADIUS_2XL)
                .inner_margin(egui::Margin {
                    left: 28,
                    right: 28,
                    top: 28,
                    bottom: 0,
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(600.0);
            // A fixed body keeps every step aligned; list-heavy pages scroll
            // within it instead of growing the modal.
            let body = egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 0));
            body.show(ui, |ui| {
                ui.set_min_height(BODY_HEIGHT);
                ui.set_max_height(BODY_HEIGHT);
                ui.vertical_centered(|ui| ui.set_width(600.0));
                match st.step {
                    WELCOME => step_welcome(ui, time as f32),
                    HEALTH => step_health(ui, debug),
                    PREFS => step_prefs(ui, state, cmd),
                    SCANNING => step_scanning(ui, time),
                    PLUGINS => step_plugins(ui, st, state, cmd, time),
                    _ => step_done(ui, st, state, &state.plugins.plugins),
                }
            });
            ui.separator();
            footer(
                ui,
                st,
                active_count(st, &state.plugins.plugins),
                &mut outcome,
                state,
                cmd,
            );
        });
    outcome
}

/// Shared scaffold of the form-style steps: lead space, left indent, 520 px
/// column, bold title + muted subtitle, then the step body.
fn step_shell(ui: &mut egui::Ui, title: &str, sub: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(26.0);
    ui.horizontal(|ui| {
        ui.add_space(theme::SPACE_12);
        ui.vertical(|ui| {
            ui.set_width(520.0);
            ui.label(
                RichText::new(title)
                    .font(theme::bold(21.0))
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_3);
            ui.label(
                RichText::new(sub)
                    .font(theme::body_lg())
                    .color(theme::TEXT_MUT),
            );
            body(ui);
        });
    });
}

fn step_welcome(ui: &mut egui::Ui, time: f32) {
    ui.add_space(52.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 0.0),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            widgets::logo_icon(ui, 96.0, time);
            ui.add_space(26.0);
            ui.label(
                RichText::new(t!("onboarding.welcome_title"))
                    .font(theme::bold(30.0))
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_7);
            centered_wrapped(
                ui,
                &t!("onboarding.welcome_body"),
                440.0,
                theme::TEXT_DIM,
                15.0,
            );
            ui.add_space(theme::SPACE_10);
            ui.label(
                RichText::new(format!(
                    "{} v{}",
                    halod_shared::app::APP_DISPLAY_NAME,
                    env!("CARGO_PKG_VERSION")
                ))
                .font(theme::mono(13.0))
                .color(theme::TEXT_FAINT),
            );
        },
    );
}

fn step_health(ui: &mut egui::Ui, debug: Option<&DebugInfo>) {
    step_shell(
        ui,
        &t!("onboarding.health_title"),
        &t!("onboarding.health_sub"),
        |ui| {
            ui.add_space(theme::SPACE_9);
            match debug {
                None => {
                    ui.spinner();
                    ui.label(
                        RichText::new(t!("onboarding.health_checking"))
                            .font(theme::body_md())
                            .color(theme::TEXT_FAINT),
                    );
                }
                Some(info) => {
                    let failing = crate::ui::screens::depcheck::failing(info);
                    if failing.is_empty() {
                        widgets::card_with_surface(
                            ui,
                            theme::PAD_CARD,
                            theme::CARD_BG,
                            theme::BORDER,
                            |ui| {
                                ui.label(
                                    RichText::new(t!("onboarding.health_ok"))
                                        .font(theme::heading())
                                        .color(theme::ONLINE_TEXT),
                                );
                            },
                        );
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(310.0)
                            .min_scrolled_height(310.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for dependency in failing {
                                    crate::ui::screens::depcheck::issue_card_with_surface(
                                        ui,
                                        dependency,
                                        theme::CARD_BG,
                                        theme::BORDER,
                                    );
                                    ui.add_space(theme::SPACE_5);
                                }
                            });
                    }
                }
            }
        },
    );
}

fn centered_wrapped(ui: &mut egui::Ui, text: &str, max_w: f32, color: egui::Color32, size: f32) {
    ui.allocate_ui_with_layout(
        Vec2::new(max_w, 0.0),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.label(RichText::new(text).font(theme::body(size)).color(color));
        },
    );
}

fn pref_card(ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_LG)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            widgets::setting_row(ui, title, desc, control);
        });
}

fn step_prefs(ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
    step_shell(
        ui,
        &t!("onboarding.prefs_title"),
        &t!("onboarding.prefs_sub"),
        |ui| {
            ui.add_space(22.0);

            pref_card(
                ui,
                &t!("onboarding.tray_title"),
                &t!("onboarding.tray_desc"),
                |ui| {
                    if widgets::toggle(ui, state.gui.close_to_tray) != state.gui.close_to_tray {
                        send_ui_config(cmd, state, Some(!state.gui.close_to_tray), None);
                    }
                },
            );
            ui.add_space(theme::SPACE_6);
            pref_card(
                ui,
                &t!("settings.window_controls_title"),
                &t!("settings.window_controls_sub"),
                |ui| {
                    let on = !state.gui.hide_window_controls;
                    let next = widgets::toggle(ui, on);
                    if next != on {
                        send_ui_config(cmd, state, None, Some(!next));
                    }
                },
            );
            ui.add_space(theme::SPACE_6);
            let on = plugins_on(state);
            pref_card(
                ui,
                &t!("onboarding.plugins_title"),
                &t!("onboarding.plugins_desc"),
                |ui| {
                    if widgets::toggle(ui, on) != on {
                        ipc::send(
                            cmd,
                            DaemonCommand::SetPluginDownloadConsent { allowed: !on },
                        );
                    }
                },
            );
        },
    );
}

fn step_scanning(ui: &mut egui::Ui, time: f64) {
    ui.add_space(theme::SPACE_12);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(230.0), Sense::hover());
        crate::ui::screens::radar::draw_radar(ui.painter(), rect.center(), 112.0, time);
        // A couple of detected-device blips (pink), placed deterministically.
        for (dx, dy) in [(-42.0, -30.0), (54.0, 40.0)] {
            let c = rect.center() + Vec2::new(dx, dy);
            theme::glow(ui.painter(), c, 8.0, theme::hex(0xe879f9), 0.7);
            ui.painter().circle_filled(c, 4.0, theme::hex(0xe879f9));
        }
        ui.add_space(22.0);
        ui.label(
            RichText::new(t!("onboarding.scanning"))
                .font(theme::title())
                .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_4);
        let line = SCAN_LINES[((time * 1.6) as usize) % SCAN_LINES.len()];
        ui.label(
            RichText::new(format!("{line}_"))
                .font(theme::mono(12.0))
                .color(theme::TEXT_MUT),
        );
    });
}

/// One plugin row: name, description, toggle, and — once selected — the system
/// access it wants, so choosing a plugin and consenting to it are one decision.
fn plugin_toggle_row(ui: &mut egui::Ui, plugin: &PluginInfo, on: bool) -> bool {
    let next = egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(4, 14))
        .show(ui, |ui| {
            let next = widgets::setting_row(ui, &plugin.name, &plugin.description, |ui| {
                widgets::toggle(ui, on)
            });
            if on && !plugin.enabled {
                ui.add_space(theme::SPACE_4);
                if plugin.declared_permissions.is_empty() {
                    ui.label(
                        RichText::new(t!("onboarding.permissions_none"))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                } else {
                    crate::ui::screens::plugins::authority_review_cards(ui, plugin);
                }
            }
            next
        })
        .inner;
    ui.separator();
    next
}

fn step_plugins(
    ui: &mut egui::Ui,
    st: &mut OnboardingUi,
    state: &AppState,
    cmd: &CommandTx,
    time: f64,
) {
    let candidates = plugin_candidates(&state.plugins.plugins);
    let official_unavailable = state.plugins.repos.iter().any(|repo| {
        repo.slug == OFFICIAL_REPO_SLUG && repo.active_revision.as_deref().is_none_or(str::is_empty)
    });
    if !candidates.is_empty() {
        st.official_retry_started = None;
    }
    step_shell(
        ui,
        &t!("onboarding.plugins_page_title"),
        &t!("onboarding.plugins_page_sub"),
        |ui| {
            ui.add_space(theme::SPACE_8);
            if candidates.is_empty() && official_unavailable {
                let retrying = st
                    .official_retry_started
                    .is_some_and(|started| time - started < 8.0);
                widgets::card_with_surface(
                    ui,
                    egui::Margin::same(18),
                    theme::CARD_BG,
                    theme::BORDER,
                    |ui| {
                        ui.label(
                            RichText::new(if retrying {
                                t!("onboarding.plugins_retrying")
                            } else {
                                t!("onboarding.plugins_error")
                            })
                            .font(theme::body_md())
                            .color(if retrying {
                                theme::TEXT_MUT
                            } else {
                                theme::OFFLINE
                            }),
                        );
                        ui.add_space(theme::SPACE_7);
                        if retrying {
                            ui.add(
                                egui::ProgressBar::new(0.35)
                                    .desired_width(ui.available_width())
                                    .desired_height(8.0)
                                    .fill(theme::CYAN)
                                    .animate(true),
                            );
                            ui.ctx().request_repaint();
                        } else if widgets::button(
                            ui,
                            &t!("onboarding.plugins_retry"),
                            ButtonKind::Primary,
                            Vec2::new(92.0, 34.0),
                        )
                        .clicked()
                        {
                            st.official_retry_started = Some(time);
                            ipc::send(
                                cmd,
                                DaemonCommand::SetPluginDownloadConsent { allowed: true },
                            );
                        }
                    },
                );
            } else if candidates.is_empty() {
                ui.label(
                    RichText::new(t!("onboarding.plugins_loading"))
                        .font(theme::body_md())
                        .color(theme::TEXT_FAINT),
                );
                ui.add_space(theme::SPACE_7);
                ui.add(
                    egui::ProgressBar::new(0.35)
                        .desired_width(520.0)
                        .desired_height(8.0)
                        .fill(theme::CYAN)
                        .animate(true),
                );
            }
            if !candidates.is_empty() {
                egui::ScrollArea::vertical()
                    .max_height(258.0)
                    .min_scrolled_height(258.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for plugin in candidates {
                            let on = is_selected(st, &plugin.id);
                            if plugin_toggle_row(ui, plugin, on) != on {
                                st.selected.insert(plugin.id.clone(), !on);
                            }
                        }
                    });
                ui.add_space(theme::SPACE_3);
                ui.label(
                    RichText::new(t!("plugins.consent_warning"))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            }
        },
    );
}

fn step_done(ui: &mut egui::Ui, st: &OnboardingUi, state: &AppState, plugins: &[PluginInfo]) {
    // The completion composition is roughly 180 px tall including its wrapped
    // summary. Center it in the fixed onboarding body rather than aligning it
    // with the top-biased form pages.
    ui.add_space((BODY_HEIGHT - 180.0) / 2.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(76.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 38.0, theme::hex(0x12211a));
        ui.painter()
            .circle_stroke(rect.center(), 38.0, Stroke::new(1.0, theme::hex(0x2b6b45)));
        icons::draw(
            ui,
            egui::Rect::from_center_size(rect.center(), Vec2::splat(38.0)),
            icons::Icon::Check,
            theme::ONLINE,
        );
        ui.add_space(theme::SPACE_10);
        ui.label(
            RichText::new(t!("onboarding.done_title"))
                .font(theme::bold(25.0))
                .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_6);
        let summary = if plugins_on(state) && active_count(st, plugins) > 0 {
            t!("onboarding.done_summary_plugins")
        } else {
            t!("onboarding.done_summary_basic")
        };
        centered_wrapped(ui, &summary, 380.0, theme::TEXT_DIM, 13.5);
    });
}

fn footer(
    ui: &mut egui::Ui,
    st: &mut OnboardingUi,
    active: usize,
    outcome: &mut Outcome,
    state: &AppState,
    cmd: &CommandTx,
) {
    ui.add_space(theme::SPACE_3);
    ui.horizontal(|ui| {
        let show_back = matches!(st.step, HEALTH | PREFS | PLUGINS);
        if show_back
            && widgets::button(
                ui,
                &t!("onboarding.back"),
                ButtonKind::Ghost,
                Vec2::new(84.0, 38.0),
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

        // Centered progress dots.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if st.step != SCANNING {
                let label = primary_label(st.step, active);
                if widgets::button(ui, &label, ButtonKind::Primary, Vec2::new(0.0, 38.0)).clicked()
                {
                    advance_primary(st, state, cmd, outcome);
                }
            } else {
                ui.label(
                    RichText::new(t!("onboarding.please_wait"))
                        .font(theme::value_sm())
                        .color(theme::TEXT_FAINT),
                );
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                dots(ui, st.step);
            });
        });
    });
    ui.add_space(theme::SPACE_3);
}

fn dots(ui: &mut egui::Ui, step: u8) {
    let active = active_dot(step);
    ui.horizontal(|ui| {
        for i in 0..=usize::from(PLUGINS) {
            let w = if i == active { 22.0 } else { 6.0 };
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 6.0), Sense::hover());
            let c = if i == active {
                theme::CYAN
            } else {
                theme::hex(0x3a3546)
            };
            ui.painter().rect_filled(rect, 3.0, c);
            ui.add_space(theme::SPACE_2);
        }
    });
}

fn advance_primary(
    st: &mut OnboardingUi,
    state: &AppState,
    cmd: &CommandTx,
    outcome: &mut Outcome,
) {
    match st.step {
        WELCOME => st.step = HEALTH,
        HEALTH => st.step = PREFS,
        PREFS => {
            if state.gui.plugin_downloads == PluginDownloadConsent::Unset {
                ipc::send(
                    cmd,
                    DaemonCommand::SetPluginDownloadConsent { allowed: false },
                );
            }
            // Official plugins are bundled, so their recommendation and
            // authority-review steps remain useful even when future plugin
            // downloads are disabled.
            st.step = SCANNING;
            st.scan_started = None;
        }
        PLUGINS => {
            let plugins: Vec<halod_shared::commands::PluginEnableConfirmation> =
                enable_targets(st, &state.plugins.plugins)
                    .into_iter()
                    .filter_map(|id| {
                        state
                            .plugins
                            .plugins
                            .iter()
                            .find(|plugin| plugin.id == id)
                            .map(|plugin| halod_shared::commands::PluginEnableConfirmation {
                                id,
                                authority: plugin.authority.clone(),
                            })
                    })
                    .collect();
            if !plugins.is_empty() {
                ipc::send(cmd, DaemonCommand::ConfirmPluginEnableBatch { plugins });
            }
            st.step = DONE;
        }
        DONE => *outcome = Outcome::Finished,
        _ => {}
    }
}

fn send_ui_config(
    cmd: &CommandTx,
    state: &AppState,
    tray: Option<bool>,
    hide_controls: Option<bool>,
) {
    ipc::send(
        cmd,
        DaemonCommand::SetUiConfig {
            close_to_tray: tray.unwrap_or(state.gui.close_to_tray),
            suppress_dependency_warning: state.gui.suppress_dependency_warning,
            hide_window_controls: hide_controls.unwrap_or(state.gui.hide_window_controls),
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
    fn disabled_downloads_still_continue_to_bundled_plugin_scan() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = AppState::default();
        state.gui.plugin_downloads = PluginDownloadConsent::Denied;
        let mut st = OnboardingUi {
            step: PREFS,
            ..Default::default()
        };
        let mut outcome = Outcome::Pending;

        advance_primary(&mut st, &state, &cmd, &mut outcome);

        assert_eq!(st.step, SCANNING);
        assert_eq!(outcome, Outcome::Pending);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn active_dot_tracks_each_onboarding_page() {
        assert_eq!(active_dot(WELCOME), 0);
        assert_eq!(active_dot(HEALTH), 1);
        assert_eq!(active_dot(PREFS), 2);
        assert_eq!(active_dot(SCANNING), 3);
        assert_eq!(active_dot(PLUGINS), 4);
        assert_eq!(active_dot(DONE), 4);
    }

    #[test]
    fn recommendations_seed_plugin_selection_on() {
        let mut st = OnboardingUi::default();
        let recs = [rec("a"), rec("b")];
        let plugins = [plugin("a", false), plugin("b", false), plugin("c", false)];
        seed_plugin_selection(&mut st, &recs, &plugins);
        assert_eq!(active_count(&st, &plugins), 2);
        st.selected.insert("a".into(), false);
        assert_eq!(active_count(&st, &plugins), 1);
    }

    #[test]
    fn recommendations_arriving_after_plugins_still_preselect_defaults() {
        let mut st = OnboardingUi::default();
        let plugins = [plugin("a", false), plugin("b", false)];

        seed_plugin_selection(&mut st, &[], &plugins);
        assert!(st.selected.is_empty(), "loading is not a user choice");

        seed_plugin_selection(&mut st, &[rec("a")], &plugins);
        assert!(is_selected(&st, "a"));
        assert!(!is_selected(&st, "b"));

        st.selected.insert("a".into(), false);
        seed_plugin_selection(&mut st, &[rec("a")], &plugins);
        assert!(!is_selected(&st, "a"), "an explicit opt-out stays off");
    }

    #[test]
    fn enable_targets_only_selected_disabled_plugins_with_authority() {
        let mut st = OnboardingUi::default();
        st.selected.insert("b".into(), false); // deselected
        let recs = [rec("a"), rec("b"), rec("c")];
        let plugins = vec![
            plugin("a", false),
            plugin("b", false),
            plugin("c", true), // already enabled → excluded
        ];
        seed_plugin_selection(&mut st, &recs, &plugins);
        let targets = enable_targets(&st, &plugins);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], "a");
    }

    #[test]
    fn plugin_selection_grants_authority_before_completion() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = AppState::default();
        state.plugins.plugins = vec![plugin("a", false)];
        let mut st = OnboardingUi {
            step: PLUGINS,
            selected: HashMap::from([("a".into(), true)]),
            ..Default::default()
        };
        let mut outcome = Outcome::Pending;

        advance_primary(&mut st, &state, &cmd, &mut outcome);
        assert_eq!(st.step, DONE);
        assert_eq!(outcome, Outcome::Pending);
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::ConfirmPluginEnableBatch { plugins })
                if plugins.len() == 1 && plugins[0].id == "a"
        ));

        advance_primary(&mut st, &state, &cmd, &mut outcome);
        assert_eq!(outcome, Outcome::Finished);
    }

    fn plugin(id: &str, enabled: bool) -> PluginInfo {
        let mut p: PluginInfo = serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "path": "", "enabled": enabled
        }))
        .unwrap();
        p.authority = halod_shared::types::PluginAuthority::default();
        p.source = PluginSource::Repo {
            slug: OFFICIAL_REPO_SLUG.into(),
        };
        p
    }
}

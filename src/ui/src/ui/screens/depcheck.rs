// SPDX-License-Identifier: GPL-3.0-or-later
//! Startup healthcheck dialog, driven by the daemon's reported dependencies.

use crate::ui::components as widgets;
use egui::RichText;
use halod_shared::debug_info::{DebugInfo, DependencyStatus};
use halod_shared::types::AppState;

use crate::runtime::ipc::CommandTx;
use crate::ui::theme;

#[derive(Default)]
pub struct DepCheckUi {
    dismissed: bool,
    dont_show_again: bool,
}

impl DepCheckUi {
    pub fn dismiss_for_session(&mut self) {
        self.dismissed = true;
    }
}

pub enum GraceAction {
    None,
    Recheck,
    RepaintAfter(f64),
}

#[derive(Default)]
pub struct GraceState {
    connected_at: Option<f64>,
    recheck_sent: bool,
}

impl GraceState {
    pub fn advance(&mut self, connected: bool, time: f64, grace_secs: f64) -> (bool, GraceAction) {
        match (connected, self.connected_at) {
            (true, None) => {
                self.connected_at = Some(time);
                self.recheck_sent = false;
            }
            (false, _) => {
                self.connected_at = None;
                self.recheck_sent = false;
            }
            _ => {}
        }
        let within_grace = self.connected_at.is_some_and(|t0| time - t0 < grace_secs);
        let action = match self.connected_at {
            Some(t0) if within_grace => {
                GraceAction::RepaintAfter((t0 + grace_secs - time).max(0.0))
            }
            Some(_) if !self.recheck_sent => {
                self.recheck_sent = true;
                GraceAction::Recheck
            }
            _ => GraceAction::None,
        };
        (within_grace, action)
    }
}

/// Failing checks (required first) shared by the dialog and the settings section.
pub fn failing(info: &DebugInfo) -> Vec<&DependencyStatus> {
    let mut out: Vec<&DependencyStatus> = info.dependencies.iter().filter(|d| !d.present).collect();
    out.sort_by_key(|d| !d.required);
    out
}

/// Bundled so same-typed flags can't be transposed at a call site.
pub struct DepCheckGate {
    pub connected: bool,
    pub suppressed: bool,
    pub dismissed: bool,
    pub within_grace: bool,
}

pub fn should_show(gate: &DepCheckGate, info: Option<&DebugInfo>) -> bool {
    gate.connected
        && !gate.suppressed
        && !gate.dismissed
        && !gate.within_grace
        && info.is_some_and(|i| !failing(i).is_empty())
}

/// Whether the healthcheck dialog is currently showing — so other overlays
/// (the tour) know to stay out of its way.
pub fn visible(
    state: &AppState,
    debug: Option<&DebugInfo>,
    connected: bool,
    within_grace: bool,
    st: &DepCheckUi,
) -> bool {
    should_show(
        &DepCheckGate {
            connected,
            suppressed: state.gui.suppress_dependency_warning,
            dismissed: st.dismissed,
            within_grace,
        },
        debug,
    )
}

pub fn show(
    ctx: &egui::Context,
    state: &AppState,
    cmd: &CommandTx,
    debug: Option<&DebugInfo>,
    connected: bool,
    within_grace: bool,
    st: &mut DepCheckUi,
) {
    let gate = DepCheckGate {
        connected,
        suppressed: state.gui.suppress_dependency_warning,
        dismissed: st.dismissed,
        within_grace,
    };
    if !should_show(&gate, debug) {
        return;
    }
    let Some(info) = debug else { return };
    let failing = failing(info);

    let mut confirmed = false;
    let closed = widgets::dialog(
        ctx,
        "healthcheck",
        &t!("depcheck.title"),
        560.0,
        |ui| {
            ui.label(
                RichText::new(t!("depcheck.intro"))
                    .font(theme::body(12.0))
                    .color(theme::TEXT_MUT),
            );
            ui.add_space(12.0);
            for dep in &failing {
                issue_card(ui, dep);
                ui.add_space(10.0);
            }
            ui.add_space(2.0);
            ui.checkbox(&mut st.dont_show_again, t!("depcheck.dont_show_again"));
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("depcheck.got_it"),
                widgets::ButtonKind::Primary,
                egui::vec2(120.0, 34.0),
            )
            .clicked()
            {
                confirmed = true;
            }
        },
    );

    if closed || confirmed {
        st.dismissed = true;
        if st.dont_show_again {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetUiConfig {
                    close_to_tray: state.gui.close_to_tray,
                    suppress_dependency_warning: true,
                    hide_window_controls: state.gui.hide_window_controls,
                },
            );
        }
    }
}

pub(crate) fn title_text(dep: &DependencyStatus) -> String {
    t!(format!("depcheck.rules.{}.title", dep.id.i18n_key())).to_string()
}

pub(crate) fn impact_text(dep: &DependencyStatus) -> String {
    t!(format!("depcheck.rules.{}.impact", dep.id.i18n_key())).to_string()
}

pub(crate) fn fix_text(dep: &DependencyStatus) -> String {
    let key = dep.id.i18n_key();
    if dep.fix_variant.is_empty() {
        t!(format!("depcheck.rules.{key}.fix")).to_string()
    } else {
        t!(format!("depcheck.rules.{key}.fix_{}", dep.fix_variant)).to_string()
    }
}

pub(crate) fn issue_card(ui: &mut egui::Ui, dep: &DependencyStatus) {
    issue_card_with_surface(ui, dep, theme::CARD_BG, theme::BORDER);
}

pub(crate) fn issue_card_with_surface(
    ui: &mut egui::Ui,
    dep: &DependencyStatus,
    fill: egui::Color32,
    border: egui::Color32,
) {
    widgets::card_with_surface(ui, egui::Margin::same(20), fill, border, |ui| {
        ui.set_width(ui.available_width());
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    RichText::new(title_text(dep))
                        .font(theme::semibold(13.0))
                        .color(theme::TEXT),
                );
            },
            |ui| {
                let (tag, color) = if dep.required {
                    (t!("depcheck.required"), theme::OFFLINE)
                } else {
                    (t!("depcheck.optional"), theme::STAT_AMBER)
                };
                ui.label(RichText::new(tag).font(theme::body(10.5)).color(color));
            },
        );
        ui.add_space(6.0);
        caption(ui, &t!("depcheck.impact"));
        body_text(ui, &impact_text(dep));
        ui.add_space(6.0);
        caption(ui, &t!("depcheck.how_to_fix"));
        body_text(ui, &fix_text(dep));
    });
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::semibold(9.5))
            .color(theme::TEXT_FAINT),
    );
}

fn body_text(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::body(11.5))
            .color(theme::TEXT_MUT),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::debug_info::{DependencyRule, OsKind, SystemDebugInfo};

    fn dep(id: DependencyRule, present: bool, required: bool) -> DependencyStatus {
        DependencyStatus {
            id,
            present,
            required,
            platform: "All".into(),
            fix_variant: String::new(),
        }
    }

    #[test]
    fn grace_suppresses_dialog_until_it_elapses() {
        let mut g = GraceState::default();
        let (within_grace, action) = g.advance(true, 0.0, 4.0);
        assert!(within_grace);
        assert!(matches!(action, GraceAction::RepaintAfter(secs) if secs == 4.0));

        let (within_grace, action) = g.advance(true, 3.9, 4.0);
        assert!(within_grace);
        assert!(matches!(action, GraceAction::RepaintAfter(_)));

        let (within_grace, action) = g.advance(true, 4.1, 4.0);
        assert!(!within_grace);
        assert!(matches!(action, GraceAction::Recheck));

        // The recheck only fires once per grace window.
        let (within_grace, action) = g.advance(true, 5.0, 4.0);
        assert!(!within_grace);
        assert!(matches!(action, GraceAction::None));
    }

    #[test]
    fn grace_resets_on_disconnect_and_reconnect() {
        let mut g = GraceState::default();
        g.advance(true, 10.0, 4.0);
        g.advance(true, 15.0, 4.0); // past grace, recheck already sent
        let (within_grace, _) = g.advance(false, 15.5, 4.0);
        assert!(!within_grace);

        // Reconnecting restarts the grace window from the new timestamp.
        let (within_grace, action) = g.advance(true, 20.0, 4.0);
        assert!(within_grace);
        assert!(matches!(action, GraceAction::RepaintAfter(secs) if secs == 4.0));
    }

    fn info(dependencies: Vec<DependencyStatus>) -> DebugInfo {
        DebugInfo {
            system: SystemDebugInfo {
                os: OsKind::Windows,
                os_version: String::new(),
                running_elevated: false,
                pawnio_present: None,
                udev_rules_present: None,
                daemon_version: String::new(),
                daemon_build: String::new(),
            },
            devices: vec![],
            hid_entries: vec![],
            smbus_buses: vec![],
            dependencies,
        }
    }

    #[test]
    fn failing_returns_only_missing_required_first() {
        let di = info(vec![
            dep(DependencyRule::Ffmpeg, false, false),
            dep(DependencyRule::UdevRules, false, true),
            dep(DependencyRule::NvidiaSmi, true, true),
        ]);
        let got = failing(&di);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, DependencyRule::UdevRules);
        assert!(got[0].required);
        assert_eq!(got[1].id, DependencyRule::Ffmpeg);
    }

    fn gate(
        connected: bool,
        suppressed: bool,
        dismissed: bool,
        within_grace: bool,
    ) -> DepCheckGate {
        DepCheckGate {
            connected,
            suppressed,
            dismissed,
            within_grace,
        }
    }

    #[test]
    fn should_show_requires_connected_unsuppressed_and_failures() {
        let di = info(vec![dep(DependencyRule::Ffmpeg, false, false)]);
        assert!(should_show(&gate(true, false, false, false), Some(&di)));
        assert!(!should_show(&gate(false, false, false, false), Some(&di)));
        assert!(!should_show(&gate(true, true, false, false), Some(&di)));
        assert!(!should_show(&gate(true, false, true, false), Some(&di)));
        assert!(!should_show(&gate(true, false, false, false), None));
        let clean = info(vec![dep(DependencyRule::Ffmpeg, true, false)]);
        assert!(!should_show(&gate(true, false, false, false), Some(&clean)));
    }

    #[test]
    fn within_grace_suppresses_even_with_failures() {
        let di = info(vec![dep(DependencyRule::Ffmpeg, false, false)]);
        assert!(!should_show(&gate(true, false, false, true), Some(&di)));
    }

    #[test]
    fn after_grace_shows_with_failures() {
        let di = info(vec![dep(DependencyRule::Ffmpeg, false, false)]);
        assert!(should_show(&gate(true, false, false, false), Some(&di)));
    }

    #[test]
    fn every_daemon_rule_id_has_translations() {
        for &rule in DependencyRule::ALL {
            let key = rule.i18n_key();
            let d = dep(rule, false, false);
            assert_ne!(
                title_text(&d),
                format!("depcheck.rules.{key}.title"),
                "missing title translation for {key:?}"
            );
            assert_ne!(
                impact_text(&d),
                format!("depcheck.rules.{key}.impact"),
                "missing impact translation for {key:?}"
            );
            // GnomeExtension resolves its fix through fix_variant keys instead
            // (covered by gnome_extension_fix_variants_are_translated).
            if rule != DependencyRule::GnomeExtension {
                assert_ne!(
                    fix_text(&d),
                    format!("depcheck.rules.{key}.fix"),
                    "missing fix translation for {key:?}"
                );
            }
        }
    }

    #[test]
    fn gnome_extension_fix_variants_are_translated() {
        let mut d = dep(DependencyRule::GnomeExtension, false, false);
        for variant in ["disabled", "missing"] {
            d.fix_variant = variant.to_string();
            assert_ne!(
                fix_text(&d),
                format!("depcheck.rules.gnome_extension.fix_{variant}"),
                "missing fix translation for gnome_extension/{variant}"
            );
        }
    }
}

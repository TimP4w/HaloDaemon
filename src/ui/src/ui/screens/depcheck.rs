// SPDX-License-Identifier: GPL-3.0-or-later
//! Startup healthcheck dialog, driven by the daemon's reported dependencies.

use crate::domain::topic_store::TopicStore;
use crate::ui::components as widgets;
use egui::RichText;
use halod_shared::debug_info::{DebugInfo, DependencyRule, DependencyStatus};

use crate::runtime::ipc::CommandTx;
use crate::ui::theme;

#[derive(Default)]
pub struct DepCheckUi {
    dismissed: bool,
    dont_show_again: bool,
    copied: CopiedCommand,
}

#[derive(Default)]
pub(crate) struct CopiedCommand(Option<(DependencyRule, f64)>);

impl CopiedCommand {
    pub(crate) fn showing(&self, id: DependencyRule, now: f64) -> bool {
        self.0.is_some_and(|(copied, at)| {
            copied == id && crate::ui::screens::settings::copied_feedback_visible(Some(at), now)
        })
    }

    pub(crate) fn mark(&mut self, id: DependencyRule, now: f64) {
        self.0 = Some((id, now));
    }
}

pub(crate) fn udev_install_commands() -> String {
    "halod udev-rules | sudo tee /etc/udev/rules.d/60-halod.rules >/dev/null\n\
sudo udevadm control --reload-rules\n\
sudo udevadm trigger"
        .to_owned()
}

pub(crate) fn fix_command(dep: &DependencyStatus) -> Option<String> {
    match (dep.id, dep.fix_variant.as_str()) {
        (DependencyRule::UdevRules, _) => Some(udev_install_commands()),
        (DependencyRule::I2cAccess, _) => Some(
            "sudo modprobe i2c-dev\n\
echo i2c-dev | sudo tee /etc/modules-load.d/i2c-dev.conf\n\
sudo usermod -aG i2c \"$USER\""
                .to_owned(),
        ),
        (DependencyRule::HalodGroup, _) => Some("sudo usermod -aG halod \"$USER\"".to_owned()),
        (DependencyRule::GnomeExtension, "disabled") => Some(format!(
            "gnome-extensions enable {}",
            halod_shared::app::GNOME_EXTENSION_UUID
        )),
        _ => None,
    }
}

pub(crate) fn fix_command_block(
    ui: &mut egui::Ui,
    dep: &DependencyStatus,
    copied: &mut CopiedCommand,
) {
    let Some(command) = fix_command(dep) else {
        return;
    };
    let now = ui.ctx().input(|i| i.time);
    let showing = copied.showing(dep.id, now);
    if showing {
        ui.ctx().request_repaint();
    }
    if widgets::command_block(ui, &command, showing) {
        copied.mark(dep.id, now);
    }
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
    last_failing: Option<Vec<DependencyRule>>,
    confirmed: bool,
}

impl GraceState {
    /// A snapshot taken while the session is still coming up can report a
    /// dependency that settles moments later, so the dialog gates on a failing
    /// set two consecutive snapshots agree on.
    pub fn observe(&mut self, failing: &[DependencyRule]) {
        self.confirmed = self.last_failing.as_deref() == Some(failing);
        self.last_failing = Some(failing.to_vec());
    }

    pub fn advance(&mut self, connected: bool, time: f64, grace_secs: f64) -> (bool, GraceAction) {
        match (connected, self.connected_at) {
            (true, None) => {
                self.connected_at = Some(time);
                self.recheck_sent = false;
                self.last_failing = None;
                self.confirmed = false;
            }
            (false, _) => {
                self.connected_at = None;
                self.recheck_sent = false;
                self.last_failing = None;
                self.confirmed = false;
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
        (within_grace || !self.confirmed, action)
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
    state: &TopicStore,
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
    state: &TopicStore,
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

    let passed = info.dependencies.len() - failing.len();
    let mut confirmed = false;
    let closed = widgets::dialog_with_subtitle(
        ctx,
        "healthcheck",
        &t!("depcheck.title"),
        &t!("depcheck.intro"),
        640.0,
        |ui| {
            ui.add_space(theme::SPACE_6);
            for dep in &failing {
                issue_card(ui, dep, &mut st.copied);
                ui.add_space(theme::SPACE_5);
            }
            if passed > 0 {
                ui.label(
                    RichText::new(t!("depcheck.passed", n = passed))
                        .font(theme::body_md())
                        .color(theme::TEXT_MUT),
                );
            }
        },
        |ui| {
            let size = egui::vec2(0.0, 36.0);
            if widgets::button(
                ui,
                &t!("depcheck.continue"),
                widgets::ButtonKind::Primary,
                size,
            )
            .clicked()
            {
                confirmed = true;
            }
            if widgets::button(
                ui,
                &t!("depcheck.check_again"),
                widgets::ButtonKind::Secondary,
                size,
            )
            .clicked()
            {
                crate::runtime::ipc::send(cmd, halod_shared::commands::DaemonCommand::GetDebugInfo);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if widgets::checkbox(ui, &t!("depcheck.dont_show_again"), st.dont_show_again)
                    .clicked()
                {
                    st.dont_show_again = !st.dont_show_again;
                }
            });
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
                    low_battery_notifications: state.gui.low_battery_notifications,
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

pub(crate) fn issue_card(ui: &mut egui::Ui, dep: &DependencyStatus, copied: &mut CopiedCommand) {
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = theme::SPACE_3;
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        RichText::new(title_text(dep))
                            .font(theme::heading())
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    let (tag, tone) = if dep.required {
                        (t!("depcheck.required"), widgets::TagTone::Warn)
                    } else {
                        (t!("depcheck.optional"), widgets::TagTone::Neutral)
                    };
                    widgets::tag(ui, &tag, tone);
                },
            );
            ui.label(
                RichText::new(t!("depcheck.affects", impact = impact_text(dep)))
                    .font(theme::body_md())
                    .color(theme::TEXT_SOFT),
            );
            ui.label(
                RichText::new(fix_text(dep))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            fix_command_block(ui, dep, copied);
        });
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

    /// Confirms `failing` so only the grace window still gates the dialog.
    fn confirm(g: &mut GraceState, failing: &[DependencyRule]) {
        g.observe(failing);
        g.observe(failing);
    }

    #[test]
    fn grace_suppresses_dialog_until_it_elapses() {
        let mut g = GraceState::default();
        let (suppressed, action) = g.advance(true, 0.0, 4.0);
        assert!(suppressed);
        assert!(matches!(action, GraceAction::RepaintAfter(secs) if secs == 4.0));
        confirm(&mut g, &[DependencyRule::Ffmpeg]);

        let (suppressed, action) = g.advance(true, 3.9, 4.0);
        assert!(suppressed);
        assert!(matches!(action, GraceAction::RepaintAfter(_)));

        let (suppressed, action) = g.advance(true, 4.1, 4.0);
        assert!(!suppressed);
        assert!(matches!(action, GraceAction::Recheck));

        // The recheck only fires once per grace window.
        let (suppressed, action) = g.advance(true, 5.0, 4.0);
        assert!(!suppressed);
        assert!(matches!(action, GraceAction::None));
    }

    #[test]
    fn grace_resets_on_disconnect_and_reconnect() {
        let mut g = GraceState::default();
        g.advance(true, 10.0, 4.0);
        confirm(&mut g, &[DependencyRule::Ffmpeg]);
        g.advance(true, 15.0, 4.0); // past grace, recheck already sent
        g.advance(false, 15.5, 4.0);

        // Reconnecting restarts the grace window from the new timestamp.
        let (suppressed, action) = g.advance(true, 20.0, 4.0);
        assert!(suppressed);
        assert!(matches!(action, GraceAction::RepaintAfter(secs) if secs == 4.0));
    }

    #[test]
    fn a_lone_snapshot_never_releases_the_dialog() {
        let mut g = GraceState::default();
        g.advance(true, 0.0, 4.0);
        g.observe(&[DependencyRule::GnomeExtension]);
        assert!(g.advance(true, 4.1, 4.0).0);
    }

    #[test]
    fn a_dependency_that_settles_never_releases_the_dialog() {
        let mut g = GraceState::default();
        g.advance(true, 0.0, 4.0);
        // The startup snapshot catches the shell mid-launch; the recheck is clean.
        g.observe(&[DependencyRule::GnomeExtension]);
        g.observe(&[]);
        assert!(g.advance(true, 4.1, 4.0).0);
    }

    #[test]
    fn reconnecting_discards_the_previous_confirmation() {
        let mut g = GraceState::default();
        g.advance(true, 0.0, 4.0);
        confirm(&mut g, &[DependencyRule::UdevRules]);
        assert!(!g.advance(true, 4.1, 4.0).0);

        g.advance(false, 5.0, 4.0);
        g.advance(true, 6.0, 4.0);
        g.observe(&[DependencyRule::UdevRules]);
        assert!(g.advance(true, 10.1, 4.0).0);
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
            dep(DependencyRule::Pactl, true, true),
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

    #[test]
    fn udev_install_command_generates_then_reloads_rules() {
        let command = udev_install_commands();
        assert!(command.contains("halod udev-rules"));
        assert!(command.contains("/etc/udev/rules.d/60-halod.rules"));
        assert!(command.contains("udevadm control --reload-rules"));
        assert!(command.contains("udevadm trigger"));
    }

    #[test]
    fn only_distro_independent_fixes_offer_a_command() {
        let with_command: Vec<DependencyRule> = DependencyRule::ALL
            .iter()
            .copied()
            .filter(|&id| fix_command(&dep(id, false, false)).is_some())
            .collect();
        assert_eq!(
            with_command,
            [
                DependencyRule::UdevRules,
                DependencyRule::I2cAccess,
                DependencyRule::HalodGroup
            ]
        );
        assert!(fix_command(&dep(DependencyRule::HalodGroup, false, false))
            .unwrap()
            .contains("usermod -aG halod"));
    }

    #[test]
    fn a_disabled_gnome_extension_gets_an_enable_command() {
        let mut gnome = dep(DependencyRule::GnomeExtension, false, false);
        gnome.fix_variant = "disabled".into();
        assert_eq!(
            fix_command(&gnome).as_deref(),
            Some("gnome-extensions enable companion@halod.timp4w.dev")
        );
        gnome.fix_variant = "missing".into();
        assert_eq!(fix_command(&gnome), None);
    }

    #[test]
    fn copied_feedback_follows_only_the_copied_rule() {
        let mut copied = CopiedCommand::default();
        assert!(!copied.showing(DependencyRule::HalodGroup, 1.0));
        copied.mark(DependencyRule::HalodGroup, 1.0);
        assert!(copied.showing(DependencyRule::HalodGroup, 1.0));
        assert!(!copied.showing(DependencyRule::UdevRules, 1.0));
        assert!(!copied.showing(DependencyRule::HalodGroup, 1000.0));
    }
}

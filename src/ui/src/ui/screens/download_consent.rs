// SPDX-License-Identifier: GPL-3.0-or-later
//! First-run prompt asking whether the daemon may contact GitHub to download
//! official plugins and check for updates. The decision persists in
//! `GuiConfig::plugin_downloads` and is revocable in Settings.

use egui::Vec2;
use halod_shared::types::{AppState, PluginDownloadConsent};

use crate::runtime::ipc::CommandTx;
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;

/// Whether to show the first-run consent prompt: only once the daemon is
/// connected, its stored decision is still `Unset`, and the user hasn't
/// deferred it this session.
pub fn should_prompt(state: &AppState, connected: bool, deferred: bool) -> bool {
    connected && !deferred && state.gui.plugin_downloads == PluginDownloadConsent::Unset
}

/// Outcome of a single frame of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No button pressed and not dismissed this frame.
    Pending,
    /// Allow GitHub access.
    Allow,
    /// Deny GitHub access.
    Deny,
    /// Dismissed without choosing — defer to the next launch.
    Defer,
}

/// Render the prompt and report the user's decision for this frame. Caller
/// dispatches `set_plugin_download_consent` for `Allow`/`Deny` and sets its
/// session-defer flag for `Defer`.
pub fn prompt(ctx: &egui::Context, _cmd: &CommandTx) -> Decision {
    let mut decision = Decision::Pending;
    let dismissed = widgets::dialog(
        ctx,
        "plugin_download_consent",
        &t!("plugins.download_consent_title"),
        460.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("plugins.download_consent_body"))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(t!("plugins.download_consent_note"))
                    .font(theme::body(11.5))
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("plugins.download_consent_allow"),
                ButtonKind::Primary,
                Vec2::new(150.0, 34.0),
            )
            .clicked()
            {
                decision = Decision::Allow;
            }
            if widgets::button(
                ui,
                &t!("plugins.download_consent_deny"),
                ButtonKind::Ghost,
                Vec2::new(130.0, 34.0),
            )
            .clicked()
            {
                decision = Decision::Deny;
            }
        },
    );
    if decision == Decision::Pending && dismissed {
        Decision::Defer
    } else {
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(consent: PluginDownloadConsent) -> AppState {
        let mut s = AppState::default();
        s.gui.plugin_downloads = consent;
        s
    }

    #[test]
    fn prompts_only_when_unset_connected_and_not_deferred() {
        assert!(should_prompt(
            &state_with(PluginDownloadConsent::Unset),
            true,
            false
        ));
        // Disconnected: nothing to consent to yet.
        assert!(!should_prompt(
            &state_with(PluginDownloadConsent::Unset),
            false,
            false
        ));
        // Deferred this session.
        assert!(!should_prompt(
            &state_with(PluginDownloadConsent::Unset),
            true,
            true
        ));
        // Already decided.
        assert!(!should_prompt(
            &state_with(PluginDownloadConsent::Allowed),
            true,
            false
        ));
        assert!(!should_prompt(
            &state_with(PluginDownloadConsent::Denied),
            true,
            false
        ));
    }
}

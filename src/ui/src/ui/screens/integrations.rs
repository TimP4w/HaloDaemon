// SPDX-License-Identifier: GPL-3.0-or-later
//! Integrations page — bridges to third-party engines/daemons (e.g. OpenRGB).
//! An integration is a `type = "integration"` plugin whose root device
//! carries no capabilities of its own (see `Device::integration_id`) and is
//! hidden from Home/sidebar; this page is where it's found, enabled, and
//! configured instead. The Plugins page still lists it and governs whether
//! its Lua may run at all — enabling/disabling it *as an integration* here is
//! a separate, independent toggle that only affects this one integration's
//! root device and the devices it exposes, never the whole device set.

use std::collections::HashMap;

use halod_shared::types::{AppState, PluginInfo, PluginKind};

use crate::runtime::ipc::CommandTx;
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::screens::plugin_config::{config_section, seed_config_edit_if_needed};
use crate::ui::screens::plugins::plugin_needs_permission;
use crate::ui::theme;

/// Whether `p` belongs on the Integrations page: an integration-type plugin
/// that is actually runnable — enabled from the Plugins screen (a disabled one
/// has no worker) and with its permissions granted (an ungranted one can never
/// connect). Showing an inert integration here — with a toggle that would
/// silently do nothing — would be misleading, so this page only lists ready
/// ones and never has to surface a permission prompt itself.
fn is_visible_integration(p: &PluginInfo) -> bool {
    p.plugin_type == PluginKind::Integration && p.enabled && !plugin_needs_permission(p)
}

/// Local UI state: which integration's Configure panel is expanded, plus its
/// config edit buffer (reuses the same seed/blank-secure-on-save discipline
/// as the Plugins screen — see `plugin_config::config_section`).
#[derive(Default)]
pub struct IntegrationsUi {
    expanded: Option<String>,
    config_edit: Option<(String, HashMap<String, String>)>,
}

/// How many devices an integration currently exposes, and whether its root
/// is connected. `None`/`0` when the integration isn't registered (disabled,
/// missing permissions, or its connection failed).
pub struct IntegrationStatus {
    pub connected: bool,
    pub device_count: usize,
}

/// Derive `plugin_id`'s live status from the current device set: its root is
/// the one `WireDevice` whose `integration_id` matches, and the devices it
/// exposes are the `IntegrationLeaf` children registered alongside it (ids
/// prefixed `{root_id}_ctrl_`, mirroring the daemon's own scheme).
pub fn integration_status(state: &AppState, plugin_id: &str) -> IntegrationStatus {
    let Some(root) = state
        .devices
        .iter()
        .find(|d| d.integration_id.as_deref() == Some(plugin_id))
    else {
        return IntegrationStatus {
            connected: false,
            device_count: 0,
        };
    };
    let prefix = format!("{}_ctrl_", root.id);
    let device_count = state
        .devices
        .iter()
        .filter(|d| d.id.starts_with(&prefix))
        .count();
    IntegrationStatus {
        connected: root.connected,
        device_count,
    }
}

/// The state a card's status row communicates, in priority order: "disabled"
/// (the user's own toggle) wins over "not yet connected" (a live hiccup).
/// Permission gating never appears here — an ungranted integration isn't shown
/// on this page at all (see `is_visible_integration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationState {
    Disabled,
    Connecting,
    Connected,
}

pub fn integration_state(p: &PluginInfo, status: &IntegrationStatus) -> IntegrationState {
    if !p.enabled || !p.integration_enabled {
        IntegrationState::Disabled
    } else if status.connected {
        IntegrationState::Connected
    } else {
        IntegrationState::Connecting
    }
}

impl IntegrationsUi {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        widgets::page_frame(ui, |ui| self.body(ui, state, cmd));
    }

    fn body(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        ui.label(
            egui::RichText::new(t!("integrations.title"))
                .font(theme::bold(22.0))
                .color(theme::TEXT),
        );
        ui.add_space(3.0);
        ui.label(
            egui::RichText::new(t!("integrations.subtitle"))
                .font(theme::body(12.0))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(18.0);

        let integrations: Vec<&PluginInfo> = state
            .plugins
            .plugins
            .iter()
            .filter(|p| is_visible_integration(p))
            .collect();

        if integrations.is_empty() {
            widgets::empty_state(
                ui,
                &t!("integrations.empty_title"),
                Some(&t!("integrations.empty_hint")),
            );
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                for p in integrations {
                    self.card(ui, state, p, cmd);
                }
            });
    }

    fn card(&mut self, ui: &mut egui::Ui, state: &AppState, p: &PluginInfo, cmd: &CommandTx) {
        widgets::card(ui, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&p.name)
                                    .font(theme::semibold(15.0))
                                    .color(theme::TEXT),
                            );
                            if !p.version.is_empty() {
                                ui.label(
                                    egui::RichText::new(&p.version)
                                        .font(theme::mono(10.5))
                                        .color(theme::TEXT_FAINT),
                                );
                            }
                        });
                        if !p.description.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(&p.description)
                                    .font(theme::body(11.5))
                                    .color(theme::TEXT_MUT),
                            );
                        }
                    });
                },
                |ui| {
                    let target = widgets::toggle(ui, p.integration_enabled);
                    if target != p.integration_enabled {
                        crate::domain::actions::integrations::set_integration_enabled(
                            cmd,
                            p.id.clone(),
                            target,
                        );
                    }
                },
            );

            ui.add_space(10.0);
            let status = integration_status(state, &p.id);
            let has_config = !p.config_fields.is_empty();
            let expanded = self.expanded.as_deref() == Some(p.id.as_str());

            // Status on the left, the Configure toggle pinned bottom-right.
            egui::Sides::new().show(
                ui,
                |ui| status_row(ui, p, &status),
                |ui| {
                    if has_config {
                        let label = if expanded {
                            t!("integrations.hide_configure")
                        } else {
                            t!("integrations.configure")
                        };
                        if widgets::button(
                            ui,
                            &label,
                            ButtonKind::Ghost,
                            egui::Vec2::new(120.0, 28.0),
                        )
                        .clicked()
                        {
                            self.expanded = if expanded { None } else { Some(p.id.clone()) };
                        }
                    }
                },
            );

            if has_config && expanded {
                ui.add_space(12.0);
                seed_config_edit_if_needed(&mut self.config_edit, &p.id, &p.config_values);
                let edits = &mut self.config_edit.as_mut().expect("just seeded above").1;
                config_section(ui, p, edits, |values| {
                    crate::domain::actions::integrations::set_integration_config(
                        cmd,
                        p.id.clone(),
                        values,
                    );
                });
            }
        });
    }
}

fn status_row(ui: &mut egui::Ui, p: &PluginInfo, status: &IntegrationStatus) {
    let (color, label) = match integration_state(p, status) {
        IntegrationState::Disabled => (theme::TEXT_FAINT2, t!("integrations.status_disabled")),
        IntegrationState::Connecting => (theme::STAT_AMBER, t!("integrations.status_connecting")),
        IntegrationState::Connected => (theme::ONLINE, t!("integrations.status_connected")),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color);
        ui.label(
            egui::RichText::new(label.as_ref())
                .font(theme::body(11.0))
                .color(color),
        );
        if status.device_count > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "· {}",
                    t!("integrations.devices_exposed", count = status.device_count)
                ))
                .font(theme::mono(10.5))
                .color(theme::TEXT_FAINT),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::WireDevice;

    fn plugin(id: &str, enabled: bool, integration_enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} integration"),
            path: String::new(),
            plugin_type: PluginKind::Integration,
            capabilities: vec![],
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
            source: Default::default(),
            declared_permissions: vec![],
            granted_permissions: vec![],
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
            integration_enabled,
            consented: true,
            content_changed: false,
        }
    }

    fn device(id: &str, integration_id: Option<&str>, connected: bool) -> WireDevice {
        WireDevice {
            id: id.into(),
            connected,
            integration_id: integration_id.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn is_visible_integration_excludes_lua_disabled_plugins() {
        // A plugin disabled from the Plugins screen has no worker, so it can
        // never connect — it must not show up here regardless of the
        // integration-specific toggle.
        assert!(is_visible_integration(&plugin("openrgb", true, true)));
        assert!(is_visible_integration(&plugin("openrgb", true, false)));
        assert!(!is_visible_integration(&plugin("openrgb", false, true)));
        assert!(!is_visible_integration(&plugin("openrgb", false, false)));
    }

    #[test]
    fn is_visible_integration_excludes_non_integration_plugins() {
        let mut p = plugin("driver", true, true);
        p.plugin_type = PluginKind::Device;
        assert!(!is_visible_integration(&p));
    }

    #[test]
    fn integration_status_is_absent_when_root_not_registered() {
        let state = AppState {
            devices: vec![device("other", None, true)],
            ..Default::default()
        };
        let status = integration_status(&state, "openrgb");
        assert!(!status.connected);
        assert_eq!(status.device_count, 0);
    }

    #[test]
    fn integration_status_counts_only_this_roots_children() {
        let state = AppState {
            devices: vec![
                device("openrgb-root", Some("openrgb"), true),
                device("openrgb-root_ctrl_0", None, true),
                device("openrgb-root_ctrl_1", None, true),
                // A second integration's root/child must never be counted.
                device("wled-root", Some("wled"), true),
                device("wled-root_ctrl_0", None, true),
                device("unrelated", None, true),
            ],
            ..Default::default()
        };
        let status = integration_status(&state, "openrgb");
        assert!(status.connected);
        assert_eq!(status.device_count, 2);
    }

    #[test]
    fn is_visible_integration_excludes_ungranted_permission_plugins() {
        // Regression: an enabled integration that still needs a permission
        // grant isn't runnable, so it must not appear on this page (which no
        // longer surfaces a permission prompt at all).
        let mut p = plugin("openrgb", true, true);
        p.declared_permissions = vec![halod_shared::types::Permission::Network];
        p.consented = false;
        assert!(!is_visible_integration(&p));

        // Once granted (consented), it becomes visible again.
        p.consented = true;
        assert!(is_visible_integration(&p));
    }

    #[test]
    fn integration_state_disabled_by_either_toggle() {
        let status = IntegrationStatus {
            connected: false,
            device_count: 0,
        };
        assert_eq!(
            integration_state(&plugin("openrgb", false, true), &status),
            IntegrationState::Disabled,
            "the generic plugin (Lua) toggle disables it too"
        );
        assert_eq!(
            integration_state(&plugin("openrgb", true, false), &status),
            IntegrationState::Disabled,
            "the integration-specific toggle disables it independently"
        );
    }

    #[test]
    fn integration_state_connecting_then_connected() {
        let p = plugin("openrgb", true, true);
        assert_eq!(
            integration_state(
                &p,
                &IntegrationStatus {
                    connected: false,
                    device_count: 0,
                }
            ),
            IntegrationState::Connecting
        );
        assert_eq!(
            integration_state(
                &p,
                &IntegrationStatus {
                    connected: true,
                    device_count: 3,
                }
            ),
            IntegrationState::Connected
        );
    }
}

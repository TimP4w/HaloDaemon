// SPDX-License-Identifier: GPL-3.0-or-later
//! Integrations page — bridges to third-party engines/daemons (e.g. OpenRGB).
//! An integration is a `type = "integration"` plugin whose root device
//! carries no capabilities of its own (see `Device::integration_id`) and is
//! hidden from Home/sidebar; this page is where it's found, enabled, and
//! configured instead. The Plugins page still lists it and governs whether
//! its Lua may run at all — enabling/disabling it *as an integration* here is
//! a separate, independent toggle that only affects this one integration's
//! root device and the devices it exposes, never the whole device set.

use std::collections::{HashMap, HashSet};

use egui::{Sense, Vec2};
use halod_shared::types::{AppState, PluginInfo, PluginIssue, PluginIssueKind, PluginKind};

use crate::domain::models::plugin_issues::plugin_issue_detail;
use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::screens::plugin_config::{config_section, seed_config_edit_if_needed};
use crate::ui::screens::plugins::{
    decode_new_assets, draw_logo_fit, initials_tile_at, plugin_needs_permission,
};
use crate::ui::theme;

/// Whether `p` belongs on the Integrations page: an integration-type plugin
/// that is actually runnable — enabled from the Plugins screen (a disabled one
/// has no worker) and with its permissions granted (an ungranted one can never
/// connect). Showing an inert integration here — with a toggle that would
/// silently do nothing — would be misleading, so this page only lists ready
/// ones and never has to surface a permission prompt itself.
pub(crate) fn is_visible_integration(p: &PluginInfo) -> bool {
    p.plugin_type == PluginKind::Integration && p.enabled && !plugin_needs_permission(p)
}

/// Local UI state: which integration's Configure panel is expanded, plus its
/// config edit buffer (reuses the same seed/blank-secure-on-save discipline
/// as the Plugins screen — see `plugin_config::config_section`).
#[derive(Default)]
pub struct IntegrationsUi {
    expanded: Option<String>,
    config_edit: Option<(String, HashMap<String, String>)>,
    /// Integrations whose enable/disable is applying → target state; toggle locked.
    in_flight: HashMap<String, bool>,
    /// Decoded logo textures, keyed like `ipc::plugin_asset_cache_key`.
    logo_textures: HashMap<String, egui::TextureHandle>,
    /// Cache keys already requested from the daemon, so a missing logo isn't
    /// re-requested every frame.
    requested_logos: HashSet<String>,
    /// Full integration runtime error opened from a card's error bar.
    issue_modal: Option<(String, String)>,
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
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        plugin_assets: &HashMap<String, Vec<u8>>,
    ) {
        self.sync_logos(ui.ctx(), cmd, &state.plugins.plugins, plugin_assets);
        widgets::page_frame(ui, |ui| self.body(ui, state, cmd));
        if let Some((title, detail)) = &self.issue_modal {
            if widgets::issue_modal(ui.ctx(), "integration_issue", title, detail) {
                self.issue_modal = None;
            }
        }
    }

    /// Request the logo of every visible integration that hasn't been fetched
    /// yet, then decode any bytes that have arrived.
    fn sync_logos(
        &mut self,
        ctx: &egui::Context,
        cmd: &CommandTx,
        plugins: &[PluginInfo],
        plugin_assets: &HashMap<String, Vec<u8>>,
    ) {
        for (plugin_id, name) in logos_to_request(plugins, plugin_assets, &self.requested_logos) {
            self.requested_logos
                .insert(ipc::plugin_asset_cache_key(&plugin_id, &name));
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::GetPluginAsset { plugin_id, name },
            );
        }
        decode_new_assets(ctx, plugin_assets, &mut self.logo_textures);
    }

    fn logo_texture(&self, p: &PluginInfo) -> Option<&egui::TextureHandle> {
        p.logo
            .as_deref()
            .map(|name| ipc::plugin_asset_cache_key(&p.id, name))
            .and_then(|key| self.logo_textures.get(&key))
    }

    fn body(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        ui.set_max_width(ui.available_width().min(840.0));
        reconcile_in_flight(&mut self.in_flight, &state.plugins.plugins);
        let title_resp = ui.label(
            egui::RichText::new(t!("integrations.title"))
                .font(theme::bold(22.0))
                .color(theme::TEXT),
        );
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::IntegrationsOverview,
            title_resp.rect,
        );
        ui.add_space(theme::SPACE_1);
        ui.label(
            egui::RichText::new(t!("integrations.subtitle"))
                .font(theme::body_md())
                .color(theme::TEXT_MUT),
        );
        ui.add_space(theme::SPACE_9);

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
        let locked = self.in_flight.get(&p.id).copied();
        let mut toggled: Option<bool> = None;
        widgets::card(ui, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                        match self.logo_texture(p) {
                            Some(tex) => draw_logo_fit(ui.painter(), rect, tex),
                            None => initials_tile_at(ui, rect, &p.name, &p.id),
                        }
                        ui.add_space(theme::SPACE_3);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(&p.name)
                                        .font(theme::title())
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
                                ui.add_space(theme::SPACE_2);
                                ui.label(
                                    egui::RichText::new(&p.description)
                                        .font(theme::body_sm())
                                        .color(theme::TEXT_MUT),
                                );
                            }
                        });
                    });
                },
                |ui| {
                    if let Some(target) = locked {
                        let toggle_anchor =
                            egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::new(34.0, 18.0));
                        crate::domain::tour::anchor(
                            ui.ctx(),
                            crate::domain::tour::AnchorId::IntegrationsToggle,
                            toggle_anchor,
                        );
                        ui.add_enabled_ui(false, |ui| {
                            widgets::toggle(ui, target);
                        });
                    } else {
                        let toggle_anchor =
                            egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::new(34.0, 18.0));
                        crate::domain::tour::anchor(
                            ui.ctx(),
                            crate::domain::tour::AnchorId::IntegrationsToggle,
                            toggle_anchor,
                        );
                        let target = widgets::toggle(ui, p.integration_enabled);
                        if target != p.integration_enabled {
                            toggled = Some(target);
                        }
                    }
                },
            );

            ui.add_space(theme::SPACE_5);
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

            if let Some(issue) = &p.health.issue {
                ui.add_space(theme::SPACE_5);
                if integration_issue_bar(ui, issue) {
                    self.issue_modal = Some((
                        t!("integrations.issue_modal_title", integration = &p.name).to_string(),
                        plugin_issue_detail(issue),
                    ));
                }
            }

            if has_config && expanded {
                ui.add_space(theme::SPACE_6);
                seed_config_edit_if_needed(&mut self.config_edit, &p.id, &p.config_values);
                let edits = &mut self.config_edit.as_mut().expect("just seeded above").1;
                config_section(ui, p, edits, |values| {
                    crate::runtime::ipc::send(
                        cmd,
                        halod_shared::commands::DaemonCommand::SetIntegrationConfig {
                            id: p.id.clone(),
                            values,
                        },
                    );
                });
            }
        });

        if let Some(target) = toggled {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetIntegrationEnabled {
                    id: p.id.clone(),
                    enabled: target,
                },
            );
            self.in_flight.insert(p.id.clone(), target);
        }
    }
}

/// Runtime/connection failure attached to one integration card. The short bar
/// keeps the page scannable; Details exposes the complete transport/Lua error.
fn integration_issue_bar(ui: &mut egui::Ui, issue: &PluginIssue) -> bool {
    let label = match issue.kind {
        PluginIssueKind::ConnectFailed => t!("integrations.issue_connect_failed"),
        PluginIssueKind::InitFailed => t!("plugins.issue_init_failed"),
        PluginIssueKind::RuntimeError => t!("integrations.issue_runtime_error"),
        PluginIssueKind::LoadWarning => t!("plugins.issue_load_warning"),
        PluginIssueKind::LoadFailed => t!("plugins.issue_load_failed"),
    };
    let details = t!("integrations.issue_details");
    widgets::Banner::danger(label.as_ref())
        .action(widgets::BannerAction::new(
            &details,
            ButtonKind::Ghost,
            Vec2::new(90.0, 28.0),
        ))
        .show(ui)
}

/// Drop landed (or vanished) in-flight toggles, unlocking them. Pure/testable.
fn reconcile_in_flight(in_flight: &mut HashMap<String, bool>, plugins: &[PluginInfo]) {
    in_flight.retain(|id, target| match plugins.iter().find(|p| &p.id == id) {
        Some(p) => p.integration_enabled != *target,
        None => false,
    });
}

/// Logos of visible integrations that aren't cached or already requested,
/// as `(plugin_id, asset_name)` pairs. Pure so it's unit-testable.
fn logos_to_request(
    plugins: &[PluginInfo],
    cache: &HashMap<String, Vec<u8>>,
    already_requested: &HashSet<String>,
) -> Vec<(String, String)> {
    plugins
        .iter()
        .filter(|p| is_visible_integration(p))
        .filter_map(|p| p.logo.as_deref().map(|name| (p, name)))
        .filter(|(p, name)| {
            let key = ipc::plugin_asset_cache_key(&p.id, name);
            !cache.contains_key(&key) && !already_requested.contains(&key)
        })
        .map(|(p, name)| (p.id.clone(), name.to_owned()))
        .collect()
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
                .font(theme::body_sm())
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
    for record in &p.data_records {
        let color = match record.status.as_str() {
            "fresh" => theme::ONLINE_TEXT,
            "stale" => theme::STAT_AMBER,
            _ => theme::TEXT_FAINT,
        };
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(&record.key)
                    .font(theme::mono(10.5))
                    .color(theme::TEXT_FAINT),
            );
            ui.label(
                egui::RichText::new(&record.status)
                    .font(theme::body_sm())
                    .color(color),
            );
        });
    }
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
            source: Default::default(),
            provenance: Default::default(),
            declared_permissions: vec![],
            authority: Default::default(),
            provides: vec![],
            consumes: vec![],
            data_records: vec![],
            accepted_authority: None,
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
            integration_enabled,
            consented: true,
            active: enabled,
            requirements: vec![],
            activation_blocker: None,
            health: Default::default(),
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
    fn logos_to_request_lists_visible_integrations_with_a_logo() {
        let mut visible = plugin("openrgb", true, true);
        visible.logo = Some("logo.png".into());
        // A logo-less integration and a Lua-disabled one contribute nothing.
        let no_logo = plugin("wled", true, true);
        let mut disabled = plugin("hidden", false, true);
        disabled.logo = Some("logo.png".into());

        let reqs = logos_to_request(
            &[visible, no_logo, disabled],
            &HashMap::new(),
            &HashSet::new(),
        );
        assert_eq!(reqs, vec![("openrgb".to_owned(), "logo.png".to_owned())]);
    }

    #[test]
    fn logos_to_request_skips_cached_and_already_requested() {
        let mut p = plugin("openrgb", true, true);
        p.logo = Some("logo.png".into());
        let key = ipc::plugin_asset_cache_key("openrgb", "logo.png");

        let mut cache = HashMap::new();
        cache.insert(key.clone(), vec![1, 2, 3]);
        assert!(logos_to_request(std::slice::from_ref(&p), &cache, &HashSet::new()).is_empty());

        let mut requested = HashSet::new();
        requested.insert(key);
        assert!(logos_to_request(&[p], &HashMap::new(), &requested).is_empty());
    }

    #[test]
    fn reconcile_unlocks_landed_and_vanished_integration_toggles() {
        // "enabling": target true, still disabled → stays locked.
        // "disabling": target false, now disabled → landed, unlock.
        // "gone": plugin no longer present → unlock.
        let mut in_flight = std::collections::HashMap::from([
            ("enabling".to_string(), true),
            ("disabling".to_string(), false),
            ("gone".to_string(), true),
        ]);
        let plugins = vec![
            plugin("enabling", true, false),
            plugin("disabling", true, false),
        ];

        reconcile_in_flight(&mut in_flight, &plugins);

        assert!(in_flight.contains_key("enabling"), "not enabled yet");
        assert!(!in_flight.contains_key("disabling"), "disable landed");
        assert!(!in_flight.contains_key("gone"), "vanished → unlocked");
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

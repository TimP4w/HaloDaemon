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
use halod_shared::types::{
    AppState, IntegrationLifecycleState, IntegrationSetupMode, IntegrationSetupPhase, PluginInfo,
    PluginIssue, PluginIssueKind, PluginKind,
};

use crate::domain::models::plugin_issues::plugin_issue_detail;
use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::screens::plugin_config::{
    config_fields_editor, config_section, config_values_to_send, seed_config_edit_if_needed,
};
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
    setup_open: Option<String>,
    setup_candidate: Option<String>,
    opened_oauth_urls: HashSet<String>,
    reset_confirm: Option<(String, String)>,
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
    Unconfigured,
    Configured,
    Active,
}

pub fn integration_state(p: &PluginInfo, status: &IntegrationStatus) -> IntegrationState {
    if !p.enabled || !p.integration_enabled {
        IntegrationState::Disabled
    } else if !p.integration_configured {
        IntegrationState::Unconfigured
    } else if status.connected || p.integration_state == IntegrationLifecycleState::Active {
        IntegrationState::Active
    } else {
        IntegrationState::Configured
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
        widgets::issue_modal_slot(ui.ctx(), "integration_issue", &mut self.issue_modal);
        self.reset_setup_modal(ui.ctx(), cmd);
        self.setup_modal(ui.ctx(), state, cmd);
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
        widgets::page_header(
            ui,
            &t!("integrations.title"),
            &t!("integrations.subtitle"),
            Some(crate::domain::tour::AnchorId::IntegrationsOverview),
        );

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
                    ui.horizontal(|ui| {
                        if can_reset_setup(p)
                            && widgets::button(
                                ui,
                                &t!("integrations.reset_setup"),
                                ButtonKind::Ghost,
                                egui::Vec2::new(110.0, 28.0),
                            )
                            .clicked()
                        {
                            self.reset_confirm = Some((p.id.clone(), p.name.clone()));
                        }
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
                    });
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
            if target && !p.integration_configured {
                self.setup_open = Some(p.id.clone());
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::BeginIntegrationSetup {
                        id: p.id.clone(),
                    },
                );
            } else {
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

    fn reset_setup_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some((_, name)) = self.reset_confirm.as_ref() else {
            return;
        };
        if let Some((id, _)) = widgets::confirm_delete_dialog(
            ctx,
            "integration_reset_setup",
            &t!("integrations.reset_setup_title"),
            &t!("integrations.reset_setup_confirm", name = name),
            &t!("integrations.reset_setup_action"),
            &mut self.reset_confirm,
        ) {
            self.setup_open = Some(id.clone());
            self.setup_candidate = None;
            self.config_edit = None;
            self.expanded = None;
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::ResetIntegrationSetup { id },
            );
        }
    }

    fn setup_modal(&mut self, ctx: &egui::Context, state: &AppState, cmd: &CommandTx) {
        if self.setup_open.is_none() {
            self.setup_open = state
                .plugins
                .plugins
                .iter()
                .find(|plugin| plugin.integration_setup.is_some())
                .map(|plugin| plugin.id.clone());
        }
        let Some(id) = self.setup_open.clone() else {
            return;
        };
        let Some(plugin) = state
            .plugins
            .plugins
            .iter()
            .find(|plugin| plugin.id == id)
            .cloned()
        else {
            self.setup_open = None;
            return;
        };
        let Some(setup) = plugin.integration_setup.clone() else {
            return;
        };
        let mut close = false;
        let title = format!("Connect {}", plugin.name);
        let dismissed =
            widgets::modal_frame_raw(ctx, "integration_setup", &title, 620.0, 470.0, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(setup_phase_label(&setup))
                            .font(theme::mono(10.5))
                            .color(theme::TEXT_FAINT),
                    );
                });
                ui.add_space(theme::SPACE_6);
                match setup.phase {
                    IntegrationSetupPhase::Init => {
                        self.setup_init(ui, &plugin, &setup, cmd);
                    }
                    IntegrationSetupPhase::Discovering => {
                        self.setup_discovery(ui, &plugin, &setup, cmd);
                    }
                    IntegrationSetupPhase::Pairing => {
                        self.setup_pairing(ui, &plugin, &setup, cmd);
                    }
                    IntegrationSetupPhase::Done => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(70.0);
                            let (symbol, color, heading) = if setup.success {
                                ("✓", theme::ONLINE, format!("{} connected", plugin.name))
                            } else {
                                ("!", theme::TRAFFIC_RED, "Setup failed".to_owned())
                            };
                            ui.label(egui::RichText::new(symbol).size(42.0).color(color));
                            ui.label(
                                egui::RichText::new(heading)
                                    .font(theme::heading())
                                    .color(theme::TEXT),
                            );
                            if let Some(error) = &setup.error {
                                ui.label(
                                    egui::RichText::new(error)
                                        .font(theme::body_sm())
                                        .color(theme::TEXT_MUT),
                                );
                            }
                        });
                        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                            if widgets::button(
                                ui,
                                "Finish",
                                ButtonKind::Primary,
                                egui::vec2(110.0, 40.0),
                            )
                            .clicked()
                            {
                                close = true;
                            }
                        });
                    }
                }
            });
        if dismissed || close {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::CancelIntegrationSetup { id },
            );
            self.setup_open = None;
            self.setup_candidate = None;
            self.config_edit = None;
        }
    }

    fn setup_init(
        &mut self,
        ui: &mut egui::Ui,
        plugin: &PluginInfo,
        setup: &halod_shared::types::IntegrationSetupStatus,
        cmd: &CommandTx,
    ) {
        if setup.selected_mode.is_none() && setup.modes.len() > 1 {
            ui.label(
                egui::RichText::new("How should Halo find the device?")
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_6);
            for (mode, label, detail) in [
                (
                    IntegrationSetupMode::Automatic,
                    "Find devices automatically",
                    "Search the local network using this integration's declared discovery services.",
                ),
                (
                    IntegrationSetupMode::Manual,
                    "Enter connection details",
                    "Configure the address and other required values yourself.",
                ),
            ] {
                if setup.modes.contains(&mode)
                    && ui
                        .button(format!("{label}\n{detail}"))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, mode);
                }
            }
            return;
        }
        let mode = setup
            .selected_mode
            .or_else(|| setup.modes.first().copied())
            .unwrap_or(IntegrationSetupMode::Manual);
        if mode == IntegrationSetupMode::Automatic {
            ui.label(
                egui::RichText::new("Find devices automatically")
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new("Halo will search the local network for compatible devices.")
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                if widgets::button(ui, "Search", ButtonKind::Primary, egui::vec2(110.0, 40.0))
                    .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, mode);
                }
            });
            return;
        }
        ui.label(
            egui::RichText::new("Connection details")
                .font(theme::heading())
                .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_4);
        seed_config_edit_if_needed(&mut self.config_edit, &plugin.id, &plugin.config_values);
        let edits = &mut self.config_edit.as_mut().expect("setup edit seeded").1;
        config_fields_editor(ui, plugin, edits);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            if widgets::button(ui, "Continue", ButtonKind::Primary, egui::vec2(110.0, 40.0))
                .clicked()
            {
                let values = config_values_to_send(edits, &plugin.config_fields);
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::SubmitIntegrationSetup {
                        id: plugin.id.clone(),
                        candidate_id: None,
                        values,
                    },
                );
            }
        });
    }

    fn setup_discovery(
        &mut self,
        ui: &mut egui::Ui,
        plugin: &PluginInfo,
        setup: &halod_shared::types::IntegrationSetupStatus,
        cmd: &CommandTx,
    ) {
        ui.label(
            egui::RichText::new("Choose a device")
                .font(theme::heading())
                .color(theme::TEXT),
        );
        ui.label(
            egui::RichText::new(
                setup
                    .message
                    .as_deref()
                    .unwrap_or("Devices found on your local network"),
            )
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
        );
        ui.add_space(theme::SPACE_5);
        if let Some(error) = &setup.error {
            widgets::Banner::danger(error).show(ui);
            ui.add_space(theme::SPACE_4);
        }
        if setup.candidates.is_empty() && setup.message.is_none() {
            widgets::Banner::warn("No devices found. Refresh or enter the address manually.")
                .show(ui);
        }
        for candidate in &setup.candidates {
            let selected = self.setup_candidate.as_deref() == Some(candidate.id.as_str());
            if ui
                .selectable_label(selected, &candidate.name)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                self.setup_candidate = Some(candidate.id.clone());
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if widgets::button(ui, "Continue", ButtonKind::Primary, egui::vec2(110.0, 40.0))
                    .clicked()
                {
                    if let Some(candidate_id) = self.setup_candidate.clone() {
                        crate::runtime::ipc::send(
                            cmd,
                            halod_shared::commands::DaemonCommand::SubmitIntegrationSetup {
                                id: plugin.id.clone(),
                                candidate_id: Some(candidate_id),
                                values: HashMap::new(),
                            },
                        );
                    }
                }
                if setup.modes.contains(&IntegrationSetupMode::Manual)
                    && widgets::button(
                        ui,
                        "Enter manually",
                        ButtonKind::Ghost,
                        egui::vec2(130.0, 40.0),
                    )
                    .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Manual);
                }
                if widgets::button(ui, "Refresh", ButtonKind::Ghost, egui::vec2(90.0, 40.0))
                    .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Automatic);
                }
            });
        });
    }

    fn setup_pairing(
        &mut self,
        ui: &mut egui::Ui,
        plugin: &PluginInfo,
        setup: &halod_shared::types::IntegrationSetupStatus,
        cmd: &CommandTx,
    ) {
        if setup.message.is_some() && setup.external_url.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(90.0);
                ui.spinner();
                ui.add_space(theme::SPACE_5);
                ui.label(
                    egui::RichText::new(setup.message.as_deref().unwrap_or("Pairing…"))
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new("Please wait while Halo completes the secure handshake.")
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            });
            return;
        }
        if let Some(url) = &setup.external_url {
            if self.opened_oauth_urls.insert(url.clone()) {
                let _ = webbrowser::open(url);
            }
            ui.label(
                egui::RichText::new("Authorize in your browser")
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.label("Halo is waiting for the secure OAuth2 callback.");
            if widgets::button(
                ui,
                "Open browser",
                ButtonKind::Primary,
                egui::vec2(130.0, 40.0),
            )
            .clicked()
            {
                let _ = webbrowser::open(url);
            }
            return;
        }
        ui.label(
            egui::RichText::new(
                setup
                    .title
                    .as_deref()
                    .unwrap_or("Put the device in pairing mode"),
            )
            .font(theme::heading())
            .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_5);
        for (index, instruction) in setup.instructions.iter().enumerate() {
            ui.label(format!("{}. {instruction}", index + 1));
        }
        if let Some(error) = &setup.error {
            ui.add_space(theme::SPACE_5);
            widgets::Banner::danger(error).show(ui);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if widgets::button(
                    ui,
                    "Start pairing",
                    ButtonKind::Primary,
                    egui::vec2(130.0, 40.0),
                )
                .clicked()
                {
                    crate::runtime::ipc::send(
                        cmd,
                        halod_shared::commands::DaemonCommand::RetryIntegrationPairing {
                            id: plugin.id.clone(),
                        },
                    );
                }
                if widgets::button(ui, "Back", ButtonKind::Ghost, egui::vec2(90.0, 40.0)).clicked()
                {
                    send_setup_mode(
                        cmd,
                        &plugin.id,
                        setup
                            .selected_mode
                            .or_else(|| setup.modes.first().copied())
                            .unwrap_or(IntegrationSetupMode::Manual),
                    );
                }
            });
        });
    }
}

fn setup_phase_label(setup: &halod_shared::types::IntegrationSetupStatus) -> &'static str {
    match setup.phase {
        IntegrationSetupPhase::Init | IntegrationSetupPhase::Discovering => "Connect · 1 / 4",
        IntegrationSetupPhase::Pairing
            if setup.message.is_some() && setup.external_url.is_none() =>
        {
            "Pairing · 3 / 4"
        }
        IntegrationSetupPhase::Pairing if setup.external_url.is_some() => "Authorize · 2 / 4",
        IntegrationSetupPhase::Pairing => "Pairing mode · 2 / 4",
        IntegrationSetupPhase::Done => "Done · 4 / 4",
    }
}

fn can_reset_setup(plugin: &PluginInfo) -> bool {
    plugin.integration_configured && plugin.integration_requires_setup
}

fn send_setup_mode(cmd: &CommandTx, id: &str, mode: IntegrationSetupMode) {
    crate::runtime::ipc::send(
        cmd,
        halod_shared::commands::DaemonCommand::SelectIntegrationSetupMode {
            id: id.to_owned(),
            mode,
        },
    );
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
    crate::domain::state::retain_in_flight(in_flight, plugins, |p, target| {
        p.integration_enabled == target
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
        IntegrationState::Unconfigured => (theme::STAT_AMBER, "Unconfigured".into()),
        IntegrationState::Configured => (theme::STAT_AMBER, "Configured".into()),
        IntegrationState::Active => (theme::ONLINE, "Active".into()),
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
            integration_configured: integration_enabled,
            integration_requires_setup: false,
            integration_state: if integration_enabled {
                IntegrationLifecycleState::Configured
            } else {
                IntegrationLifecycleState::Disabled
            },
            integration_setup: None,
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
    fn integration_state_configured_then_active() {
        let p = plugin("openrgb", true, true);
        assert_eq!(
            integration_state(
                &p,
                &IntegrationStatus {
                    connected: false,
                    device_count: 0,
                }
            ),
            IntegrationState::Configured
        );
        assert_eq!(
            integration_state(
                &p,
                &IntegrationStatus {
                    connected: true,
                    device_count: 3,
                }
            ),
            IntegrationState::Active
        );
    }

    #[test]
    fn pairing_progress_uses_the_third_modal_step() {
        let mut setup = halod_shared::types::IntegrationSetupStatus {
            phase: IntegrationSetupPhase::Pairing,
            ..Default::default()
        };
        assert_eq!(setup_phase_label(&setup), "Pairing mode · 2 / 4");
        setup.message = Some("Pairing…".into());
        assert_eq!(setup_phase_label(&setup), "Pairing · 3 / 4");
    }

    #[test]
    fn reset_setup_is_available_only_after_an_interactive_setup() {
        let mut p = plugin("nanoleaf", true, true);
        p.integration_requires_setup = true;
        assert!(can_reset_setup(&p));
        p.integration_configured = false;
        assert!(!can_reset_setup(&p));
        p.integration_configured = true;
        p.integration_requires_setup = false;
        assert!(!can_reset_setup(&p));
    }
}

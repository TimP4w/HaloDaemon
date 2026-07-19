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

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use halod_shared::types::{
    IntegrationLifecycleState, IntegrationSetupMode, IntegrationSetupPhase, PluginInfo,
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
    /// Host serial ports for the `serial_port` config dropdown, mirrored from the
    /// App's cache each frame so the deep config renderers can read them.
    serial_ports: Vec<halod_shared::types::SerialPortInfo>,
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
pub fn integration_status(state: &TopicStore, plugin_id: &str) -> IntegrationStatus {
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
    pub fn release_textures(&mut self) {
        self.logo_textures.clear();
        self.requested_logos.clear();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &TopicStore,
        cmd: &CommandTx,
        plugin_assets: &HashMap<String, Vec<u8>>,
        serial_ports: &[halod_shared::types::SerialPortInfo],
    ) {
        self.serial_ports = serial_ports.to_vec();
        self.sync_logos(ui.ctx(), cmd, &state.plugins.plugins, plugin_assets);
        widgets::page_frame(ui, |ui| self.body(ui, state, cmd));
        self.reset_setup_modal(ui.ctx(), cmd);
        self.setup_modal(ui.ctx(), state, cmd);
        // Rendered last so a "Details" opened from inside the setup modal (or a
        // card's error bar) stacks on top of it.
        widgets::issue_modal_slot(ui.ctx(), "integration_issue", &mut self.issue_modal);
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

    fn body(&mut self, ui: &mut egui::Ui, state: &TopicStore, cmd: &CommandTx) {
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

    fn card(&mut self, ui: &mut egui::Ui, state: &TopicStore, p: &PluginInfo, cmd: &CommandTx) {
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
                                            .font(theme::body(10.5))
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
                config_section(ui, p, edits, &self.serial_ports, |values| {
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

    fn setup_modal(&mut self, ctx: &egui::Context, state: &TopicStore, cmd: &CommandTx) {
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
        let title = t!("integrations.setup_connect", name = plugin.name).to_string();
        let time = ctx.input(|i| i.time) as f32;
        let dismissed =
            widgets::modal_frame_raw(ctx, "integration_setup", &title, 620.0, 500.0, |ui| {
                setup_rail(ui, &setup);
                ui.add_space(theme::SPACE_8);
                match setup.phase {
                    IntegrationSetupPhase::Init => self.setup_init(ui, &plugin, &setup, cmd),
                    IntegrationSetupPhase::Discovering => {
                        self.setup_discovery(ui, &plugin, &setup, cmd, time)
                    }
                    IntegrationSetupPhase::Pairing => {
                        self.setup_pairing(ui, &plugin, &setup, cmd, time)
                    }
                    IntegrationSetupPhase::Done => close = setup_done(ui, &plugin, &setup),
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
            step_heading(ui, &t!("integrations.setup_method_title"));
            step_subtext(ui, &t!("integrations.setup_method_sub"));
            ui.add_space(theme::SPACE_6);
            if setup.modes.contains(&IntegrationSetupMode::Automatic)
                && option_card(
                    ui,
                    paint_scan_icon,
                    &t!("integrations.setup_method_auto"),
                    &t!("integrations.setup_method_auto_desc"),
                )
            {
                send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Automatic);
            }
            ui.add_space(theme::SPACE_5);
            if setup.modes.contains(&IntegrationSetupMode::Manual)
                && option_card(
                    ui,
                    paint_fields_icon,
                    &t!("integrations.setup_method_manual"),
                    &t!("integrations.setup_method_manual_desc"),
                )
            {
                send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Manual);
            }
            return;
        }
        let mode = setup
            .selected_mode
            .or_else(|| setup.modes.first().copied())
            .unwrap_or(IntegrationSetupMode::Manual);
        if mode == IntegrationSetupMode::Automatic {
            step_heading(ui, &t!("integrations.setup_auto_title"));
            step_subtext(ui, &t!("integrations.setup_auto_sub"));
            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                if widgets::button(
                    ui,
                    &t!("integrations.setup_search"),
                    ButtonKind::Primary,
                    egui::vec2(110.0, 40.0),
                )
                .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, mode);
                }
            });
            return;
        }
        step_heading(ui, &t!("integrations.setup_manual_title"));
        step_subtext(ui, &t!("integrations.setup_manual_sub"));
        ui.add_space(theme::SPACE_5);
        seed_config_edit_if_needed(&mut self.config_edit, &plugin.id, &plugin.config_values);
        let edits = &mut self.config_edit.as_mut().expect("setup edit seeded").1;
        config_fields_editor(ui, plugin, edits, &self.serial_ports);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            if widgets::button(
                ui,
                &t!("integrations.setup_continue"),
                ButtonKind::Primary,
                egui::vec2(110.0, 40.0),
            )
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
        time: f32,
    ) {
        step_heading(ui, &t!("integrations.setup_choose_device"));
        // While the worker is still probing, the daemon keeps `message` set and
        // `candidates` empty (see the integration setup usecase).
        if let Some(message) = &setup.message {
            ui.add_space(theme::SPACE_6);
            scanning_card(ui, &t!(message.as_str()), time);
            return;
        }
        step_subtext(ui, &t!("integrations.setup_found_on_network"));
        ui.add_space(theme::SPACE_5);
        if let Some(error) = &setup.error {
            self.setup_error_bar(ui, plugin, error);
            ui.add_space(theme::SPACE_4);
        }
        if setup.candidates.is_empty() {
            empty_state(
                ui,
                &t!("integrations.setup_no_devices"),
                &t!("integrations.setup_no_devices_hint"),
            );
        } else {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height(230.0)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = theme::SPACE_5;
                    for candidate in &setup.candidates {
                        let selected =
                            self.setup_candidate.as_deref() == Some(candidate.id.as_str());
                        if device_row(ui, &candidate.name, &candidate.id, selected) {
                            self.setup_candidate = Some(candidate.id.clone());
                        }
                    }
                });
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                let ready = self.setup_candidate.is_some();
                if setup_primary(ui, &t!("integrations.setup_continue"), ready) {
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
                        &t!("integrations.setup_enter_manually"),
                        ButtonKind::Ghost,
                        egui::vec2(130.0, 40.0),
                    )
                    .clicked()
                {
                    send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Manual);
                }
                if widgets::button(
                    ui,
                    &t!("integrations.setup_refresh"),
                    ButtonKind::Ghost,
                    egui::vec2(90.0, 40.0),
                )
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
        time: f32,
    ) {
        if setup.message.is_some() && setup.external_url.is_none() {
            ui.add_space(theme::SPACE_12);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
                theme::glow(ui.painter(), rect.center(), 44.0, theme::PROGRESS_A, 0.28);
                widgets::paint_spinner(ui.painter(), rect.center(), 46.0, time);
                ui.add_space(theme::SPACE_9);
                let key = setup
                    .message
                    .as_deref()
                    .unwrap_or("integrations.setup_pairing_fallback");
                ui.label(
                    RichText::new(t!(key))
                        .font(theme::bold(18.0))
                        .color(theme::TEXT),
                );
                ui.add_space(theme::SPACE_3);
                ui.label(
                    RichText::new(t!("integrations.setup_pairing_wait"))
                        .font(theme::body_md())
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(theme::SPACE_9);
                widgets::progress_bar(ui, 320.0, 7.0, None);
            });
            ui.ctx().request_repaint();
            return;
        }
        if let Some(url) = &setup.external_url {
            if self.opened_oauth_urls.insert(url.clone()) {
                let _ = webbrowser::open(url);
            }
            step_heading(ui, &t!("integrations.setup_authorize_title"));
            step_subtext(ui, &t!("integrations.setup_authorize_sub"));
            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                if widgets::button(
                    ui,
                    &t!("integrations.setup_open_browser"),
                    ButtonKind::Primary,
                    egui::vec2(140.0, 40.0),
                )
                .clicked()
                {
                    let _ = webbrowser::open(url);
                }
            });
            return;
        }
        let pair_title = t!("integrations.setup_pair_title_fallback");
        step_heading(ui, setup.title.as_deref().unwrap_or(&pair_title));
        ui.add_space(theme::SPACE_6);
        pairing_hero(ui, &plugin.name, time);
        ui.add_space(theme::SPACE_8);
        for (index, instruction) in setup.instructions.iter().enumerate() {
            instruction_row(ui, index + 1, instruction);
            ui.add_space(theme::SPACE_5);
        }
        if let Some(error) = &setup.error {
            ui.add_space(theme::SPACE_3);
            self.setup_error_bar(ui, plugin, error);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if widgets::button(
                    ui,
                    &t!("integrations.setup_start_pairing"),
                    ButtonKind::Primary,
                    egui::vec2(140.0, 40.0),
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
                if widgets::button(
                    ui,
                    &t!("integrations.setup_back"),
                    ButtonKind::Ghost,
                    egui::vec2(90.0, 40.0),
                )
                .clicked()
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

    /// A compact one-line error with a "Details" action that opens the shared
    /// issue modal (with copy) on the full text — so a long stack trace never
    /// inflates the modal. Reuses the same bar/modal as a card's runtime error.
    fn setup_error_bar(&mut self, ui: &mut egui::Ui, plugin: &PluginInfo, error: &str) {
        let short = error
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(error);
        let details = t!("integrations.issue_details");
        if widgets::Banner::danger(short)
            .action(widgets::BannerAction::new(
                &details,
                ButtonKind::Ghost,
                Vec2::new(90.0, 28.0),
            ))
            .show(ui)
        {
            self.issue_modal = Some((
                t!("integrations.issue_modal_title", integration = &plugin.name).to_string(),
                error.to_owned(),
            ));
        }
    }
}

/// The rail's four stages; the index of the active one comes from [`rail_step`].
const RAIL_STEP_COUNT: usize = 4;

/// Localized uppercase label for rail stage `i`.
fn rail_label(i: usize) -> String {
    match i {
        0 => t!("integrations.setup_step_discover"),
        1 => t!("integrations.setup_step_prepare"),
        2 => t!("integrations.setup_step_pair"),
        _ => t!("integrations.setup_step_done"),
    }
    .to_uppercase()
}

fn rail_step(setup: &halod_shared::types::IntegrationSetupStatus) -> usize {
    match setup.phase {
        IntegrationSetupPhase::Init | IntegrationSetupPhase::Discovering => 0,
        IntegrationSetupPhase::Pairing
            if setup.message.is_some() && setup.external_url.is_none() =>
        {
            2
        }
        IntegrationSetupPhase::Pairing => 1,
        IntegrationSetupPhase::Done => 3,
    }
}

/// The gradient step rail across the modal top: filled bars for reached steps,
/// a track bar for the rest, with the active step's label picked out in accent.
/// One equal column per step — all sharing a top edge — so the bars line up on a
/// single baseline instead of stair-stepping.
fn setup_rail(ui: &mut egui::Ui, setup: &halod_shared::types::IntegrationSetupStatus) {
    let cur = rail_step(setup);
    ui.columns(RAIL_STEP_COUNT, |cols| {
        for (i, col) in cols.iter_mut().enumerate() {
            let w = col.available_width();
            widgets::progress_bar(col, w, 4.0, Some(if i <= cur { 1.0 } else { 0.0 }));
            col.add_space(theme::SPACE_1);
            let color = if i == cur {
                theme::PROGRESS_A
            } else if i < cur {
                theme::TEXT_MUT
            } else {
                theme::TEXT_FAINT2
            };
            col.vertical_centered(|ui| {
                ui.label(
                    RichText::new(rail_label(i))
                        .font(theme::bold(10.5))
                        .color(color),
                );
            });
        }
    });
}

fn step_heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::bold(18.0))
            .color(theme::TEXT),
    );
}

fn step_subtext(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::SPACE_2);
    ui.label(
        RichText::new(text)
            .font(theme::body_md())
            .color(theme::TEXT_MUT),
    );
}

/// A primary button that dims to non-interactive when `ready` is false.
fn setup_primary(ui: &mut egui::Ui, label: &str, ready: bool) -> bool {
    let size = egui::vec2(110.0, 40.0);
    if ready {
        widgets::button(ui, label, ButtonKind::Primary, size).clicked()
    } else {
        widgets::button_disabled(ui, label, ButtonKind::Primary, size);
        false
    }
}

/// One selectable method card (icon tile · title · description · chevron).
/// Returns whether it was clicked this frame.
fn option_card(
    ui: &mut egui::Ui,
    paint_icon: fn(&egui::Painter, Rect),
    title: &str,
    desc: &str,
) -> bool {
    let resp = egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_LG)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Sides::new().show(
                ui,
                |ui| {
                    let (tile, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
                    theme::paint_well(ui.painter(), tile, theme::RADIUS_MD);
                    paint_icon(ui.painter(), tile);
                    ui.add_space(theme::SPACE_7);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(title)
                                .font(theme::heading())
                                .color(theme::TEXT),
                        );
                        ui.add_space(theme::SPACE_1);
                        ui.label(
                            RichText::new(desc)
                                .font(theme::body_sm())
                                .color(theme::TEXT_MUT),
                        );
                    });
                },
                |ui| {
                    ui.label(
                        RichText::new("›")
                            .font(theme::title())
                            .color(theme::TEXT_FAINT),
                    );
                },
            );
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.clicked()
}

/// Concentric-ring "scan the network" glyph, centered in `tile`.
fn paint_scan_icon(p: &egui::Painter, tile: Rect) {
    let c = tile.center();
    p.circle_stroke(c, 10.0, Stroke::new(1.5, theme::TEXT_FAINT2));
    p.circle_stroke(c, 6.0, Stroke::new(1.5, theme::TEXT_MUT));
    p.circle_filled(c, 2.6, theme::PROGRESS_A);
}

/// Stacked-lines "type the details" glyph, centered in `tile`.
fn paint_fields_icon(p: &egui::Painter, tile: Rect) {
    let c = tile.center();
    for (i, (half, color)) in [
        (10.0, theme::TEXT_FAINT2),
        (10.0, theme::TEXT_MUT),
        (6.0, theme::TEXT_FAINT2),
    ]
    .into_iter()
    .enumerate()
    {
        let y = c.y - 6.0 + i as f32 * 6.0;
        p.line_segment(
            [Pos2::new(c.x - half, y), Pos2::new(c.x + half, y)],
            Stroke::new(2.0, color),
        );
    }
}

/// A discovered-device row with a live-status dot and a radio selector.
fn device_row(ui: &mut egui::Ui, name: &str, id: &str, selected: bool) -> bool {
    let resp = egui::Frame::NONE
        .fill(if selected {
            theme::ROW_ACTIVE
        } else {
            theme::CARD_BG
        })
        .stroke(Stroke::new(
            1.0,
            if selected { theme::CYAN } else { theme::BORDER },
        ))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            let full_w = ui.available_width();
            ui.set_width(full_w);
            const DOT: f32 = 9.0;
            const RADIO_GUTTER: f32 = 34.0;
            let indent = DOT + theme::SPACE_3;
            ui.allocate_ui_with_layout(
                Vec2::new((full_w - RADIO_GUTTER).max(40.0), 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(DOT), Sense::hover());
                        theme::glow(ui.painter(), dot.center(), 5.0, theme::ONLINE, 0.7);
                        ui.painter().circle_filled(dot.center(), 4.5, theme::ONLINE);
                        ui.add_space(theme::SPACE_3);
                        ui.label(
                            RichText::new(name)
                                .font(theme::heading())
                                .color(theme::TEXT),
                        );
                    });
                    if !id.is_empty() {
                        ui.horizontal(|ui| {
                            ui.add_space(indent);
                            ui.label(
                                RichText::new(id)
                                    .font(theme::value_xs())
                                    .color(theme::TEXT_MUT),
                            );
                        });
                    }
                },
            );
            let rect = ui.min_rect();
            let radio_c = Pos2::new(rect.right() - 10.0, rect.center().y);
            let ring = if selected {
                theme::CYAN
            } else {
                theme::TEXT_FAINT2
            };
            ui.painter()
                .circle_stroke(radio_c, 9.0, Stroke::new(2.0, ring));
            if selected {
                ui.painter().circle_filled(radio_c, 5.0, theme::CYAN);
            }
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.clicked()
}

/// The "searching your network" card: spinner beside a live status line.
fn scanning_card(ui: &mut egui::Ui, message: &str, time: f32) {
    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_LG)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                widgets::paint_spinner(ui.painter(), rect.center(), 22.0, time);
                ui.add_space(theme::SPACE_6);
                ui.label(
                    RichText::new(message)
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            });
        });
    ui.ctx().request_repaint();
}

/// Centered "nothing found" placeholder with a dashed marker.
fn empty_state(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.add_space(theme::SPACE_10);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
        ui.painter()
            .circle_stroke(rect.center(), 21.0, Stroke::new(1.5, theme::TEXT_FAINT2));
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "?",
            theme::bold(18.0),
            theme::TEXT_FAINT,
        );
        ui.add_space(theme::SPACE_5);
        ui.label(
            RichText::new(title)
                .font(theme::heading())
                .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_2);
        ui.allocate_ui_with_layout(
            Vec2::new(320.0, 0.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.label(
                    RichText::new(hint)
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            },
        );
    });
}

/// The "press the link button" illustration: a pulsing beacon in a well.
fn pairing_hero(ui: &mut egui::Ui, label: &str, time: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 132.0), Sense::hover());
    theme::paint_well(ui.painter(), rect, theme::RADIUS_LG);
    let c = rect.center();
    for (phase, color) in [(0.0, theme::PROGRESS_A), (0.95, theme::PROGRESS_B)] {
        let p = (time + phase).rem_euclid(1.9) / 1.9;
        let alpha = (1.0 - p).clamp(0.0, 1.0);
        ui.painter().circle_stroke(
            c,
            7.0 + p * 26.0,
            Stroke::new(1.5, theme::a(color, 0.85 * alpha)),
        );
    }
    theme::glow(ui.painter(), c, 16.0, theme::PROGRESS_A, 0.6);
    ui.painter().circle_filled(
        c,
        7.0,
        theme::lerp_color(theme::PROGRESS_A, egui::Color32::WHITE, 0.4),
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(14.0, -12.0),
        Align2::LEFT_BOTTOM,
        label.to_uppercase(),
        theme::value_xs(),
        theme::TEXT_FAINT2,
    );
    ui.ctx().request_repaint();
}

/// A numbered instruction line (accent index chip · text).
fn instruction_row(ui: &mut egui::Ui, n: usize, text: &str) {
    ui.horizontal(|ui| {
        let (chip, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        ui.painter()
            .circle_filled(chip.center(), 12.0, theme::a(theme::PROGRESS_A, 0.14));
        ui.painter().circle_stroke(
            chip.center(),
            12.0,
            Stroke::new(1.0, theme::a(theme::PROGRESS_A, 0.5)),
        );
        ui.painter().text(
            chip.center(),
            Align2::CENTER_CENTER,
            n.to_string(),
            theme::value_sm(),
            theme::PROGRESS_A,
        );
        ui.add_space(theme::SPACE_5);
        // Wrap: a label in a horizontal layout won't wrap on its own, so a long
        // (e.g. localized) instruction would overrun the modal instead of
        // flowing under itself.
        ui.add(
            egui::Label::new(
                RichText::new(text)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            )
            .wrap(),
        );
    });
}

/// The terminal step: a success (or failure) badge with a Finish action.
/// Returns whether Finish was clicked.
fn setup_done(
    ui: &mut egui::Ui,
    plugin: &PluginInfo,
    setup: &halod_shared::types::IntegrationSetupStatus,
) -> bool {
    let mut finish = false;
    ui.add_space(theme::SPACE_12);
    ui.vertical_centered(|ui| {
        if setup.success {
            widgets::success_check(ui, 68.0);
            ui.add_space(theme::SPACE_9);
            ui.label(
                RichText::new(t!("integrations.setup_connected", name = plugin.name))
                    .font(theme::bold(21.0))
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_3);
            ui.label(
                RichText::new(t!("integrations.setup_connected_sub"))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(68.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 34.0, theme::a(theme::OFFLINE, 0.14));
            ui.painter()
                .circle_stroke(rect.center(), 34.0, Stroke::new(1.0, theme::OFFLINE));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "!",
                theme::bold(30.0),
                theme::OFFLINE,
            );
            ui.add_space(theme::SPACE_9);
            ui.label(
                RichText::new(t!("integrations.setup_failed"))
                    .font(theme::bold(21.0))
                    .color(theme::TEXT),
            );
            if let Some(error) = &setup.error {
                // Only the first line, so a long trace never blows up the modal.
                let short = error.lines().find(|line| !line.trim().is_empty());
                ui.add_space(theme::SPACE_3);
                ui.label(
                    RichText::new(short.unwrap_or(error))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            }
        }
    });
    ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
        if widgets::button(
            ui,
            &t!("integrations.setup_finish"),
            ButtonKind::Primary,
            egui::vec2(110.0, 40.0),
        )
        .clicked()
        {
            finish = true;
        }
    });
    finish
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
        IntegrationState::Unconfigured => {
            (theme::STAT_AMBER, t!("integrations.status_unconfigured"))
        }
        IntegrationState::Configured => (theme::STAT_AMBER, t!("integrations.status_configured")),
        IntegrationState::Active => (theme::ONLINE, t!("integrations.status_active")),
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
                    .font(theme::body(10.5))
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
        let state = TopicStore {
            devices: vec![device("other", None, true)],
            ..Default::default()
        };
        let status = integration_status(&state, "openrgb");
        assert!(!status.connected);
        assert_eq!(status.device_count, 0);
    }

    #[test]
    fn integration_status_counts_only_this_roots_children() {
        let state = TopicStore {
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
    fn rail_advances_from_prepare_to_pair_when_the_handshake_starts() {
        // "Press the link button" sits on Prepare; once the daemon posts a
        // pairing message (and it isn't the OAuth browser step), the rail
        // advances to Pair — but an OAuth handshake stays on Prepare/authorize.
        let mut setup = halod_shared::types::IntegrationSetupStatus {
            phase: IntegrationSetupPhase::Pairing,
            ..Default::default()
        };
        assert_eq!(rail_step(&setup), 1);
        setup.message = Some("Pairing…".into());
        assert_eq!(rail_step(&setup), 2);
        setup.external_url = Some("https://example/oauth".into());
        assert_eq!(rail_step(&setup), 1);
    }

    #[test]
    fn rail_covers_discover_through_done() {
        let step = |phase| {
            rail_step(&halod_shared::types::IntegrationSetupStatus {
                phase,
                ..Default::default()
            })
        };
        assert_eq!(step(IntegrationSetupPhase::Init), 0);
        assert_eq!(step(IntegrationSetupPhase::Discovering), 0);
        assert_eq!(step(IntegrationSetupPhase::Done), 3);
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

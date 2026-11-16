// SPDX-License-Identifier: GPL-3.0-or-later
//! The configurable half of an integration plugin, rendered inside its Plugins
//! detail pane: connection status, settings, the devices it exposes, and the
//! guided setup. Enabling the plugin itself stays on the Plugins toggle.

use std::collections::{HashMap, HashSet};

use crate::domain::state::Page;
use crate::domain::topic_store::TopicStore;
use egui::{Align2, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{
    IntegrationLifecycleState, IntegrationSetupMode, IntegrationSetupPhase, PluginInfo,
    PluginIssue, PluginIssueKind, WireDevice,
};

use crate::domain::models::plugin_issues::plugin_issue_detail;
use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::screens::plugin_config::{
    config_fields_editor, config_section, config_values_to_send, seed_config_edit_if_needed,
};
use crate::ui::screens::plugins::plugin_name;
use crate::ui::theme;

#[derive(Default)]
pub struct IntegrationPanel {
    config_edit: Option<(String, HashMap<String, String>)>,
    /// Integrations whose resume is applying → target state.
    in_flight: HashMap<String, bool>,
    issue_modal: Option<(String, String)>,
    setup_candidate: Option<String>,
    opened_oauth_urls: HashSet<String>,
    reset_confirm: Option<(String, String)>,
    serial_ports: Vec<halod_shared::types::SerialPortInfo>,
}

pub struct IntegrationStatus {
    pub connected: bool,
    pub device_count: usize,
}

/// Devices an integration exposes: children of its root, whose ids carry the
/// `{root_id}_ctrl_` prefix the daemon assigns.
fn exposed_devices<'a>(
    state: &'a TopicStore,
    plugin_id: &str,
) -> impl Iterator<Item = &'a WireDevice> {
    let prefix = state
        .devices
        .iter()
        .find(|d| d.integration_id.as_deref() == Some(plugin_id))
        .map(|root| format!("{}_ctrl_", root.id));
    state
        .devices
        .iter()
        .filter(move |d| prefix.as_deref().is_some_and(|p| d.id.starts_with(p)))
}

pub fn integration_status(state: &TopicStore, plugin_id: &str) -> IntegrationStatus {
    IntegrationStatus {
        connected: state
            .devices
            .iter()
            .any(|d| d.integration_id.as_deref() == Some(plugin_id) && d.connected),
        device_count: exposed_devices(state, plugin_id).count(),
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelView {
    Hidden,
    Setup,
    NotSetUp,
    Paused,
    Running,
}

/// An unconfigured integration is always paused too, so setup must win over
/// the paused/resume state: resuming is refused until setup has run.
fn panel_view(p: &PluginInfo) -> PanelView {
    if !p.enabled {
        PanelView::Hidden
    } else if p.integration_setup.is_some() {
        PanelView::Setup
    } else if !p.integration_configured {
        PanelView::NotSetUp
    } else if !p.integration_enabled {
        PanelView::Paused
    } else {
        PanelView::Running
    }
}

fn connection_address(p: &PluginInfo) -> Option<String> {
    let value = |kind| {
        p.config_fields
            .iter()
            .find(|f| f.kind == kind)
            .and_then(|f| p.config_values.get(&f.key))
            .filter(|v| !v.is_empty())
    };
    let host = value(halod_shared::types::PluginConfigFieldKind::Host)?;
    Some(
        match value(halod_shared::types::PluginConfigFieldKind::Port) {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        },
    )
}

fn section_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::SPACE_8);
    ui.label(
        RichText::new(text)
            .font(theme::semibold(15.0))
            .color(theme::TEXT),
    );
    ui.add_space(theme::SPACE_4);
}

/// Right-aligned row of setup actions under a step's content.
fn action_row(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(theme::SPACE_9);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 36.0),
        egui::Layout::right_to_left(egui::Align::Center),
        body,
    );
}

impl IntegrationPanel {
    /// Dialogs owned by the panel; call once per frame after the page body.
    pub fn modals(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        if self.reset_confirm.is_some() {
            if let Some((id, _)) = widgets::confirm_delete_dialog(
                ctx,
                "integration_reset_setup",
                &t!("integrations.reset_setup_title"),
                &t!(
                    "integrations.reset_setup_confirm",
                    name = self.reset_confirm.as_ref().map_or("", |(_, n)| n.as_str())
                ),
                &t!("integrations.reset_setup_action"),
                &mut self.reset_confirm,
            ) {
                self.setup_candidate = None;
                self.config_edit = None;
                ipc::send(cmd, DaemonCommand::ResetIntegrationSetup { id });
            }
        }
        widgets::issue_modal_slot(ctx, "integration_issue", &mut self.issue_modal);
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &TopicStore,
        p: &PluginInfo,
        cmd: &CommandTx,
        serial_ports: &[halod_shared::types::SerialPortInfo],
        page: &mut Page,
    ) {
        let view = panel_view(p);
        if view == PanelView::Hidden {
            return;
        }
        self.serial_ports = serial_ports.to_vec();
        reconcile_in_flight(&mut self.in_flight, &state.plugins.plugins);
        ui.add_space(theme::SPACE_8);

        match view {
            PanelView::Hidden => {}
            PanelView::Setup => {
                if let Some(setup) = &p.integration_setup {
                    self.setup_panel(ui, p, setup, cmd);
                }
            }
            PanelView::NotSetUp => {
                let label = t!("integrations.set_up");
                if widgets::Banner::info(&t!("integrations.not_set_up"))
                    .action(widgets::BannerAction::new(
                        &label,
                        ButtonKind::Primary,
                        Vec2::new(90.0, 30.0),
                    ))
                    .show(ui)
                {
                    ipc::send(
                        cmd,
                        DaemonCommand::BeginIntegrationSetup { id: p.id.clone() },
                    );
                }
            }
            PanelView::Paused => {
                let resuming = self.in_flight.contains_key(&p.id);
                let label = t!("integrations.resume");
                if widgets::Banner::neutral(&t!("integrations.paused"))
                    .action(
                        widgets::BannerAction::new(
                            &label,
                            ButtonKind::Primary,
                            Vec2::new(90.0, 30.0),
                        )
                        .loading(resuming),
                    )
                    .show(ui)
                {
                    self.in_flight.insert(p.id.clone(), true);
                    ipc::send(
                        cmd,
                        DaemonCommand::SetIntegrationEnabled {
                            id: p.id.clone(),
                            enabled: true,
                        },
                    );
                }
                self.connection_section(ui, p, cmd);
            }
            PanelView::Running => self.running(ui, state, p, cmd, page),
        }
    }

    fn running(
        &mut self,
        ui: &mut egui::Ui,
        state: &TopicStore,
        p: &PluginInfo,
        cmd: &CommandTx,
        page: &mut Page,
    ) {
        let status = integration_status(state, &p.id);
        let state_label = match integration_state(p, &status) {
            IntegrationState::Active => {
                let text = match connection_address(p) {
                    Some(address) => t!(
                        "integrations.connected_to",
                        address = address,
                        count = status.device_count
                    ),
                    None => t!("integrations.connected", count = status.device_count),
                };
                widgets::Banner::ok(&text).show(ui);
                t!("integrations.status_active")
            }
            _ => {
                widgets::Banner::warn(&t!("integrations.status_connecting")).show(ui);
                t!("integrations.status_configured")
            }
        };
        if let Some(issue) = &p.health.issue {
            ui.add_space(theme::SPACE_5);
            if integration_issue_bar(ui, issue) {
                self.issue_modal = Some((
                    t!(
                        "integrations.issue_modal_title",
                        integration = plugin_name(p)
                    )
                    .to_string(),
                    plugin_issue_detail(issue),
                ));
            }
        }
        ui.add_space(theme::SPACE_9);
        widgets::stat_pair(
            ui,
            (
                &t!("integrations.devices"),
                &status.device_count.to_string(),
            ),
            (&t!("integrations.status"), &state_label),
        );

        self.connection_section(ui, p, cmd);

        if status.device_count > 0 {
            section_heading(ui, &t!("integrations.devices_title"));
            for device in exposed_devices(state, &p.id) {
                if device_link_row(ui, device) {
                    *page = Page::Device(device.id.clone());
                }
            }
        }

        if can_reset_setup(p) {
            ui.add_space(theme::SPACE_8);
            widgets::divider(ui);
            ui.add_space(theme::SPACE_8);
            if widgets::button(
                ui,
                &t!("integrations.reset_setup"),
                ButtonKind::Danger,
                Vec2::new(100.0, 30.0),
            )
            .clicked()
            {
                self.reset_confirm = Some((p.id.clone(), plugin_name(p).to_owned()));
            }
        }
    }

    fn connection_section(&mut self, ui: &mut egui::Ui, p: &PluginInfo, cmd: &CommandTx) {
        if p.config_fields.is_empty() {
            return;
        }
        section_heading(ui, &t!("integrations.connection"));
        seed_config_edit_if_needed(&mut self.config_edit, &p.id, &p.config_values);
        let edits = &mut self.config_edit.as_mut().expect("just seeded above").1;
        config_section(
            ui,
            p,
            edits,
            &self.serial_ports,
            &t!("integrations.save_reconnect"),
            |values| {
                ipc::send(
                    cmd,
                    DaemonCommand::SetIntegrationConfig {
                        id: p.id.clone(),
                        values,
                    },
                );
            },
        );
    }

    fn setup_panel(
        &mut self,
        ui: &mut egui::Ui,
        plugin: &PluginInfo,
        setup: &halod_shared::types::IntegrationSetupStatus,
        cmd: &CommandTx,
    ) {
        let time = ui.input(|i| i.time) as f32;
        let mut close = false;
        let labels = rail_labels();
        let step = rail_step(setup);
        egui::Frame::NONE
            .fill(theme::INNER_BG)
            .stroke(Stroke::new(1.0, theme::ACCENT_BORDER))
            .corner_radius(theme::RADIUS_MD)
            .inner_margin(Margin::same(20))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new(t!(
                                "integrations.setup_connect",
                                name = plugin_name(plugin)
                            ))
                            .font(theme::semibold(16.0))
                            .color(theme::TEXT),
                        );
                    },
                    |ui| {
                        ui.label(
                            RichText::new(t!(
                                "integrations.setup_step_of",
                                current = step + 1,
                                total = labels.len()
                            ))
                            .font(theme::body_md())
                            .color(theme::TEXT_DIM),
                        );
                    },
                );
                ui.add_space(theme::SPACE_6);
                ui.add_space(theme::SPACE_6);
                widgets::progress_steps(ui, &labels, step);
                ui.add_space(theme::SPACE_9);
                match setup.phase {
                    IntegrationSetupPhase::Init => self.setup_init(ui, plugin, setup, cmd),
                    IntegrationSetupPhase::Discovering => {
                        self.setup_discovery(ui, plugin, setup, cmd, time)
                    }
                    IntegrationSetupPhase::Pairing => {
                        self.setup_pairing(ui, plugin, setup, cmd, time)
                    }
                    IntegrationSetupPhase::Done => close = setup_done(ui, plugin, setup),
                }
            });
        if close {
            self.cancel_setup(cmd, &plugin.id);
        }
    }

    pub fn cancel_setup(&mut self, cmd: &CommandTx, id: &str) {
        ipc::send(
            cmd,
            DaemonCommand::CancelIntegrationSetup { id: id.to_owned() },
        );
        self.setup_candidate = None;
        self.config_edit = None;
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
            action_row(ui, |ui| {
                if widgets::button(
                    ui,
                    &t!("integrations.setup_search"),
                    ButtonKind::Primary,
                    egui::vec2(100.0, 36.0),
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
        action_row(ui, |ui| {
            if widgets::button(
                ui,
                &t!("integrations.setup_continue"),
                ButtonKind::Primary,
                egui::vec2(100.0, 36.0),
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
            widgets::empty_state(
                ui,
                &t!("integrations.setup_no_devices"),
                Some(&t!("integrations.setup_no_devices_hint")),
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
        action_row(ui, |ui| {
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
                    egui::vec2(120.0, 30.0),
                )
                .clicked()
            {
                send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Manual);
            }
            if widgets::button(
                ui,
                &t!("integrations.setup_refresh"),
                ButtonKind::Ghost,
                egui::vec2(80.0, 30.0),
            )
            .clicked()
            {
                send_setup_mode(cmd, &plugin.id, IntegrationSetupMode::Automatic);
            }
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
            ui.add_space(theme::SPACE_8);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
                theme::glow(ui.painter(), rect.center(), 44.0, theme::CYAN, 0.28);
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
            action_row(ui, |ui| {
                if widgets::button(
                    ui,
                    &t!("integrations.setup_open_browser"),
                    ButtonKind::Primary,
                    egui::vec2(120.0, 36.0),
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
        pairing_hero(ui, plugin_name(plugin), time);
        ui.add_space(theme::SPACE_8);
        for (index, instruction) in setup.instructions.iter().enumerate() {
            instruction_row(ui, index + 1, instruction);
            ui.add_space(theme::SPACE_5);
        }
        if let Some(error) = &setup.error {
            ui.add_space(theme::SPACE_3);
            self.setup_error_bar(ui, plugin, error);
        }
        action_row(ui, |ui| {
            if widgets::button(
                ui,
                &t!("integrations.setup_start_pairing"),
                ButtonKind::Primary,
                egui::vec2(120.0, 36.0),
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
                ButtonKind::Secondary,
                egui::vec2(80.0, 30.0),
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
                ButtonKind::Secondary,
                Vec2::new(90.0, 28.0),
            ))
            .show(ui)
        {
            self.issue_modal = Some((
                t!(
                    "integrations.issue_modal_title",
                    integration = plugin_name(plugin)
                )
                .to_string(),
                error.to_owned(),
            ));
        }
    }
}

fn rail_labels() -> Vec<String> {
    [
        "integrations.setup_step_discover",
        "integrations.setup_step_prepare",
        "integrations.setup_step_pair",
        "integrations.setup_step_done",
    ]
    .into_iter()
    .map(|key| t!(key).to_string())
    .collect()
}

/// A device the integration exposes, with an Open action into its page.
fn device_link_row(ui: &mut egui::Ui, device: &WireDevice) -> bool {
    let mut open = false;
    widgets::inset(ui, Margin::symmetric(12, 10), |ui| {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(
                        RichText::new(&device.name)
                            .font(theme::medium(14.0))
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new(crate::domain::models::device::type_label(device))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                });
            },
            |ui| {
                open = widgets::text_link(ui, &t!("integrations.open_device")).clicked();
            },
        );
    });
    ui.add_space(theme::SPACE_4);
    open
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

fn step_heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::semibold(14.0))
            .color(theme::TEXT),
    );
}

fn step_subtext(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::SPACE_2);
    ui.label(
        RichText::new(text)
            .font(theme::body_md())
            .color(theme::TEXT_DIM),
    );
}

/// A primary button that dims to non-interactive when `ready` is false.
fn setup_primary(ui: &mut egui::Ui, label: &str, ready: bool) -> bool {
    widgets::button_enabled(
        ui,
        label,
        ButtonKind::Primary,
        egui::vec2(100.0, 36.0),
        ready,
    )
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
    p.circle_filled(c, 2.6, theme::CYAN);
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
    widgets::card_with_margin(ui, Margin::same(16), |ui| {
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

/// The "press the link button" illustration: a pulsing beacon in a well.
fn pairing_hero(ui: &mut egui::Ui, label: &str, time: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 132.0), Sense::hover());
    theme::paint_well(ui.painter(), rect, theme::RADIUS_LG);
    let c = rect.center();
    for (phase, color) in [(0.0, theme::CYAN), (0.95, theme::CYAN)] {
        let p = (time + phase).rem_euclid(1.9) / 1.9;
        let alpha = (1.0 - p).clamp(0.0, 1.0);
        ui.painter().circle_stroke(
            c,
            7.0 + p * 26.0,
            Stroke::new(1.5, theme::a(color, 0.85 * alpha)),
        );
    }
    theme::glow(ui.painter(), c, 16.0, theme::CYAN, 0.6);
    ui.painter().circle_filled(
        c,
        7.0,
        theme::lerp_color(theme::CYAN, egui::Color32::WHITE, 0.4),
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(14.0, -12.0),
        Align2::LEFT_BOTTOM,
        label,
        theme::caption(),
        theme::TEXT_MUT,
    );
    ui.ctx().request_repaint();
}

/// A numbered instruction line (accent index chip · text).
fn instruction_row(ui: &mut egui::Ui, n: usize, text: &str) {
    ui.horizontal(|ui| {
        let (chip, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        ui.painter()
            .circle_filled(chip.center(), 12.0, theme::a(theme::CYAN, 0.14));
        ui.painter().circle_stroke(
            chip.center(),
            12.0,
            Stroke::new(1.0, theme::a(theme::CYAN, 0.5)),
        );
        ui.painter().text(
            chip.center(),
            Align2::CENTER_CENTER,
            n.to_string(),
            theme::value_sm(),
            theme::CYAN,
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
    ui.add_space(theme::SPACE_8);
    ui.vertical_centered(|ui| {
        if setup.success {
            widgets::success_check(ui, 68.0);
            ui.add_space(theme::SPACE_9);
            ui.label(
                RichText::new(t!(
                    "integrations.setup_connected",
                    name = plugin_name(plugin)
                ))
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
    action_row(ui, |ui| {
        if widgets::button(
            ui,
            &t!("integrations.setup_finish"),
            ButtonKind::Primary,
            egui::vec2(100.0, 36.0),
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
            ButtonKind::Secondary,
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

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::PluginKind;

    fn plugin(id: &str, enabled: bool, integration_enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} integration"),
            translations: Default::default(),
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
            repo: String::new(),
            provenance: Default::default(),
            access: Default::default(),
            provides: vec![],
            consumes: vec![],
            data_records: vec![],
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
    fn exposed_devices_are_this_roots_children_only() {
        let state = TopicStore {
            devices: vec![
                device("openrgb-root", Some("openrgb"), true),
                device("openrgb-root_ctrl_0", None, true),
                device("wled-root_ctrl_0", None, true),
            ],
            ..Default::default()
        };
        let ids: Vec<&str> = exposed_devices(&state, "openrgb")
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, vec!["openrgb-root_ctrl_0"]);
    }

    #[test]
    fn rail_labels_match_the_rail_steps() {
        for phase in [
            IntegrationSetupPhase::Init,
            IntegrationSetupPhase::Discovering,
            IntegrationSetupPhase::Pairing,
            IntegrationSetupPhase::Done,
        ] {
            let setup = halod_shared::types::IntegrationSetupStatus {
                phase,
                ..Default::default()
            };
            assert!(rail_step(&setup) < rail_labels().len());
        }
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
    fn an_unconfigured_integration_offers_setup_instead_of_resume() {
        let mut p = plugin("openrgb", true, false);
        p.integration_configured = false;
        p.integration_requires_setup = true;
        assert_eq!(panel_view(&p), PanelView::NotSetUp);
        p.integration_setup = Some(Default::default());
        assert_eq!(panel_view(&p), PanelView::Setup);
    }

    #[test]
    fn a_configured_integration_shows_its_connection_form_paused_or_running() {
        let mut paused = plugin("weather", true, false);
        paused.integration_configured = true;
        assert_eq!(panel_view(&paused), PanelView::Paused);
        assert_eq!(
            panel_view(&plugin("openrgb", true, true)),
            PanelView::Running
        );
    }

    #[test]
    fn the_panel_is_hidden_until_the_plugin_is_enabled() {
        assert_eq!(
            panel_view(&plugin("openrgb", false, true)),
            PanelView::Hidden
        );
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

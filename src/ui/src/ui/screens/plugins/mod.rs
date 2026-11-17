// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    AvailablePlugin, PluginInfo, PluginIssue, PluginIssueKind, PluginProvenance,
    PluginRecommendation, PluginRepoInfo, PluginRepoLocation, PluginRequirement,
    PluginRequirementStatus, PluginsState, RepoTrust, RequirementImpact,
};

use crate::domain::models::plugin_issues::plugin_issue_detail;
use crate::domain::models::udev::udev_rules_need_action;
use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::icons;
use crate::ui::theme;
use halod_shared::commands::DaemonCommand;

mod access;
mod repo;
mod udev;

pub(crate) use access::EnablePrompt;
use access::*;
use udev::*;

pub(crate) fn plugin_name(plugin: &PluginInfo) -> &str {
    plugin.localized_name(&rust_i18n::locale())
}

fn plugin_description(plugin: &PluginInfo) -> &str {
    plugin.localized_description(&rust_i18n::locale())
}

#[derive(Default)]
struct AddRepoState {
    url: String,
}

/// Failsafe for a refresh that never lands (e.g. an unreachable remote).
const REPO_CHECK_TIMEOUT: f64 = 25.0;

struct RepoCheck {
    slug: String,
    prev_sync: Option<String>,
    started: f64,
}

/// What the detail column shows: a plugin, a repo, or nothing (empty state).
#[derive(Default, Clone, PartialEq, Eq, Debug)]
enum Selection {
    Plugin(String),
    Repo(String),
    #[default]
    None,
}

#[derive(Default)]
pub struct PluginsUi {
    selection: Selection,
    add_repo: Option<AddRepoState>,
    pending_repo_delete: Option<String>,
    pending_uninstall: Option<String>,
    checking_repo: Option<RepoCheck>,
    /// Plugin ids whose enable/disable is applying → target state; the toggle is
    /// locked at the target until the daemon confirms.
    in_flight: HashMap<String, bool>,
    requested_assets: HashSet<String>,
    asset_textures: HashMap<String, egui::TextureHandle>,
    /// Plugin ids whose install or update is in flight → egui start time.
    installing: HashMap<String, f64>,
    updating_all: Option<f64>,
    issue_modal: Option<(String, String)>,
    udev_rule_details: Option<Vec<String>>,
    #[cfg(target_os = "linux")]
    next_udev_status_check: f64,
    udev_command_copied: bool,
    udev_info_open: bool,
    udev_rechecking: Option<f64>,
    filter: usize,
    search: String,
    integration: crate::ui::screens::plugin_integration::IntegrationPanel,
    pub(crate) enable_prompt: EnablePrompt,
}

/// The plugin list's type filter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PluginFilter {
    All,
    Devices,
    Integrations,
    Effects,
}

impl PluginFilter {
    const ALL: [PluginFilter; 4] = [Self::All, Self::Devices, Self::Integrations, Self::Effects];

    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::All => t!("plugins.filter_all"),
            Self::Devices => t!("plugins.filter_devices"),
            Self::Integrations => t!("plugins.filter_integrations"),
            Self::Effects => t!("plugins.filter_effects"),
        }
    }

    fn admits(self, kind: halod_shared::types::PluginKind) -> bool {
        use halod_shared::types::PluginKind;
        match self {
            Self::All => true,
            Self::Devices => kind == PluginKind::Device,
            Self::Integrations => kind == PluginKind::Integration,
            Self::Effects => matches!(kind, PluginKind::Effect | PluginKind::Lcd),
        }
    }
}

/// How long the udev "Recheck rules" button shows its spinner, giving the
/// action visible feedback while the fresh status is fetched.
const UDEV_RECHECK_SPINNER: f64 = 1.0;

/// Failsafe for an install that never lands (e.g. an unreachable remote).
const UPDATE_TIMEOUT: f64 = 90.0;

/// Per-frame inputs of the Plugins page that live outside the topic store.
pub struct PluginsCtx<'a> {
    pub state: &'a TopicStore,
    pub cmd: &'a CommandTx,
    pub assets: &'a HashMap<String, Vec<u8>>,
    pub udev: Option<&'a halod_shared::types::UdevRulesStatus>,
    pub serial_ports: &'a [halod_shared::types::SerialPortInfo],
}

impl PluginsUi {
    /// A failed command arrives as a notification; stop the spinners then
    /// rather than at the failsafe timeout.
    pub fn observe_notifications(&mut self, notifications: &[halod_shared::types::Notification]) {
        if notifications.iter().any(|notification| {
            matches!(
                notification.code,
                halod_shared::types::NotificationCode::Generic { .. }
            )
        }) {
            self.installing.clear();
            self.updating_all = None;
        }
    }

    #[cfg(all(test, feature = "screenshots"))]
    pub(crate) fn select_plugin(&mut self, id: &str) {
        self.selection = Selection::Plugin(id.to_owned());
    }

    pub fn release_textures(&mut self) {
        self.asset_textures.clear();
        self.requested_assets.clear();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PluginsCtx,
        page: &mut crate::domain::state::Page,
    ) {
        let now = ui.input(|input| input.time);
        #[cfg(target_os = "linux")]
        if now >= self.next_udev_status_check {
            ipc::send(cx.cmd, DaemonCommand::GetUdevRulesStatus);
            self.next_udev_status_check = now + 5.0;
        }
        self.selection = resolve_selection(
            self.selection.clone(),
            &cx.state.plugins.plugins,
            &cx.state.plugins.repos,
        );
        self.sync_assets(ui.ctx(), cx.cmd, &cx.state.plugins.plugins, cx.assets);

        self.body(ui, cx, page);
        self.integration.modals(ui.ctx(), cx.cmd);

        self.add_repo_modal(ui.ctx(), cx.cmd);
        self.repo_delete_modal(ui.ctx(), cx.cmd);
        self.uninstall_modal(ui.ctx(), cx.cmd);
        widgets::issue_modal_slot(ui.ctx(), "plugin_issue_page", &mut self.issue_modal);
        if self.udev_rule_details.is_some()
            && udev_rule_details_modal(
                ui.ctx(),
                self.udev_rule_details.as_deref().unwrap_or_default(),
            )
        {
            self.udev_rule_details = None;
        }
        if self.udev_info_open {
            if let Some(status) = cx.udev.filter(|status| status.supported) {
                let rechecking = self
                    .udev_rechecking
                    .is_some_and(|started| now - started < UDEV_RECHECK_SPINNER);
                if !rechecking {
                    self.udev_rechecking = None;
                }
                let (close, recheck) =
                    udev_info_modal(ui.ctx(), status, rechecking, &mut self.udev_command_copied);
                if recheck && !rechecking {
                    self.udev_rechecking = Some(now);
                    ipc::send(cx.cmd, DaemonCommand::GetUdevRulesStatus);
                }
                if rechecking {
                    ui.ctx().request_repaint();
                }
                if close {
                    self.udev_info_open = false;
                }
            } else {
                self.udev_info_open = false;
            }
        }
    }

    /// Request undeclared assets for the selected plugin and decode new bytes.
    fn sync_assets(
        &mut self,
        ctx: &egui::Context,
        cmd: &CommandTx,
        plugins: &[PluginInfo],
        plugin_assets: &HashMap<String, Vec<u8>>,
    ) {
        if let Selection::Plugin(id) = &self.selection {
            if let Some(p) = plugins.iter().find(|p| &p.id == id) {
                for (plugin_id, name) in assets_to_request(p, plugin_assets, &self.requested_assets)
                {
                    self.requested_assets
                        .insert(ipc::plugin_asset_cache_key(&plugin_id, &name));
                    ipc::send(cmd, DaemonCommand::GetPluginAsset { plugin_id, name });
                }
            }
        }
        decode_new_assets(ctx, plugin_assets, &mut self.asset_textures);
    }

    fn logo(&self, p: &PluginInfo) -> Option<egui::TextureHandle> {
        p.logo
            .as_deref()
            .map(|name| ipc::plugin_asset_cache_key(&p.id, name))
            .and_then(|key| self.asset_textures.get(&key).cloned())
    }

    fn locked_target(&self, p: &PluginInfo) -> Option<bool> {
        if is_load_failed(p) {
            Some(false)
        } else {
            self.in_flight.get(&p.id).copied()
        }
    }

    fn toggle(&mut self, cmd: &CommandTx, p: &PluginInfo) {
        if !p.enabled {
            self.enable_prompt.request(&p.id);
            return;
        }
        ipc::send(
            cmd,
            DaemonCommand::SetPluginEnabled {
                id: p.id.clone(),
                enabled: false,
            },
        );
        self.in_flight.insert(p.id.clone(), false);
    }

    pub fn show_enable_prompt(
        &mut self,
        ctx: &egui::Context,
        plugins: &[PluginInfo],
        cmd: &CommandTx,
    ) {
        if let Some(id) = self.enable_prompt.show(ctx, plugins, cmd) {
            self.in_flight.insert(id, true);
        }
    }

    fn install(&mut self, now: f64, cmd: &CommandTx, repo: &str, id: &str) {
        self.installing.insert(id.to_owned(), now);
        ipc::send(
            cmd,
            DaemonCommand::InstallPlugin {
                repo: repo.to_owned(),
                id: id.to_owned(),
            },
        );
    }

    fn uninstall_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        if self.pending_uninstall.is_none() {
            return;
        }
        if let Some(id) = widgets::confirm_delete_dialog(
            ctx,
            "uninstall_plugin",
            &t!("plugins.uninstall_title"),
            &t!(
                "plugins.uninstall_body",
                name = self.pending_uninstall.as_deref().unwrap_or_default()
            ),
            &t!("plugins.uninstall"),
            &mut self.pending_uninstall,
        ) {
            ipc::send(cmd, DaemonCommand::UninstallPlugin { id });
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, cx: &PluginsCtx, page: &mut crate::domain::state::Page) {
        reconcile_in_flight(
            &mut self.in_flight,
            &cx.state.plugins.plugins,
            matches!(
                cx.state.discovery.phase,
                halod_shared::types::DiscoveryPhase::Discovering
            ),
        );
        let now = ui.input(|i| i.time);
        clear_finished_installs(
            &mut self.installing,
            &mut self.updating_all,
            &cx.state.plugins,
            now,
        );
        if self.updating_all.is_some() || !self.installing.is_empty() {
            ui.ctx().request_repaint();
        }

        widgets::page_frame(ui, |ui| {
            let shown = cx
                .state
                .plugins
                .plugins
                .iter()
                .filter(|p| p.platform_supported);
            let total = shown.clone().count();
            let enabled = shown.filter(|p| p.enabled).count();
            let title = widgets::page_title(
                ui,
                &t!("plugins.title"),
                &t!("plugins.counts", count = total, enabled = enabled),
                |ui| {
                    widgets::search_field(ui, &mut self.search, &t!("plugins.search"), 260.0);
                },
            );
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::PluginsOverview,
                title,
            );
            self.banners(ui, cx);
            widgets::split_columns(ui, 400.0, 16.0, |side, detail| {
                self.sidebar(side, cx);
                widgets::card_frameless(detail, |detail| {
                    self.detail_column(detail, cx, page);
                });
            });
        });
    }

    // ── Left: sidebar — banners, plugin list, repositories ──────────────────

    fn sidebar(&mut self, ui: &mut egui::Ui, cx: &PluginsCtx) {
        let labels: Vec<_> = PluginFilter::ALL.iter().map(|f| f.label()).collect();
        let labels: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
        widgets::segmented(ui, &labels, &mut self.filter, true);
        ui.add_space(theme::SPACE_6);
        egui::ScrollArea::vertical()
            .id_salt("plugins_sidebar_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::card_with_margin(ui, egui::Margin::same(6), |ui| {
                    ui.set_width(ui.available_width());
                    self.plugin_list(ui, cx);
                });
                self.skipped_notice(ui, cx.state);
                ui.add_space(theme::SPACE_6);
                widgets::card_with_margin(ui, egui::Margin::symmetric(16, 14), |ui| {
                    ui.set_width(ui.available_width());
                    self.repositories(ui, cx);
                });
            });
    }

    fn banners(&mut self, ui: &mut egui::Ui, cx: &PluginsCtx) {
        let now = ui.input(|i| i.time);
        if let Some(status) = cx.udev.filter(|status| udev_rules_need_action(status)) {
            match udev_rules_banner(ui, status, self.udev_command_copied) {
                UdevBannerAction::Install => {
                    ui.ctx().copy_text(udev_install_commands());
                    self.udev_command_copied = true;
                }
                UdevBannerAction::Info => self.udev_info_open = true,
                UdevBannerAction::None => {}
            }
            ui.add_space(theme::SPACE_5);
        }
        let due = updates_due_count(&cx.state.plugins);
        if due > 0 {
            if update_all_banner(ui, due, self.updating_all.is_some()) {
                self.updating_all = Some(now);
                ipc::send(cx.cmd, DaemonCommand::UpdateAllPlugins);
            }
            ui.add_space(theme::SPACE_5);
        }
    }

    fn plugin_list(&mut self, ui: &mut egui::Ui, cx: &PluginsCtx) {
        // Platform-unsupported plugins can never activate on this host, so
        // they're hidden from the list entirely.
        let filter = PluginFilter::ALL[self.filter.min(PluginFilter::ALL.len() - 1)];
        let query = self.search.trim().to_lowercase();
        let visible: Vec<&PluginInfo> = cx
            .state
            .plugins
            .plugins
            .iter()
            .filter(|p| {
                p.platform_supported
                    && filter.admits(p.plugin_type)
                    && plugin_name(p).to_lowercase().contains(&query)
            })
            .collect();
        if visible.is_empty() {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(t!("plugins.empty_title"))
                            .font(theme::body_md())
                            .color(theme::TEXT_MUT),
                    );
                });
            return;
        }
        ui.spacing_mut().item_spacing.y = 3.0;
        for p in visible {
            let selected = self.selection == Selection::Plugin(p.id.clone());
            let update_available = update_for(p, &cx.state.plugins.available).is_some();
            let needs_action = update_available
                || crate::ui::screens::plugin_integration::awaits_setup(p)
                || cx.udev.is_some_and(|status| {
                    status.supported
                        && !status.current
                        && status.plugins_requiring_update.iter().any(|id| id == &p.id)
                });
            let logo = self.logo(p);
            match list_row(
                ui,
                p,
                selected,
                needs_action,
                update_available,
                logo.as_ref(),
                self.locked_target(p),
            ) {
                RowAction::Select => self.selection = Selection::Plugin(p.id.clone()),
                RowAction::Toggle => self.toggle(cx.cmd, p),
                RowAction::None => {}
            }
        }
    }

    fn detail_column(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PluginsCtx,
        page: &mut crate::domain::state::Page,
    ) {
        let plugin = match &self.selection {
            Selection::Plugin(id) => cx.state.plugins.plugins.iter().find(|p| &p.id == id),
            _ => None,
        };
        let repo = match &self.selection {
            Selection::Repo(slug) => cx.state.plugins.repos.iter().find(|r| &r.slug == slug),
            _ => None,
        };
        if plugin.is_none() && repo.is_none() {
            widgets::empty_state(
                ui,
                &t!("plugins.empty_title"),
                Some(&t!("plugins.empty_hint")),
            );
            return;
        }
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("plugin_detail_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                detail_pane(ui, |ui| match (plugin, repo) {
                    (Some(p), _) => self.detail_body(ui, cx, p, page),
                    (_, Some(r)) => action = self.repo_detail(ui, cx, r),
                    _ => {}
                });
            });
        if let (Some(action), Some(r)) = (action, repo) {
            self.apply_repo_action(ui.input(|i| i.time), cx.cmd, r, action);
        }
    }

    fn detail_body(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PluginsCtx,
        p: &PluginInfo,
        page: &mut crate::domain::state::Page,
    ) {
        let logo = self.logo(p);
        let locked_target = self.locked_target(p);
        let update = update_for(p, &cx.state.plugins.available);
        let now = ui.input(|i| i.time);
        let mut toggle_clicked = false;
        let setup_open = p.integration_setup.is_some();
        egui::Sides::new().show(
            ui,
            |ui| {
                match &logo {
                    Some(tex) => {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                        draw_logo_fit(ui.painter(), rect, tex);
                    }
                    None => initials_tile(ui, plugin_name(p), 48.0),
                }
                ui.add_space(theme::SPACE_3);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(plugin_name(p))
                                .font(theme::semibold(20.0))
                                .color(theme::TEXT),
                        );
                        if p.experimental {
                            widgets::tag(ui, &t!("plugins.experimental"), widgets::TagTone::Warn);
                        }
                    });
                    ui.label(
                        egui::RichText::new(detail_meta(p))
                            .font(theme::body_md())
                            .color(theme::TEXT_DIM),
                    );
                });
            },
            |ui| {
                let on = locked_target.unwrap_or(p.enabled);
                let label = if setup_open {
                    t!("integrations.cancel_setup")
                } else if on {
                    t!("plugins.disable")
                } else {
                    t!("plugins.enable")
                };
                let size = Vec2::new(90.0, 36.0);
                let resp = if locked_target.is_some() {
                    widgets::button_loading(ui, &label, ButtonKind::Secondary, size)
                } else {
                    widgets::button(ui, &label, ButtonKind::Secondary, size)
                };
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::PluginsToggle,
                    resp.rect,
                );
                toggle_clicked = resp.clicked();
            },
        );
        if toggle_clicked && setup_open {
            self.integration.cancel_setup(cx.cmd, &p.id);
        } else if toggle_clicked {
            self.toggle(cx.cmd, p);
        }

        let issue_modal = |issue| {
            Some((
                t!("plugins.issue_modal_title", plugin = plugin_name(p)).to_string(),
                plugin_issue_detail(issue),
            ))
        };
        if is_load_failed(p) {
            if let Some(issue) = &p.health.issue {
                ui.add_space(theme::SPACE_7);
                if issue_banner(ui, issue) {
                    self.issue_modal = issue_modal(issue);
                }
            }
            if let Some(update) = update {
                ui.add_space(theme::SPACE_7);
                if update_banner(
                    ui,
                    &p.version,
                    &update.version,
                    self.installing.contains_key(&p.id),
                ) {
                    self.install(now, cx.cmd, &update.repo, &p.id);
                }
            }
            return;
        }

        if matches!(
            plugin_status(p),
            PluginStatus::Inactive | PluginStatus::Degraded
        ) {
            ui.add_space(theme::SPACE_9);
            status_banner(ui, p);
        }

        if let Some(rules) = cx
            .udev
            .filter(|status| status.supported && !status.current)
            .and_then(|status| status.missing_rules_by_plugin.get(&p.id))
            .filter(|rules| !rules.is_empty())
        {
            ui.add_space(theme::SPACE_9);
            if udev_plugin_warning_banner(ui) {
                self.udev_rule_details = Some(rules.clone());
            }
        }

        if let Some(issue) = &p.health.issue {
            ui.add_space(theme::SPACE_9);
            if issue_banner(ui, issue) {
                self.issue_modal = issue_modal(issue);
            }
        }

        if let Some(update) = update {
            ui.add_space(theme::SPACE_9);
            if update_banner(
                ui,
                &p.version,
                &update.version,
                self.installing.contains_key(&p.id),
            ) {
                self.install(now, cx.cmd, &update.repo, &p.id);
            }
        }

        if !plugin_description(p).is_empty() {
            ui.add_space(theme::SPACE_9);
            ui.label(
                egui::RichText::new(plugin_description(p))
                    .font(theme::body(14.0))
                    .color(theme::TEXT_SOFT),
            );
        }

        capabilities_targets_row(ui, p);

        if !p.effect_names.is_empty() {
            ui.add_space(theme::SPACE_9);
            widgets::section_label(ui, &t!("plugins.effects"));
            ui.add_space(theme::SPACE_4);
            ui.horizontal_wrapped(|ui| {
                for name in &p.effect_names {
                    widgets::tag(ui, name, widgets::TagTone::Neutral);
                }
            });
        }

        if p.plugin_type == halod_shared::types::PluginKind::Integration {
            self.integration
                .show(ui, cx.state, p, cx.cmd, cx.serial_ports, page);
        }
        uses_section(ui, p);
        if p.provenance != PluginProvenance::Local {
            ui.add_space(theme::SPACE_9);
            if widgets::button(
                ui,
                &t!("plugins.uninstall"),
                ButtonKind::Danger,
                Vec2::new(130.0, 32.0),
            )
            .clicked()
            {
                self.pending_uninstall = Some(p.id.clone());
            }
        }
    }
}

/// What the detail column should show: keep the current selection if its
/// target still exists, otherwise fall back to the first plugin (or `None`).
fn resolve_selection(
    current: Selection,
    plugins: &[PluginInfo],
    repos: &[PluginRepoInfo],
) -> Selection {
    match &current {
        Selection::Plugin(id) if plugins.iter().any(|p| &p.id == id) => return current,
        Selection::Repo(slug) if repos.iter().any(|r| &r.slug == slug) => return current,
        _ => {}
    }
    match plugins.first() {
        Some(p) => Selection::Plugin(p.id.clone()),
        None => Selection::None,
    }
}

pub(super) fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(candidate), Ok(current)) => candidate > current,
        _ => false,
    }
}

/// A newer, compatible version of `p` in the catalog of the repo it came from.
pub(crate) fn update_for<'a>(
    p: &PluginInfo,
    available: &'a [AvailablePlugin],
) -> Option<&'a AvailablePlugin> {
    available.iter().find(|a| {
        a.repo == p.repo && a.id == p.id && a.compatible && is_newer(&a.version, &p.version)
    })
}

pub(super) fn updates_due_count(state: &PluginsState) -> usize {
    state
        .plugins
        .iter()
        .filter(|p| update_for(p, &state.available).is_some())
        .count()
}

/// An install is done once the plugin is present with no update left due.
fn clear_finished_installs(
    installing: &mut HashMap<String, f64>,
    updating_all: &mut Option<f64>,
    state: &PluginsState,
    now: f64,
) {
    let done = |id: &str| {
        state
            .plugins
            .iter()
            .any(|p| p.id == id && update_for(p, &state.available).is_none())
    };
    installing.retain(|id, started| !done(id) && now - *started < UPDATE_TIMEOUT);
    if let Some(started) = *updating_all {
        if updates_due_count(state) == 0 || now - started > UPDATE_TIMEOUT {
            *updating_all = None;
        }
    }
}

/// Decode any newly-arrived asset bytes into GPU textures, keyed like the
/// asset cache. Shared by the Plugins and Integrations screens.
pub(crate) fn decode_new_assets(
    ctx: &egui::Context,
    plugin_assets: &HashMap<String, Vec<u8>>,
    textures: &mut HashMap<String, egui::TextureHandle>,
) {
    for (key, bytes) in plugin_assets {
        if textures.contains_key(key) {
            continue;
        }
        if let Some(tex) = widgets::tex_from_bytes(ctx, bytes, key) {
            textures.insert(key.clone(), tex);
        }
    }
}

/// Paint a logo texture into `rect`, letterboxed to preserve its aspect ratio.
pub(crate) fn draw_logo_fit(painter: &egui::Painter, rect: Rect, tex: &egui::TextureHandle) {
    painter.image(
        tex.id(),
        widgets::letterbox(rect, tex.size_vec2().x / tex.size_vec2().y),
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

/// Which of `p`'s declared display assets aren't cached or already requested.
fn assets_to_request(
    p: &PluginInfo,
    cache: &HashMap<String, Vec<u8>>,
    already_requested: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut names: Vec<String> = p.logo.iter().cloned().collect();
    names.extend(p.effect_thumbnails.iter().map(|t| t.thumbnail.clone()));
    names
        .into_iter()
        .filter(|name| {
            let key = ipc::plugin_asset_cache_key(&p.id, name);
            !cache.contains_key(&key) && !already_requested.contains(&key)
        })
        .map(|name| (p.id.clone(), name))
        .collect()
}

// ── Row + detail painters ───────────────────────────────────────────────────

enum RowAction {
    None,
    Select,
    Toggle,
}

/// Persistence key marking the first-run onboarding as seen. Reuses the
/// `seen_tours` set (via `MarkTourSeen`), so recommendations need no extra config.
pub(crate) const ONBOARDING_KEY: &str = halod_shared::types::ONBOARDING_TOUR_KEY;

/// Stable dedup key for a recommendation, persisted in `seen_tours` so a given
/// (plugin, device) is announced at most once across restarts.
pub(crate) fn recommendation_key(rec: &PluginRecommendation) -> String {
    use halod_shared::types::PluginRecommendationMatch;
    match &rec.hardware {
        PluginRecommendationMatch::Always => format!("rec:{}:always", rec.plugin_id),
        PluginRecommendationMatch::Command { executable } => {
            format!("rec:{}:command:{executable}", rec.plugin_id)
        }
        PluginRecommendationMatch::Hid { vid, pid } => {
            format!("rec:{}:hid:{vid:04x}:{pid:04x}", rec.plugin_id)
        }
        PluginRecommendationMatch::Usb { vid, pid } => {
            format!("rec:{}:usb:{vid:04x}:{pid:04x}", rec.plugin_id)
        }
        PluginRecommendationMatch::SmbusGpu {
            pci_vendor,
            pci_device,
            pci_sub_vendor,
            pci_sub_device,
        } => format!(
            "rec:{}:smbus_gpu:{pci_vendor:04x}:{pci_device:04x}:{pci_sub_vendor:04x}:{pci_sub_device:04x}",
            rec.plugin_id
        ),
    }
}

/// Whether the first-run onboarding should still be shown.
pub(crate) fn onboarding_pending(seen: &std::collections::BTreeSet<String>) -> bool {
    !seen.contains(ONBOARDING_KEY)
}

/// Toasts for newly available recommendations: one per stable match identity
/// not already surfaced (persisted `seen`) and not yet shown this session
/// (`toasted`). Each toast is paired with its persistence key so the caller can
/// `MarkTourSeen` it.
pub(crate) fn recommendation_toasts(
    recommendations: &[PluginRecommendation],
    seen: &std::collections::BTreeSet<String>,
    toasted: &mut HashSet<String>,
    timestamp_ms: u64,
) -> Vec<(String, halod_shared::types::Notification)> {
    use halod_shared::types::{Notification, NotificationCode};
    recommendations
        .iter()
        .filter_map(|rec| {
            let key = recommendation_key(rec);
            if seen.contains(&key) || !toasted.insert(key.clone()) {
                return None;
            }
            Some((
                key,
                Notification {
                    code: NotificationCode::PluginRecommended {
                        plugin: rec.plugin_name.clone(),
                    },
                    show_native: true,
                    timestamp_ms,
                },
            ))
        })
        .collect()
}

/// A toggle has landed once the plugin is at the target and the re-probe scan
/// has finished — so the toggle stays locked for the whole
/// (multi-second) device scan, not just until the config flag flips.
fn plugin_toggle_landed(p: &PluginInfo, target: bool, discovering: bool) -> bool {
    p.enabled == target && !discovering
}

/// Drop landed (or vanished) in-flight toggles, unlocking them. Pure/testable.
fn reconcile_in_flight(
    in_flight: &mut HashMap<String, bool>,
    plugins: &[PluginInfo],
    discovering: bool,
) {
    crate::domain::state::retain_in_flight(in_flight, plugins, |p, target| {
        plugin_toggle_landed(p, target, discovering)
    });
}

/// Effective status shown to the user: distinct from the toggle position, which
/// reflects intent (`p.enabled`). Requirement gating and degrade rows come from
/// the daemon (`p.active` / `p.activation_blocker` / `p.requirements`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginStatus {
    /// Running normally.
    Active,
    /// Running, but a degrading requirement is unmet (a feature is off).
    Degraded,
    /// Enabled by the user but held inactive by a missing blocking requirement.
    Inactive,
    /// Off — disabled or unsupported.
    Disabled,
}

fn plugin_status(p: &PluginInfo) -> PluginStatus {
    if p.activation_blocker.is_some() {
        PluginStatus::Inactive
    } else if p.active {
        let degraded = p
            .requirements
            .iter()
            .any(|r| !r.satisfied && r.impact == RequirementImpact::Degrade);
        if degraded {
            PluginStatus::Degraded
        } else {
            PluginStatus::Active
        }
    } else {
        PluginStatus::Disabled
    }
}

/// Dot colour, label, and text colour for a plugin's effective status.
fn status_visual(status: PluginStatus) -> (egui::Color32, String, egui::Color32) {
    match status {
        PluginStatus::Active => (
            theme::ONLINE,
            t!("plugins.status_active").to_string(),
            theme::ONLINE_TEXT,
        ),
        PluginStatus::Degraded => (
            theme::STAT_AMBER,
            t!("plugins.status_degraded").to_string(),
            theme::STAT_AMBER,
        ),
        PluginStatus::Inactive => (
            theme::OFFLINE,
            t!("plugins.status_inactive").to_string(),
            theme::OFFLINE,
        ),
        PluginStatus::Disabled => (
            theme::TEXT_FAINT2,
            t!("plugins.status_disabled").to_string(),
            theme::TEXT_MUT,
        ),
    }
}

/// One line of user-facing remediation for an unmet requirement. Structured
/// reason codes from the daemon map to localized copy here (the UI owns text).
fn requirement_line(s: &PluginRequirementStatus) -> String {
    match &s.requirement {
        PluginRequirement::Command { executable } => {
            t!("plugins.req_command_missing", name = executable).to_string()
        }
        PluginRequirement::KernelModule { name } => {
            match s.reason {
                // Available but not loaded → an actionable modprobe hint.
                Some(halod_shared::types::RequirementFailureReason::NotLoaded) => {
                    t!("plugins.req_module_not_loaded", name = name).to_string()
                }
                _ => t!("plugins.req_module_unavailable", name = name).to_string(),
            }
        }
        PluginRequirement::PawnIo => t!("plugins.req_pawnio_missing").to_string(),
        PluginRequirement::LinuxI2c => t!("plugins.req_i2c_missing").to_string(),
        PluginRequirement::LinuxHwmon { access } => match (access.as_str(), s.reason) {
            ("pwm", Some(halod_shared::types::RequirementFailureReason::PermissionDenied)) => {
                t!("plugins.req_hwmon_pwm_permission").to_string()
            }
            (_, Some(halod_shared::types::RequirementFailureReason::PermissionDenied)) => {
                t!("plugins.req_hwmon_read_permission").to_string()
            }
            _ => t!("plugins.req_hwmon_missing").to_string(),
        },
    }
}

fn list_row(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    selected: bool,
    needs_action: bool,
    update_available: bool,
    logo_tex: Option<&egui::TextureHandle>,
    // `Some(target)` while applying: toggle shown at `target`, dimmed, click-blocked.
    locked_target: Option<bool>,
) -> RowAction {
    let (rect, resp) = widgets::hover_row(ui, 54.0, selected);
    let center_y = rect.center().y;

    let tile_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, center_y - 17.0),
        Vec2::splat(34.0),
    );
    match logo_tex {
        Some(tex) => draw_logo_fit(ui.painter(), tile_rect, tex),
        None => initials_tile_at(ui, tile_rect, plugin_name(p)),
    }

    let text_x = tile_rect.right() + 12.0;
    ui.painter().text(
        Pos2::new(text_x, center_y + 1.0),
        Align2::LEFT_BOTTOM,
        compact_plugin_name(plugin_name(p)),
        theme::medium(14.0),
        theme::TEXT,
    );
    let (note, note_color) = if update_available {
        (
            t!("plugins.update_available").into_owned(),
            theme::WARN_TEXT,
        )
    } else if p.experimental {
        (t!("plugins.experimental").into_owned(), theme::WARN_TEXT)
    } else if crate::ui::screens::plugin_integration::awaits_setup(p) {
        (t!("plugins.not_set_up").into_owned(), theme::WARN_TEXT)
    } else if p.plugin_type == halod_shared::types::PluginKind::Integration
        && !p.integration_configured
    {
        (t!("plugins.not_set_up").into_owned(), theme::TEXT_MUT)
    } else {
        (p.version.clone(), theme::TEXT_MUT)
    };
    let kind = ui.painter().text(
        Pos2::new(text_x, center_y + 2.0),
        Align2::LEFT_TOP,
        format!("{} · ", plugin_type_label(p.plugin_type)),
        theme::body_sm(),
        note_color,
    );
    ui.painter().text(
        Pos2::new(kind.right(), center_y + 2.0),
        Align2::LEFT_TOP,
        note,
        theme::body_sm(),
        note_color,
    );
    if needs_action {
        ui.painter().circle_filled(
            Pos2::new(tile_rect.right() - 2.0, tile_rect.top() + 2.0),
            4.0,
            theme::STAT_AMBER,
        );
    }
    if p.health.issue.is_some() {
        ui.painter().circle_filled(
            Pos2::new(tile_rect.left() + 2.0, tile_rect.top() + 2.0),
            4.0,
            theme::TRAFFIC_RED,
        );
    }

    let toggle_rect = Rect::from_min_size(
        Pos2::new(
            rect.right() - 10.0 - widgets::TOGGLE_SIZE.x,
            center_y - widgets::TOGGLE_SIZE.y / 2.0,
        ),
        widgets::TOGGLE_SIZE,
    );
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::PluginsToggle,
        toggle_rect,
    );
    let toggle_on = locked_target.unwrap_or(p.enabled);
    if widgets::toggle_at(
        ui,
        toggle_rect,
        ui.scope_id().with(("plugin_toggle", &p.id)),
        toggle_on,
        locked_target.is_some(),
    )
    .clicked()
    {
        RowAction::Toggle
    } else if resp.clicked() {
        RowAction::Select
    } else {
        RowAction::None
    }
}

fn compact_plugin_name(name: &str) -> String {
    const MAX_CHARS: usize = 25;
    let mut chars = name.chars();
    let compact: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{compact}...")
    } else {
        compact
    }
}

/// Up to 2 uppercase initials from `name` (first letter of the first two
/// words), falling back to "?" for an empty name.
fn initials_for(name: &str) -> String {
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(|c| c.to_uppercase())
        .collect();
    if initials.is_empty() {
        "?".to_owned()
    } else {
        initials
    }
}

/// An initials tile at an already-allocated `rect`.
pub(crate) fn initials_tile_at(ui: &mut egui::Ui, rect: Rect, name: &str) {
    ui.painter().rect_filled(rect, 8.0, theme::TILE_BG);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initials_for(name),
        theme::semibold(rect.height() * 0.34),
        theme::TEXT_DIM,
    );
}

/// An initials tile that allocates its own `size`×`size` space.
fn initials_tile(ui: &mut egui::Ui, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    initials_tile_at(ui, rect, name);
}

/// Edge-to-edge padding for the detail pane. The detail view fills the page
/// rather than sitting in a card, so this supplies the inset the card's inner
/// margin used to provide.
fn detail_pane<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 24,
            right: 24,
            top: 24,
            bottom: 24,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

fn detail_meta(p: &PluginInfo) -> String {
    let mut parts = vec![plugin_type_label(p.plugin_type).into_owned()];
    if !p.version.is_empty() {
        parts.push(p.version.clone());
    }
    parts.push(provenance_label(p.provenance).into_owned());
    parts.join(" · ")
}

fn plugin_type_label(kind: halod_shared::types::PluginKind) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::PluginKind;
    match kind {
        PluginKind::Device => t!("plugins.type_device"),
        PluginKind::Effect => t!("plugins.type_effect"),
        PluginKind::Integration => t!("plugins.type_integration"),
        PluginKind::Lcd => std::borrow::Cow::Borrowed("LCD"),
    }
}

fn provenance_label(provenance: PluginProvenance) -> std::borrow::Cow<'static, str> {
    match provenance {
        PluginProvenance::Official => t!("plugins.provenance_official"),
        PluginProvenance::Signed => t!("plugins.provenance_signed"),
        PluginProvenance::Unsigned => t!("plugins.provenance_repository"),
        PluginProvenance::Local => t!("plugins.provenance_local"),
    }
}

fn status_banner(ui: &mut egui::Ui, p: &PluginInfo) {
    let (dot, text, color) = status_visual(plugin_status(p));
    // Every unmet requirement (blocking or degrading) is worth surfacing so the
    // user can act; the pill above already signals which kind.
    let unmet: Vec<&PluginRequirementStatus> =
        p.requirements.iter().filter(|r| !r.satisfied).collect();
    widgets::Banner::neutral(&text)
        .title_color(color)
        .title_font(theme::body_md())
        .dot(dot)
        .show_with(ui, |ui| {
            for req in unmet {
                ui.add_space(theme::SPACE_2);
                ui.label(
                    egui::RichText::new(requirement_line(req))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            }
        });
}

fn updates_available_key(count: usize) -> &'static str {
    if count == 1 {
        "plugins.updates_available"
    } else {
        "plugins.updates_available_plural"
    }
}

fn update_all_banner(ui: &mut egui::Ui, count: usize, updating: bool) -> bool {
    let title = t!(updates_available_key(count), count = count).to_string();
    let label = if updating {
        t!("plugins.updating")
    } else {
        t!("plugins.update_all")
    };
    widgets::Banner::warn(&title)
        .title_color(theme::TEXT)
        .title_font(theme::body_md())
        .dot(theme::STAT_AMBER)
        .action(
            widgets::BannerAction::new(&label, ButtonKind::Warn, Vec2::new(130.0, 30.0))
                .loading(updating),
        )
        .action_below()
        .show(ui)
}

/// The i18n label key for a plugin issue banner, by kind.
fn issue_label_key(kind: &PluginIssueKind) -> &'static str {
    match kind {
        PluginIssueKind::ConnectFailed => "plugins.issue_connect_failed",
        PluginIssueKind::InitFailed => "plugins.issue_init_failed",
        PluginIssueKind::RuntimeError => "plugins.issue_runtime_error",
        PluginIssueKind::LoadWarning => "plugins.issue_load_warning",
        PluginIssueKind::LoadFailed => "plugins.issue_load_failed",
    }
}

fn is_load_failed(p: &PluginInfo) -> bool {
    p.health
        .issue
        .as_ref()
        .is_some_and(|i| i.kind == PluginIssueKind::LoadFailed)
}

/// A per-plugin issue banner with a "Details" button; returns `true` when
/// Details is clicked. Load warnings tint amber, connect/runtime errors red.
fn issue_banner(ui: &mut egui::Ui, issue: &PluginIssue) -> bool {
    let title = t!(issue_label_key(&issue.kind));
    let label = t!("plugins.issue_details");
    let banner = if issue.kind == PluginIssueKind::LoadWarning {
        widgets::Banner::warn(title.as_ref())
    } else {
        widgets::Banner::danger(title.as_ref())
    };
    banner
        .action(widgets::BannerAction::new(
            &label,
            ButtonKind::Secondary,
            Vec2::new(90.0, 30.0),
        ))
        .show(ui)
}

fn update_banner(ui: &mut egui::Ui, current: &str, available: &str, updating: bool) -> bool {
    let title = t!("plugins.update_available");
    let label = if updating {
        t!("plugins.updating")
    } else {
        t!("plugins.update")
    };
    widgets::Banner::warn(title.as_ref())
        .title_color(theme::TEXT)
        .title_font(theme::body_md())
        .action(
            widgets::BannerAction::new(&label, ButtonKind::Warn, Vec2::new(90.0, 30.0))
                .loading(updating),
        )
        .show_action_with(ui, |ui| {
            if !current.is_empty() || !available.is_empty() {
                ui.label(
                    egui::RichText::new(format!("{current} → {available}"))
                        .font(theme::value_sm())
                        .color(theme::STAT_AMBER),
                );
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plugin_kind_falls_under_all_and_exactly_one_type_filter() {
        use halod_shared::types::PluginKind;
        for kind in [
            PluginKind::Device,
            PluginKind::Effect,
            PluginKind::Integration,
            PluginKind::Lcd,
        ] {
            assert!(PluginFilter::All.admits(kind));
            let typed = PluginFilter::ALL[1..]
                .iter()
                .filter(|f| f.admits(kind))
                .count();
            assert_eq!(typed, 1, "{kind:?}");
        }
    }

    pub(super) fn available(
        repo: &str,
        id: &str,
        version: &str,
        compatible: bool,
    ) -> AvailablePlugin {
        AvailablePlugin {
            repo: repo.into(),
            id: id.into(),
            version: version.into(),
            api: "3.0".into(),
            compatible,
        }
    }

    fn installed(id: &str, version: &str) -> PluginInfo {
        let mut p = info(id, true);
        p.repo = "official".into();
        p.version = version.into();
        p
    }

    #[test]
    fn updates_available_key_picks_singular_only_for_one() {
        assert_eq!(updates_available_key(1), "plugins.updates_available");
        assert_eq!(updates_available_key(0), "plugins.updates_available_plural");
        assert_eq!(updates_available_key(9), "plugins.updates_available_plural");
    }

    #[test]
    fn updates_are_newer_compatible_versions_from_the_same_repo() {
        let p = installed("kraken", "1.0.0");
        let catalog = [
            available("other", "kraken", "9.0.0", true),
            available("official", "kraken", "2.0.0", false),
        ];
        assert!(update_for(&p, &catalog).is_none());
        let catalog = [available("official", "kraken", "1.0.1", true)];
        assert_eq!(
            update_for(&p, &catalog).map(|a| a.version.as_str()),
            Some("1.0.1")
        );
        assert!(update_for(&installed("kraken", "1.0.1"), &catalog).is_none());
        assert!(!is_newer("1.0.0", "not-semver"));
    }

    #[test]
    fn installs_finish_when_the_version_lands_or_time_runs_out() {
        let mut state = PluginsState {
            plugins: vec![installed("kraken", "1.0.0")],
            available: vec![available("official", "kraken", "1.1.0", true)],
            ..Default::default()
        };
        let mut installing = HashMap::from([("kraken".to_owned(), 0.0), ("new".to_owned(), 0.0)]);
        let mut updating_all = Some(0.0);

        clear_finished_installs(&mut installing, &mut updating_all, &state, 1.0);
        assert_eq!(installing.len(), 2);
        assert!(updating_all.is_some());

        state.plugins[0].version = "1.1.0".into();
        clear_finished_installs(&mut installing, &mut updating_all, &state, 2.0);
        assert_eq!(installing.keys().collect::<Vec<_>>(), ["new"]);
        assert!(updating_all.is_none());

        clear_finished_installs(
            &mut installing,
            &mut updating_all,
            &state,
            UPDATE_TIMEOUT + 1.0,
        );
        assert!(installing.is_empty());
    }

    #[test]
    fn command_error_notification_stops_install_progress_immediately() {
        let mut screen = PluginsUi {
            installing: HashMap::from([("plugin-a".to_owned(), 1.0)]),
            updating_all: Some(1.0),
            ..Default::default()
        };
        screen.observe_notifications(&[halod_shared::types::Notification {
            code: halod_shared::types::NotificationCode::Generic {
                message: "install failed".to_owned(),
            },
            show_native: false,
            timestamp_ms: 0,
        }]);
        assert!(screen.installing.is_empty());
        assert!(screen.updating_all.is_none());
    }

    fn queued(screen: &mut PluginsUi, plugins: &[PluginInfo]) -> Option<String> {
        screen
            .enable_prompt
            .current(plugins)
            .map(|plugin| plugin.id.clone())
    }

    #[test]
    fn confirming_the_prompt_sends_exactly_one_enable() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut screen = PluginsUi::default();
        let plugins = [info("acme", false)];
        screen.toggle(&cmd, &plugins[0]);
        assert!(rx.try_recv().is_err());
        assert_eq!(queued(&mut screen, &plugins).as_deref(), Some("acme"));

        assert!(screen.enable_prompt.resolve(&cmd, "acme", true));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::SetPluginEnabled { id, enabled: true }) if id == "acme"
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(queued(&mut screen, &plugins), None);
    }

    #[test]
    fn declining_sends_nothing_and_asks_again_next_time() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut screen = PluginsUi::default();
        let plugins = [info("acme", false)];
        screen.toggle(&cmd, &plugins[0]);

        assert!(!screen.enable_prompt.resolve(&cmd, "acme", false));
        assert!(rx.try_recv().is_err());
        assert_eq!(screen.locked_target(&plugins[0]), None);
        assert_eq!(queued(&mut screen, &plugins), None);

        screen.toggle(&cmd, &plugins[0]);
        assert_eq!(queued(&mut screen, &plugins).as_deref(), Some("acme"));
    }

    #[test]
    fn disabling_never_prompts() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut screen = PluginsUi::default();
        let plugins = [info("acme", true)];
        screen.toggle(&cmd, &plugins[0]);
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::SetPluginEnabled { id, enabled: false }) if id == "acme"
        ));
        assert_eq!(queued(&mut screen, &plugins), None);
    }

    pub(super) fn info(id: &str, enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} device"),
            translations: Default::default(),
            path: format!("/home/u/.config/halod/plugins/{id}.lua"),
            plugin_type: halod_shared::types::PluginKind::Device,
            experimental: false,
            capabilities: vec!["RGB".into()],
            platforms: vec![],
            platform_supported: true,
            effect_names: vec![],
            enabled,
            author: "Someone".into(),
            version: "1.0.0".into(),
            description: "desc".into(),
            targets: vec!["Acme K1".into()],
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
            integration_enabled: true,
            integration_configured: true,
            integration_requires_setup: false,
            integration_state: halod_shared::types::IntegrationLifecycleState::Configured,
            integration_setup: None,
            active: enabled,
            requirements: vec![],
            activation_blocker: None,
            health: Default::default(),
        }
    }

    #[test]
    fn toggle_lands_only_when_matched_and_scan_done() {
        let on = info("p", true);
        let off = info("p", false);
        assert!(plugin_toggle_landed(&off, false, false));
        assert!(!plugin_toggle_landed(&on, false, false), "still active");
        assert!(
            !plugin_toggle_landed(&off, false, true),
            "scan still running"
        );
        assert!(plugin_toggle_landed(&on, true, false));
    }

    #[test]
    fn reconcile_unlocks_landed_and_vanished_toggles() {
        let mut in_flight = HashMap::from([
            ("keep".to_string(), false), // still applying
            ("done".to_string(), true),  // landed
            ("gone".to_string(), true),  // plugin disappeared
        ]);
        let plugins = vec![info("keep", true), info("done", true)];

        reconcile_in_flight(&mut in_flight, &plugins, false);

        assert!(in_flight.contains_key("keep"), "disable not applied yet");
        assert!(!in_flight.contains_key("done"), "enable landed → unlocked");
        assert!(!in_flight.contains_key("gone"), "vanished → unlocked");
    }

    #[test]
    fn reconcile_keeps_all_locked_while_scanning() {
        let plugins = vec![info("done", true)];
        let mut in_flight = HashMap::from([("done".to_string(), true)]);
        reconcile_in_flight(&mut in_flight, &plugins, true);
        assert!(in_flight.contains_key("done"), "scanning → locked");
    }

    #[test]
    fn recommendation_toasts_fire_once_and_respect_persisted_seen() {
        use std::collections::BTreeSet;
        let rec = PluginRecommendation {
            plugin_id: "nzxt_kraken".into(),
            plugin_name: "NZXT Kraken".into(),
            hardware: halod_shared::types::PluginRecommendationMatch::Hid {
                vid: 0x1e71,
                pid: 0x2007,
            },
            accessible: true,
        };
        let key = recommendation_key(&rec);

        // First surfacing this session fires a toast paired with its key.
        let mut toasted = HashSet::new();
        let seen = BTreeSet::new();
        let out = recommendation_toasts(std::slice::from_ref(&rec), &seen, &mut toasted, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, key);

        // Same session → already toasted, no repeat.
        assert!(
            recommendation_toasts(std::slice::from_ref(&rec), &seen, &mut toasted, 0).is_empty()
        );

        // Persisted seen from a previous run → never toasts again, fresh session.
        let mut fresh = HashSet::new();
        let seen: BTreeSet<String> = [key].into_iter().collect();
        assert!(recommendation_toasts(&[rec], &seen, &mut fresh, 0).is_empty());
    }

    #[test]
    fn onboarding_pending_until_marked_seen() {
        use std::collections::BTreeSet;
        assert!(onboarding_pending(&BTreeSet::new()));
        let seen: BTreeSet<String> = [ONBOARDING_KEY.to_string()].into_iter().collect();
        assert!(!onboarding_pending(&seen));
    }

    #[test]
    fn issue_label_key_maps_each_kind() {
        assert_eq!(
            issue_label_key(&PluginIssueKind::ConnectFailed),
            "plugins.issue_connect_failed"
        );
        assert_eq!(
            issue_label_key(&PluginIssueKind::InitFailed),
            "plugins.issue_init_failed"
        );
        assert_eq!(
            issue_label_key(&PluginIssueKind::RuntimeError),
            "plugins.issue_runtime_error"
        );
        assert_eq!(
            issue_label_key(&PluginIssueKind::LoadWarning),
            "plugins.issue_load_warning"
        );
        assert_eq!(
            issue_label_key(&PluginIssueKind::LoadFailed),
            "plugins.issue_load_failed"
        );
    }

    #[test]
    fn is_load_failed_only_for_load_failed_issue() {
        let mut p = info("p", true);
        assert!(!is_load_failed(&p));
        p.health.issue = Some(PluginIssue {
            kind: PluginIssueKind::RuntimeError,
            detail: "x".into(),
            context: None,
            timestamp_ms: 0,
        });
        assert!(!is_load_failed(&p));
        p.health.issue = Some(PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "x".into(),
            context: None,
            timestamp_ms: 0,
        });
        assert!(is_load_failed(&p));
    }

    #[test]
    fn assets_to_request_lists_undeclared_logo_and_thumbnails() {
        let mut p = info("a", true);
        p.logo = Some("logo.png".into());
        p.effect_thumbnails = vec![halod_shared::types::PluginEffectAsset {
            id: "rainbow".into(),
            thumbnail: "rainbow.png".into(),
        }];
        let reqs = assets_to_request(&p, &HashMap::new(), &HashSet::new());
        assert_eq!(
            reqs,
            vec![
                ("a".to_owned(), "logo.png".to_owned()),
                ("a".to_owned(), "rainbow.png".to_owned()),
            ]
        );
    }

    #[test]
    fn assets_to_request_skips_cached_and_already_requested() {
        let mut p = info("a", true);
        p.logo = Some("logo.png".into());
        p.effect_thumbnails = vec![halod_shared::types::PluginEffectAsset {
            id: "rainbow".into(),
            thumbnail: "rainbow.png".into(),
        }];
        let mut cache = HashMap::new();
        cache.insert(ipc::plugin_asset_cache_key("a", "logo.png"), vec![1, 2, 3]);
        let mut requested = HashSet::new();
        requested.insert(ipc::plugin_asset_cache_key("a", "rainbow.png"));

        assert!(assets_to_request(&p, &cache, &requested).is_empty());
    }

    #[test]
    fn assets_to_request_empty_when_plugin_declares_no_assets() {
        let p = info("a", true);
        assert!(assets_to_request(&p, &HashMap::new(), &HashSet::new()).is_empty());
    }

    pub(super) fn repo(slug: &str) -> PluginRepoInfo {
        PluginRepoInfo {
            slug: slug.to_owned(),
            location: PluginRepoLocation::Url(format!("https://example.com/{slug}/catalog.yaml")),
            official: false,
            trust: RepoTrust::Unsigned,
            fingerprint: None,
            last_sync: None,
            commit: None,
        }
    }

    #[test]
    fn initials_for_takes_first_letter_of_first_two_words() {
        assert_eq!(initials_for("WLED UDP"), "WU");
        assert_eq!(initials_for("kraken"), "K");
        assert_eq!(initials_for(""), "?");
        assert_eq!(initials_for("  "), "?");
    }

    #[test]
    fn selection_keeps_valid_current() {
        let plugins = vec![info("a", true), info("b", false)];
        assert_eq!(
            resolve_selection(Selection::Plugin("b".into()), &plugins, &[]),
            Selection::Plugin("b".into())
        );
    }

    #[test]
    fn selection_falls_back_to_first_when_missing_or_none() {
        let plugins = vec![info("a", true), info("b", false)];
        assert_eq!(
            resolve_selection(Selection::Plugin("gone".into()), &plugins, &[]),
            Selection::Plugin("a".into())
        );
        assert_eq!(
            resolve_selection(Selection::None, &plugins, &[]),
            Selection::Plugin("a".into())
        );
    }

    #[test]
    fn selection_is_none_for_empty_list() {
        assert_eq!(
            resolve_selection(Selection::Plugin("a".into()), &[], &[]),
            Selection::None
        );
        assert_eq!(
            resolve_selection(Selection::None, &[], &[]),
            Selection::None
        );
    }

    #[test]
    fn selection_keeps_a_valid_repo_selection() {
        let plugins = vec![info("a", true)];
        let repos = vec![repo("foo")];
        assert_eq!(
            resolve_selection(Selection::Repo("foo".into()), &plugins, &repos),
            Selection::Repo("foo".into())
        );
    }

    #[test]
    fn selection_falls_back_when_the_selected_repo_is_gone() {
        let plugins = vec![info("a", true)];
        assert_eq!(
            resolve_selection(Selection::Repo("gone".into()), &plugins, &[]),
            Selection::Plugin("a".into())
        );
    }

    #[test]
    fn status_dot_reflects_enabled() {
        let dot = |p: &PluginInfo| status_visual(plugin_status(p)).0;
        assert_eq!(dot(&info("a", true)), theme::ONLINE);
        assert_eq!(dot(&info("a", false)), theme::TEXT_FAINT2);
    }

    #[test]
    fn plugin_status_classifies_effective_state() {
        use halod_shared::types::{
            PluginActivationBlocker, PluginRequirement, PluginRequirementStatus, RequirementImpact,
        };

        assert_eq!(plugin_status(&info("a", true)), PluginStatus::Active);

        let mut disabled = info("d", false);
        disabled.active = false;
        assert_eq!(plugin_status(&disabled), PluginStatus::Disabled);

        // Enabled by intent but blocked → inactive; the toggle still reads on.
        let mut blocked = info("b", true);
        blocked.active = false;
        let missing = PluginRequirementStatus {
            requirement: PluginRequirement::Command {
                executable: "liquidctl".into(),
            },
            impact: RequirementImpact::Block,
            satisfied: false,
            reason: None,
            feature: None,
        };
        blocked.activation_blocker = Some(PluginActivationBlocker::MissingRequirements {
            requirements: vec![missing.clone()],
        });
        assert_eq!(plugin_status(&blocked), PluginStatus::Inactive);
        assert_eq!(status_visual(plugin_status(&blocked)).0, theme::OFFLINE);
        assert!(blocked.enabled, "a blocked plugin's toggle stays on");
        assert!(requirement_line(&missing).contains("liquidctl"));

        // Active but a degrading requirement is unmet → limited functionality.
        let mut degraded = info("g", true);
        degraded.requirements = vec![PluginRequirementStatus {
            requirement: PluginRequirement::Command {
                executable: "pactl".into(),
            },
            impact: RequirementImpact::Degrade,
            satisfied: false,
            reason: None,
            feature: Some("audio_routing".into()),
        }];
        assert_eq!(plugin_status(&degraded), PluginStatus::Degraded);
    }

    #[test]
    fn plugin_type_label_and_color_distinguish_device_and_effect() {
        use halod_shared::types::PluginKind;
        assert_eq!(
            plugin_type_label(PluginKind::Device),
            t!("plugins.type_device")
        );
        assert_eq!(
            plugin_type_label(PluginKind::Effect),
            t!("plugins.type_effect")
        );
    }
}

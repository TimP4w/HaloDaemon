// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugins page — a master–detail view of canonical plugin packages. The left
//! column lists every plugin with an enable toggle; the right column shows the
//! manifest catalog, authority, and provenance. Every package is supplied by
//! a registered remote, local Git, or archive repository.

use std::collections::{HashMap, HashSet};

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    PluginDownloadConsent, PluginInfo, PluginIssue, PluginIssueKind, PluginProvenance,
    PluginRecommendation, PluginRepoInfo, PluginRepoLocation, PluginRequirement,
    PluginRequirementStatus, PluginSource, PluginUpdateStatus, RepoSignatureStatus,
    RepoUpdateStatus, RequirementImpact,
};

use crate::domain::models::plugin_issues::plugin_issue_detail;
use crate::domain::models::udev::udev_rules_need_action;
use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::icons;
use crate::ui::theme;
use halod_shared::commands::DaemonCommand;

mod authority;
mod repo;
mod udev;

use authority::*;
pub(crate) use authority::{
    authority_review_cards, plugin_needs_permission, plugin_requires_regrant,
};
use repo::*;
use udev::*;

pub(crate) fn plugin_name(plugin: &PluginInfo) -> &str {
    plugin.localized_name(&rust_i18n::locale())
}

fn plugin_description(plugin: &PluginInfo) -> &str {
    plugin.localized_description(&rust_i18n::locale())
}

/// In-progress state of the "Add repository" modal.
#[derive(Default)]
struct AddRepoState {
    url: String,
}

/// Failsafe: drop the "checking for updates" spinner after this long even if
/// the check never lands (e.g. an unreachable remote).
const REPO_CHECK_TIMEOUT: f64 = 25.0;

/// A repo whose update check is in flight — the spinner shows until its
/// `last_sync` advances past `prev_sync` (the check landed) or it times out.
struct RepoCheck {
    slug: String,
    prev_sync: Option<String>,
    started: f64,
}

/// A malformed repo package selected for a scoped remote restore.
#[derive(Clone)]
struct PendingRepoRepair {
    slug: String,
}

/// What the detail column shows: a plugin, a repo, or nothing (empty state).
#[derive(Default, Clone, PartialEq, Eq, Debug)]
enum Selection {
    Plugin(String),
    Repo(String),
    #[default]
    None,
}

/// Local UI state for the Plugins screen (selection + open dialogs).
#[derive(Default)]
pub struct PluginsUi {
    /// What the detail column shows.
    selection: Selection,
    /// The add-repository modal, when open.
    add_repo: Option<AddRepoState>,
    /// Repo slug pending a remove confirmation, when the dialog is open.
    pending_repo_delete: Option<String>,
    /// Repository pending a whole-repository recovery confirmation.
    pending_repo_repair: Option<PendingRepoRepair>,
    /// The repo whose "check for updates" is currently in flight, if any.
    checking_repo: Option<RepoCheck>,
    /// Id pending a permission consent decision, when the dialog is open.
    pending_consent: Option<String>,
    /// Plugin ids whose enable/disable is applying → target state; the toggle is
    /// locked at the target until the daemon confirms.
    in_flight: HashMap<String, bool>,
    /// Asset cache keys already requested, so a pending fetch isn't re-sent.
    requested_assets: HashSet<String>,
    requested_releases: HashSet<String>,
    /// Decoded asset bytes turned into textures, keyed like `requested_assets`.
    asset_textures: HashMap<String, egui::TextureHandle>,
    /// Plugin ids whose single-plugin update is in flight → egui time it began.
    /// The button shows a spinner until the plugin drops its "update available"
    /// flag (the daemon re-broadcasts once the checkout lands) or it times out.
    updating: HashMap<String, f64>,
    /// "Update all" in flight → egui time it began, cleared the same way.
    updating_all: Option<f64>,
    /// Repositories whose whole-repo update is in flight → egui start time.
    /// Entries are reconciled centrally so progress ends even when another
    /// detail pane is selected. "Update all" inserts every affected repo.
    updating_repos: HashMap<String, f64>,
    /// The open plugin-issue Details modal (title + detail), when the user
    /// clicked Details on a plugin's issue banner.
    issue_modal: Option<(String, String)>,
    /// Exact udev lines missing for the selected plugin.
    udev_rule_details: Option<Vec<String>>,
    #[cfg(target_os = "linux")]
    next_udev_status_check: f64,
    udev_command_copied: bool,
    udev_info_open: bool,
    /// egui time a udev "Recheck rules" began, driving the button's spinner; the
    /// spinner runs for [`UDEV_RECHECK_SPINNER`] to acknowledge the request.
    udev_rechecking: Option<f64>,
    /// Index into [`PluginFilter::ALL`] for the list's type filter.
    filter: usize,
    search: String,
    integration: crate::ui::screens::plugin_integration::IntegrationPanel,
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

/// Failsafe: drop an update spinner after this long even if the daemon never
/// re-broadcasts (e.g. an unreachable remote mid-update).
const UPDATE_TIMEOUT: f64 = 90.0;

/// Per-frame inputs of the Plugins page that live outside the topic store.
pub struct PluginsCtx<'a> {
    pub state: &'a TopicStore,
    pub cmd: &'a CommandTx,
    pub assets: &'a HashMap<String, Vec<u8>>,
    pub releases: &'a HashMap<String, Vec<halod_shared::types::PluginReleaseInfo>>,
    pub udev: Option<&'a halod_shared::types::UdevRulesStatus>,
    pub serial_ports: &'a [halod_shared::types::SerialPortInfo],
}

impl PluginsCtx<'_> {
    fn repo_updates(&self) -> &[RepoUpdateStatus] {
        &self.state.plugins.repo_updates
    }

    fn plugin_updates(&self) -> &[PluginUpdateStatus] {
        &self.state.plugins.updates
    }
}

impl PluginsUi {
    /// Stop optimistic update progress as soon as the daemon reports that the
    /// command failed. The same notification is rendered by the root toast
    /// layer, so the user sees the reason instead of a spinner lasting until
    /// the failsafe timeout.
    pub fn observe_notifications(&mut self, notifications: &[halod_shared::types::Notification]) {
        if notifications.iter().any(|notification| {
            matches!(
                notification.code,
                halod_shared::types::NotificationCode::Generic { .. }
            )
        }) {
            self.updating.clear();
            self.updating_all = None;
            self.updating_repos.clear();
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
        self.repo_repair_modal(ui.ctx(), cx.cmd);
        self.consent_modal(ui.ctx(), cx.state, cx.cmd);
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
        let out = request_toggle(cmd, p, self.pending_consent.take());
        self.pending_consent = out.pending_consent;
        if let Some(target) = out.dispatched {
            apply_and_lock(&mut self.in_flight, &p.id, target);
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
        clear_finished_updates(
            &mut self.updating,
            &mut self.updating_all,
            cx.plugin_updates(),
            now,
        );
        clear_finished_repo_updates(
            &mut self.updating_repos,
            cx.repo_updates(),
            cx.plugin_updates(),
            now,
        );
        if self.updating_all.is_some()
            || !self.updating.is_empty()
            || !self.updating_repos.is_empty()
        {
            // Keep the timeout advancing and the spinner animating.
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
            let enabled = shown.filter(|p| plugin_enabled_with_consent(p)).count();
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
        if let Some(problem) = repository_integrity_problem(cx.state) {
            if repository_integrity_banner(ui, &problem) {
                if problem.repairable {
                    self.pending_repo_repair = Some(PendingRepoRepair { slug: problem.slug });
                } else {
                    self.pending_repo_delete = Some(problem.slug);
                }
            }
            ui.add_space(theme::SPACE_5);
        }
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
        let due = updates_due_count(cx.repo_updates(), cx.plugin_updates());
        if due > 0 {
            if update_all_banner(ui, due, self.updating_all.is_some()) {
                self.updating_all = Some(now);
                for slug in update_all_seed_slugs(cx.repo_updates(), cx.plugin_updates()) {
                    self.updating_repos.insert(slug, now);
                }
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
            ui.label(
                egui::RichText::new(t!("plugins.empty_title"))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
            return;
        }
        ui.spacing_mut().item_spacing.y = 3.0;
        for p in visible {
            let selected = self.selection == Selection::Plugin(p.id.clone());
            // An on-disk change only flags the row while the
            // plugin is held disabled; re-enabling accepts it.
            let needs_action = plugin_requires_regrant(p)
                || cx.udev.is_some_and(|status| {
                    status.supported
                        && !status.current
                        && status.plugins_requiring_update.iter().any(|id| id == &p.id)
                })
                || cx.plugin_updates().iter().any(|s| {
                    s.plugin_id == p.id && (s.update_available || (s.on_disk_changed && !p.enabled))
                });
            let update_available = cx
                .plugin_updates()
                .iter()
                .any(|s| s.plugin_id == p.id && s.update_available);
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
        let update = cx.plugin_updates().iter().find(|s| s.plugin_id == p.id);
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
                let on = locked_target.unwrap_or_else(|| plugin_enabled_with_consent(p));
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
            // A failed plugin cannot expose its normal controls, but an upstream
            // update may be the only way to make it compatible again. Keep that
            // recovery action available even when the checked-out files also count
            // as modified on disk.
            if let Some(update) = update.filter(|u| {
                u.update_available && p.provenance != PluginProvenance::LocalDevelopment
            }) {
                ui.add_space(theme::SPACE_7);
                let in_flight = self.updating.contains_key(&update.plugin_id);
                if update_banner(
                    ui,
                    &update.current_version,
                    &update.available_version,
                    in_flight,
                ) {
                    self.updating
                        .insert(update.plugin_id.clone(), ui.input(|i| i.time));
                    ipc::send(
                        cx.cmd,
                        DaemonCommand::UpdatePluginRepo {
                            slug: update.slug.clone(),
                        },
                    );
                }
            }
            return;
        }

        if plugin_requires_regrant(p) {
            ui.add_space(theme::SPACE_9);
            regrant_warning_banner(ui, p);
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

        // A local on-disk edit is surfaced ahead of an upstream update: it's the
        // surprising, security-relevant state (the daemon has disabled the plugin).
        // Only while the plugin is held inactive: once re-enabled, the risk is
        // accepted and the banner is gone.
        if update.is_some_and(|u| u.on_disk_changed) && !plugin_enabled_with_consent(p) {
            ui.add_space(theme::SPACE_9);
            modified_on_disk_banner(ui);
        } else if let Some(update) = update
            .filter(|u| u.update_available && p.provenance != PluginProvenance::LocalDevelopment)
        {
            // A plugin is updated by pulling its whole repository, never one
            // plugin at a time.
            ui.add_space(theme::SPACE_9);
            update_info_banner(ui, &update.current_version, &update.available_version);
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
        permissions_section(ui, p, cx.cmd);
    }

    // ── Dialogs ─────────────────────────────────────────────────────────────

    /// Grant-permission prompt: shown when the user turns on a plugin that
    /// declares permissions (or right after importing one). Lists each
    /// permission with what it lets the plugin do; "Grant & Enable" accepts and
    /// activates, "Cancel" leaves the plugin installed but off.
    fn consent_modal(&mut self, ctx: &egui::Context, state: &TopicStore, cmd: &CommandTx) {
        let Some(id) = self.pending_consent.clone() else {
            return;
        };
        let Some(p) = state.plugins.plugins.iter().find(|p| p.id == id) else {
            self.pending_consent = None;
            return;
        };

        let mut grant = false;
        let mut cancel = false;
        let dismissed = widgets::dialog(
            ctx,
            "plugin_consent",
            &t!("plugins.consent_title"),
            460.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.consent_body", name = plugin_name(p)))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
                match consent_reason(p) {
                    ConsentReason::AuthorityExpanded => {
                        ui.add_space(theme::SPACE_4);
                        ui.label(
                            egui::RichText::new(t!("plugins.consent_permission_added"))
                                .font(theme::body_sm())
                                .color(theme::STAT_AMBER),
                        );
                    }
                    ConsentReason::New => {}
                }
                ui.add_space(theme::SPACE_6);
                authority_review_cards(ui, p);
                ui.add_space(theme::SPACE_1);
                ui.label(
                    egui::RichText::new(t!("plugins.consent_warning"))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("plugins.consent_grant_enable"),
                    ButtonKind::Primary,
                    Vec2::new(170.0, 34.0),
                )
                .clicked()
                {
                    grant = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.consent_cancel"),
                    ButtonKind::Secondary,
                    Vec2::new(100.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );

        if grant {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::ConfirmPluginEnable {
                    id: id.clone(),
                    authority: p.authority.clone(),
                },
            );
            // Granting enables the plugin — apply and lock like a direct enable.
            apply_and_lock(&mut self.in_flight, &id, true);
            self.pending_consent = None;
        } else if cancel || dismissed {
            self.pending_consent = None;
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

fn clear_finished_updates(
    updating: &mut HashMap<String, f64>,
    updating_all: &mut Option<f64>,
    plugin_updates: &[PluginUpdateStatus],
    now: f64,
) {
    let still_due = |id: &str| {
        plugin_updates
            .iter()
            .any(|s| s.plugin_id == id && (s.update_available || s.on_disk_changed))
    };
    updating.retain(|id, started| still_due(id) && now - *started < UPDATE_TIMEOUT);
    if let Some(started) = *updating_all {
        let any_due = plugin_updates.iter().any(|s| s.update_available);
        if !any_due || now - started > UPDATE_TIMEOUT {
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

/// Toasts for plugins the daemon quarantined: derived from state (the daemon's
/// own toast fires before the GUI connects), once each until re-enabled.
pub(crate) fn quarantine_toasts(
    plugins: &[PluginInfo],
    plugin_updates: &[PluginUpdateStatus],
    toasted: &mut HashSet<String>,
    timestamp_ms: u64,
) -> Vec<halod_shared::types::Notification> {
    use halod_shared::types::{Notification, NotificationCode};

    let is_quarantined = |p: &PluginInfo| {
        !p.enabled
            && plugin_updates
                .iter()
                .any(|u| u.plugin_id == p.id && u.on_disk_changed)
    };
    toasted.retain(|id| plugins.iter().any(|p| &p.id == id && is_quarantined(p)));

    plugins
        .iter()
        .filter(|p| is_quarantined(p) && toasted.insert(p.id.clone()))
        .map(|p| Notification {
            code: NotificationCode::PluginContentChanged {
                plugin: plugin_name(p).to_owned(),
            },
            show_native: true,
            timestamp_ms,
        })
        .collect()
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
/// `MarkTourSeen` it. Mirrors [`quarantine_toasts`].
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

/// True when the user's enable intent is consent-satisfied. Runtime `active`
/// additionally depends on host requirements and is reported by the daemon.
fn plugin_enabled_with_consent(p: &PluginInfo) -> bool {
    p.enabled && p.consented
}

/// What a toggle click should do, given the plugin's current state.
#[derive(Debug, PartialEq, Eq)]
enum ToggleDecision {
    /// Active → turn it off.
    Disable,
    /// Every disabled-to-enabled transition opens the authority review modal.
    NeedsConsent,
}

/// Every enable is an explicit, current authority confirmation. This applies
/// to permission-free plugins too: transports and supported-device scope are
/// still meaningful authority for the user to review.
fn toggle_decision(p: &PluginInfo) -> ToggleDecision {
    if plugin_enabled_with_consent(p) {
        ToggleDecision::Disable
    } else {
        ToggleDecision::NeedsConsent
    }
}

/// Outcome of an enable/disable toggle click.
struct ToggleOutcome {
    /// The `pending_consent` to keep (this plugin's id when a grant modal opens).
    pending_consent: Option<String>,
    /// `Some(target)` when a `SetPluginEnabled` was dispatched.
    dispatched: Option<bool>,
}

/// Apply a toggle click through the consent gate. Enabling/disabling dispatches
/// immediately; a plugin needing permission opens the grant modal instead (by
/// returning its id as the new `pending_consent`).
fn request_toggle(cmd: &CommandTx, p: &PluginInfo, pending: Option<String>) -> ToggleOutcome {
    match toggle_decision(p) {
        ToggleDecision::Disable => {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetPluginEnabled {
                    id: p.id.clone(),
                    enabled: false,
                },
            );
            ToggleOutcome {
                pending_consent: pending,
                dispatched: Some(false),
            }
        }
        ToggleDecision::NeedsConsent => ToggleOutcome {
            pending_consent: Some(p.id.clone()),
            dispatched: None,
        },
    }
}

/// Apply a just-dispatched toggle immediately and lock it at `target`.
fn apply_and_lock(in_flight: &mut HashMap<String, bool>, id: &str, target: bool) {
    in_flight.insert(id.to_owned(), target);
}

/// A toggle has landed once the plugin is at the target and the re-probe scan
/// has finished — so the toggle stays locked for the whole
/// (multi-second) device scan, not just until the config flag flips.
fn plugin_toggle_landed(p: &PluginInfo, target: bool, discovering: bool) -> bool {
    plugin_enabled_with_consent(p) == target && !discovering
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
    /// Off — disabled, unconsented, or unsupported.
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

fn status_dot(p: &PluginInfo) -> egui::Color32 {
    status_visual(plugin_status(p)).0
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
    let toggle_on = locked_target.unwrap_or_else(|| plugin_enabled_with_consent(p));
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
        PluginProvenance::VerifiedOfficial => t!("plugins.provenance_official"),
        PluginProvenance::UnsignedRepository => t!("plugins.provenance_repository"),
        PluginProvenance::LocalDevelopment => t!("plugins.provenance_development"),
        PluginProvenance::LocalUnsigned => t!("plugins.provenance_local"),
        PluginProvenance::InvalidSignature => t!("plugins.provenance_invalid"),
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

/// Amber "Update available vX → vY / Update" banner in a plugin's detail. Never
/// automatic.
/// Banner shown when a plugin's on-disk content differs from its checked-out
/// baseline — a local edit or tampering. The daemon has disabled the plugin for
/// safety; this explains why and how to recover. Purely informational: the user
/// re-enables it with the normal toggle, which accepts the current content.
fn modified_on_disk_banner(ui: &mut egui::Ui) {
    let title = t!("plugins.modified_on_disk");
    let sub = t!("plugins.modified_on_disk_sub");
    widgets::Banner::warn(title.as_ref())
        .subtitle(sub.as_ref())
        .show(ui);
}

/// Prominent recovery hint for a plugin disabled after its previously approved
/// content or permission set changed. The row dot gets the user here; this
/// banner makes the required action explicit before the normal detail content.
fn regrant_warning_banner(ui: &mut egui::Ui, p: &PluginInfo) {
    let detail = match consent_reason(p) {
        ConsentReason::AuthorityExpanded => t!("plugins.consent_permission_added"),
        ConsentReason::New => t!("plugins.consent_modified"),
    };
    let title = t!("plugins.regrant_required");
    widgets::Banner::warn(title.as_ref())
        .subtitle(detail.as_ref())
        .show(ui);
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

/// Informational "Update available vX → vY" banner in a plugin's detail, with
/// no action: a plugin is updated by pulling its whole repository, so the button
/// lives on the repo detail instead.
fn update_info_banner(ui: &mut egui::Ui, current: &str, available: &str) {
    let title = t!("plugins.update_available");
    widgets::Banner::warn(title.as_ref())
        .title_color(theme::TEXT)
        .title_font(theme::body_md())
        .show_with(ui, |ui| {
            if !current.is_empty() || !available.is_empty() {
                ui.add_space(theme::SPACE_1);
                ui.label(
                    egui::RichText::new(format!("{current} → {available}"))
                        .font(theme::value_sm())
                        .color(theme::STAT_AMBER),
                );
            }
            ui.add_space(theme::SPACE_1);
            ui.label(
                egui::RichText::new(t!("plugins.update_via_repo"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
        });
}

fn update_banner(ui: &mut egui::Ui, current: &str, available: &str, updating: bool) -> bool {
    let title = t!("plugins.update_available");
    let label = if updating {
        t!("plugins.updating")
    } else {
        t!("plugins.repos_update")
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

    pub(super) fn status(id: &str, update_available: bool) -> PluginUpdateStatus {
        PluginUpdateStatus {
            plugin_id: id.to_owned(),
            slug: "repo".to_owned(),
            update_available,
            on_disk_changed: false,
            current_version: "1.0.0".to_owned(),
            available_version: "1.1.0".to_owned(),
        }
    }

    #[test]
    fn updates_available_key_picks_singular_only_for_one() {
        assert_eq!(updates_available_key(1), "plugins.updates_available");
        assert_eq!(updates_available_key(0), "plugins.updates_available_plural");
        assert_eq!(updates_available_key(9), "plugins.updates_available_plural");
    }

    #[test]
    fn clear_finished_updates_retires_a_plugin_that_finished_updating() {
        let mut updating = HashMap::from([("a".to_owned(), 0.0)]);
        let mut updating_all = None;
        // Still reporting an available update: the spinner stays.
        clear_finished_updates(&mut updating, &mut updating_all, &[status("a", true)], 1.0);
        assert!(updating.contains_key("a"));
        // Update landed (flag cleared): the spinner is retired.
        clear_finished_updates(&mut updating, &mut updating_all, &[status("a", false)], 2.0);
        assert!(updating.is_empty());
    }

    #[test]
    fn clear_finished_updates_drops_a_stuck_spinner_after_the_timeout() {
        let mut updating = HashMap::from([("a".to_owned(), 0.0)]);
        let mut updating_all = None;
        // Still "due" but past the failsafe deadline — drop it anyway.
        clear_finished_updates(
            &mut updating,
            &mut updating_all,
            &[status("a", true)],
            UPDATE_TIMEOUT + 1.0,
        );
        assert!(updating.is_empty());
    }

    #[test]
    fn clear_finished_updates_clears_update_all_once_nothing_is_due() {
        let mut updating = HashMap::new();
        let mut updating_all = Some(0.0);
        // One plugin still due: keep the "update all" spinner.
        clear_finished_updates(&mut updating, &mut updating_all, &[status("a", true)], 1.0);
        assert_eq!(updating_all, Some(0.0));
        // Nothing due anymore: clear it.
        clear_finished_updates(&mut updating, &mut updating_all, &[status("a", false)], 2.0);
        assert_eq!(updating_all, None);
    }

    #[test]
    fn command_error_notification_stops_update_progress_immediately() {
        let mut screen = PluginsUi {
            updating: HashMap::from([("plugin-a".to_owned(), 1.0)]),
            updating_all: Some(1.0),
            updating_repos: HashMap::from([("repo-a".to_owned(), 1.0)]),
            ..Default::default()
        };
        let notification = halod_shared::types::Notification {
            code: halod_shared::types::NotificationCode::Generic {
                message: "repository update would roll back".to_owned(),
            },
            show_native: false,
            timestamp_ms: 0,
        };

        screen.observe_notifications(&[notification]);

        assert!(screen.updating.is_empty());
        assert!(screen.updating_all.is_none());
        assert!(screen.updating_repos.is_empty());
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
            integration_enabled: true,
            integration_configured: true,
            integration_requires_setup: false,
            integration_state: halod_shared::types::IntegrationLifecycleState::Configured,
            integration_setup: None,
            consented: true,
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
    fn quarantine_toasts_fire_once_then_rearm_after_reenable() {
        let quarantined = |id: &str| PluginUpdateStatus {
            on_disk_changed: true,
            ..status(id, false)
        };
        let mut disabled = info("edited", true);
        disabled.enabled = false;
        let ok = info("ok", true);
        let updates = vec![quarantined("edited"), status("ok", false)];
        let mut toasted = HashSet::new();

        let first = quarantine_toasts(&[disabled.clone(), ok.clone()], &updates, &mut toasted, 0);
        assert_eq!(first.len(), 1);
        assert!(
            quarantine_toasts(&[disabled.clone(), ok.clone()], &updates, &mut toasted, 0)
                .is_empty()
        );

        // Re-enabled → forgotten, and a later re-quarantine alerts again.
        let mut reenabled = disabled.clone();
        reenabled.enabled = true;
        assert!(quarantine_toasts(&[reenabled, ok.clone()], &updates, &mut toasted, 0).is_empty());
        assert!(toasted.is_empty());
        assert_eq!(
            quarantine_toasts(&[disabled, ok], &updates, &mut toasted, 0).len(),
            1
        );
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

    pub(super) fn repo(slug: &str, release_tag: &str) -> PluginRepoInfo {
        PluginRepoInfo {
            url: format!("https://example.com/{slug}.git"),
            slug: slug.to_owned(),
            repository_id: None,
            release_tag: Some(release_tag.to_owned()),
            release_policy: Default::default(),
            active_revision: None,
            previous_release_tag: None,
            last_sync: None,
            official: false,
            location: Default::default(),
            signature: RepoSignatureStatus::Unsigned,
            signing_key_fingerprint: None,
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
        let repos = vec![repo("foo", "aaaaaaaa")];
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
        assert_eq!(status_dot(&info("a", true)), theme::ONLINE);
        assert_eq!(status_dot(&info("a", false)), theme::TEXT_FAINT2);
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
        assert_eq!(status_dot(&blocked), theme::OFFLINE);
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
    fn toggle_decision_routes_through_the_consent_gate() {
        use halod_shared::types::Permission;
        // Permission-free, off → authority review.
        let mut p = info("a", false);
        assert_eq!(toggle_decision(&p), ToggleDecision::NeedsConsent);

        // Permission-free, on → disable.
        p.enabled = true;
        assert_eq!(toggle_decision(&p), ToggleDecision::Disable);

        // Declares a permission, not yet consented, off → must consent first.
        let mut q = info("b", false);
        q.declared_permissions = vec![Permission::Network];
        q.consented = false;
        assert_eq!(toggle_decision(&q), ToggleDecision::NeedsConsent);

        // Even "enabled" but unconsented is not active → still needs consent.
        q.enabled = true;
        assert_eq!(toggle_decision(&q), ToggleDecision::NeedsConsent);

        // Granted (consented) + enabled → active → disable.
        q.consented = true;
        assert_eq!(toggle_decision(&q), ToggleDecision::Disable);
    }

    #[test]
    fn enabled_with_consent_requires_both_flags() {
        let mut p = info("a", true);
        assert!(plugin_enabled_with_consent(&p));
        p.consented = false;
        assert!(
            !plugin_enabled_with_consent(&p),
            "unconsented is inert even if enabled"
        );
        p.consented = true;
        p.enabled = false;
        assert!(!plugin_enabled_with_consent(&p), "disabled is not active");
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

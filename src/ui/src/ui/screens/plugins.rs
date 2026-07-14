// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugins page — a master–detail view of the Lua device plugins found in the
//! plugins directory (plus built-ins). The left column lists every plugin with
//! an enable toggle; the right column shows the selected plugin's detail. User
//! scripts can be added (upload a `.lua` file or paste source) and deleted;
//! built-ins can be toggled but not deleted.

use std::collections::{HashMap, HashSet};

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    AppState, PluginDownloadConsent, PluginInfo, PluginIssue, PluginIssueKind, PluginRepoInfo,
    PluginSource, PluginUpdateStatus, RepoUpdateStatus,
};

use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::icons;
use crate::ui::theme;

/// In-progress state of the add-plugin modal. A plugin is always a directory
/// package (`plugin.yaml` + entry script) — there is no single-file/pasted
/// import path.
#[derive(Default)]
struct AddState;

/// In-progress state of the "Add repository" modal.
#[derive(Default)]
struct AddRepoState {
    url: String,
    /// Selected branch; empty means the remote's default branch.
    branch: String,
    /// URL we've already asked the daemon to enumerate branches for.
    fetched_url: Option<String>,
    /// Deadline (egui time) after which to request branches for a newly-typed
    /// URL — a small debounce so we don't `ls-remote` on every keystroke.
    fetch_at: Option<f64>,
}

/// Debounce before enumerating a freshly-typed repo URL's branches.
const REPO_BRANCH_FETCH_DEBOUNCE: f64 = 0.4;

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
    name: String,
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
    /// The add-plugin modal, when open.
    add: Option<AddState>,
    /// The add-repository modal, when open.
    add_repo: Option<AddRepoState>,
    /// Id pending a delete confirmation, when the dialog is open.
    pending_delete: Option<String>,
    /// Repo slug pending a remove confirmation, when the dialog is open.
    pending_repo_delete: Option<String>,
    /// Malformed repo package pending a scoped recovery confirmation.
    pending_repo_repair: Option<PendingRepoRepair>,
    /// The repo whose "check for updates" is currently in flight, if any.
    checking_repo: Option<RepoCheck>,
    /// Id pending a permission consent decision, when the dialog is open.
    pending_consent: Option<String>,
    /// Plugin ids whose enable/disable is applying → target state; the toggle is
    /// locked at the target until the daemon confirms.
    in_flight: HashMap<String, bool>,
    /// Plugin ids seen as of the previous frame, used only to spot a
    /// freshly-imported plugin. `None` until the first frame, so pre-existing
    /// plugins at startup never spuriously pop the dialog.
    known_ids: Option<std::collections::HashSet<String>>,
    /// Set right when the GUI sends an import command, cleared on the next
    /// new plugin id it sees (whichever outcome). Distinguishes "the user
    /// just added this through the Add-plugin modal" (blocking consent
    /// dialog here) from an auto-discovered plugin found by a directory scan
    /// (the daemon pushes a toast notification for those instead).
    awaiting_import: bool,
    /// Asset cache keys already requested, so a pending fetch isn't re-sent.
    requested_assets: HashSet<String>,
    /// Decoded asset bytes turned into textures, keyed like `requested_assets`.
    asset_textures: HashMap<String, egui::TextureHandle>,
    /// Plugin ids whose single-plugin update is in flight → egui time it began.
    /// The button shows a spinner until the plugin drops its "update available"
    /// flag (the daemon re-broadcasts once the checkout lands) or it times out.
    updating: HashMap<String, f64>,
    /// "Update all" in flight → egui time it began, cleared the same way.
    updating_all: Option<f64>,
    /// The open plugin-issue Details modal (title + detail), when the user
    /// clicked Details on a plugin's issue banner.
    issue_modal: Option<(String, String)>,
}

/// Failsafe: drop an update spinner after this long even if the daemon never
/// re-broadcasts (e.g. an unreachable remote mid-update).
const UPDATE_TIMEOUT: f64 = 90.0;

impl PluginsUi {
    #[allow(clippy::too_many_arguments)] // screen inputs mirror independent IPC result streams
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        plugin_assets: &HashMap<String, Vec<u8>>,
        repo_updates: &[RepoUpdateStatus],
        plugin_updates: &[PluginUpdateStatus],
        repo_branches: &HashMap<String, Vec<String>>,
    ) {
        self.selection = resolve_selection(
            self.selection.clone(),
            &state.plugins.plugins,
            &state.plugins.repos,
        );
        self.detect_new_plugin_needing_consent(&state.plugins.plugins);
        self.sync_assets(ui.ctx(), cmd, &state.plugins.plugins, plugin_assets);

        widgets::page_frame(ui, |ui| {
            self.body(ui, state, cmd, repo_updates, plugin_updates)
        });

        self.add_modal(ui.ctx(), cmd);
        self.add_repo_modal(ui.ctx(), cmd, repo_branches);
        self.delete_modal(ui.ctx(), state, cmd);
        self.repo_delete_modal(ui.ctx(), cmd);
        self.repo_repair_modal(ui.ctx(), cmd);
        self.consent_modal(ui.ctx(), state, cmd);
        if let Some((title, detail)) = &self.issue_modal {
            if widgets::issue_modal(ui.ctx(), "plugin_issue_page", title, detail) {
                self.issue_modal = None;
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
                    crate::runtime::ipc::send(
                        cmd,
                        halod_shared::commands::DaemonCommand::GetPluginAsset { plugin_id, name },
                    );
                }
            }
        }
        decode_new_assets(ctx, plugin_assets, &mut self.asset_textures);
    }

    /// A plugin id that appears now but wasn't present last frame opens the
    /// consent modal only when it arrived via our own import (see
    /// `awaiting_import`) — an auto-discovered plugin gets a toast instead
    /// (pushed by the daemon), not a blocking dialog.
    fn detect_new_plugin_needing_consent(&mut self, plugins: &[PluginInfo]) {
        let ids: std::collections::HashSet<String> = plugins.iter().map(|p| p.id.clone()).collect();
        if let Some(known) = &self.known_ids {
            let new: Vec<&PluginInfo> = plugins.iter().filter(|p| !known.contains(&p.id)).collect();
            if self.awaiting_import && !new.is_empty() {
                if let Some(p) = new.iter().find(|p| plugin_needs_permission(p)) {
                    self.pending_consent = Some(p.id.clone());
                }
                self.awaiting_import = false;
            }
        }
        self.known_ids = Some(ids);
    }

    fn sync_update_progress(&mut self, plugin_updates: &[PluginUpdateStatus], now: f64) {
        clear_finished_updates(
            &mut self.updating,
            &mut self.updating_all,
            plugin_updates,
            now,
        );
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        repo_updates: &[RepoUpdateStatus],
        plugin_updates: &[PluginUpdateStatus],
    ) {
        let title_resp = ui.label(
            egui::RichText::new(t!("plugins.title"))
                .font(theme::bold(22.0))
                .color(theme::TEXT),
        );
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::PluginsOverview,
            title_resp.rect,
        );
        ui.add_space(3.0);
        ui.label(
            egui::RichText::new(t!("plugins.subtitle"))
                .font(theme::body(12.0))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(18.0);

        reconcile_in_flight(
            &mut self.in_flight,
            &state.plugins.plugins,
            matches!(
                state.discovery.phase,
                halod_shared::types::DiscoveryPhase::Discovering
            ),
        );

        let now = ui.input(|i| i.time);
        self.sync_update_progress(plugin_updates, now);

        let due = plugin_updates.iter().filter(|s| s.update_available).count();
        if due > 0 {
            if update_all_banner(ui, due, self.updating_all.is_some()) {
                self.updating_all = Some(now);
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::UpdateAllPlugins,
                );
            }
            ui.add_space(18.0);
        }
        if self.updating_all.is_some() || !self.updating.is_empty() {
            // Keep the timeout advancing and the spinner animating.
            ui.ctx().request_repaint();
        }

        widgets::split_columns(ui, 320.0, 18.0, |left, right| {
            self.list_column(left, state, cmd, repo_updates, plugin_updates);
            self.detail_column(right, state, cmd, plugin_updates);
        });
    }

    // ── Left: plugin list + repositories ────────────────────────────────────

    fn list_column(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        repo_updates: &[RepoUpdateStatus],
        plugin_updates: &[PluginUpdateStatus],
    ) {
        widgets::card(ui, |ui| {
            let active = state
                .plugins
                .plugins
                .iter()
                .filter(|p| plugin_active(p))
                .count();
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(t!("plugins.title"))
                                .font(theme::semibold(15.0))
                                .color(theme::TEXT),
                        );
                        ui.label(
                            egui::RichText::new(t!(
                                "plugins.counts",
                                count = state.plugins.plugins.len(),
                                active = active
                            ))
                            .font(theme::mono(10.0))
                            .color(theme::TEXT_FAINT),
                        );
                    });
                },
                |ui| {
                    let add_resp = widgets::button(
                        ui,
                        &t!("plugins.add"),
                        ButtonKind::Primary,
                        Vec2::new(96.0, 30.0),
                    );
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::PluginsAddPlugin,
                        add_resp.rect,
                    );
                    if add_resp.clicked() {
                        self.add = Some(AddState);
                    }
                },
            );
            ui.add_space(12.0);

            if state.plugins.plugins.is_empty() {
                ui.label(
                    egui::RichText::new(t!("plugins.empty_title"))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_MUT),
                );
            } else {
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for p in &state.plugins.plugins {
                            let selected = self.selection == Selection::Plugin(p.id.clone());
                            // An on-disk change only flags the row while the
                            // plugin is held disabled; re-enabling accepts it.
                            let needs_action = plugin_requires_regrant(p)
                                || plugin_updates.iter().any(|s| {
                                    s.plugin_id == p.id
                                        && (s.update_available || (s.on_disk_changed && !p.enabled))
                                });
                            let logo_tex = p
                                .logo
                                .as_deref()
                                .map(|name| ipc::plugin_asset_cache_key(&p.id, name))
                                .and_then(|key| self.asset_textures.get(&key));
                            let locked = if is_load_failed(p) {
                                Some(false)
                            } else {
                                self.in_flight.get(&p.id).copied()
                            };
                            match list_row(ui, p, selected, needs_action, logo_tex, locked) {
                                RowAction::Select => {
                                    self.selection = Selection::Plugin(p.id.clone())
                                }
                                RowAction::Toggle => {
                                    let out = request_toggle(cmd, p, self.pending_consent.take());
                                    self.pending_consent = out.pending_consent;
                                    if let Some(target) = out.dispatched {
                                        apply_and_lock(&mut self.in_flight, &p.id, target);
                                    }
                                }
                                RowAction::None => {}
                            }
                        }
                    });
            }

            self.skipped_notice(ui, state);

            ui.add_space(18.0);
            egui::Sides::new().show(
                ui,
                |ui| {
                    widgets::caps_label_inline(ui, &t!("plugins.repos_title"));
                },
                |ui| {
                    let add_repo_resp =
                        widgets::button(ui, "+", ButtonKind::Ghost, Vec2::new(28.0, 26.0));
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::PluginsAddRepo,
                        add_repo_resp.rect,
                    );
                    if add_repo_resp.clicked() {
                        self.add_repo = Some(AddRepoState::default());
                    }
                },
            );
            ui.add_space(8.0);

            let rows = repo_rows(&state.plugins.repos, repo_updates);
            if rows.is_empty() {
                ui.label(
                    egui::RichText::new(t!("plugins.repos_empty"))
                        .font(theme::body(11.5))
                        .color(theme::TEXT_MUT),
                );
                return;
            }
            for row in rows {
                let selected = self.selection == Selection::Repo(row.slug.to_owned());
                if repo_row(ui, &row, selected) {
                    self.selection = Selection::Repo(row.slug.to_owned());
                }
            }
        });
    }

    fn skipped_notice(&mut self, ui: &mut egui::Ui, state: &AppState) {
        if state.plugins.skipped.is_empty() {
            return;
        }
        ui.add_space(18.0);
        widgets::caps_label_inline(ui, &t!("plugins.skipped_heading"));
        ui.add_space(6.0);
        for s in &state.plugins.skipped {
            let name = std::path::Path::new(&s.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(s.path.as_str());
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter()
                            .circle_filled(dot.center(), 3.0, theme::TRAFFIC_RED);
                        ui.label(
                            egui::RichText::new(name)
                                .font(theme::body(12.0))
                                .color(theme::TEXT_DIM),
                        );
                    });
                },
                |ui| {
                    if let Some((slug, _)) = skipped_repo_location(&s.path, &state.plugins.repos) {
                        if widgets::button(
                            ui,
                            &t!("plugins.repos_repair"),
                            ButtonKind::Danger,
                            Vec2::new(70.0, 26.0),
                        )
                        .clicked()
                        {
                            self.pending_repo_repair = Some(PendingRepoRepair {
                                slug,
                                name: name.to_owned(),
                            });
                        }
                        ui.add_space(6.0);
                    }
                    if widgets::button(
                        ui,
                        &t!("plugins.issue_details"),
                        ButtonKind::Ghost,
                        Vec2::new(70.0, 26.0),
                    )
                    .clicked()
                    {
                        self.issue_modal = Some((
                            t!("plugins.skipped_title").to_string(),
                            format!("{}\n\n{}", s.path, s.reason),
                        ));
                    }
                },
            );
            ui.add_space(4.0);
        }
    }

    // ── Right: detail ───────────────────────────────────────────────────────

    fn detail_column(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        plugin_updates: &[PluginUpdateStatus],
    ) {
        match &self.selection {
            Selection::Plugin(id) => {
                let Some(p) = state.plugins.plugins.iter().find(|p| &p.id == id) else {
                    widgets::empty_state(
                        ui,
                        &t!("plugins.empty_title"),
                        Some(&t!("plugins.empty_hint")),
                    );
                    return;
                };
                let logo_tex = p
                    .logo
                    .as_deref()
                    .map(|name| ipc::plugin_asset_cache_key(&p.id, name))
                    .and_then(|key| self.asset_textures.get(&key));
                let update = plugin_updates.iter().find(|s| &s.plugin_id == id);
                let now = ui.input(|i| i.time);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        widgets::card(ui, |ui| {
                            detail_body(
                                ui,
                                p,
                                cmd,
                                logo_tex,
                                update,
                                &mut self.pending_delete,
                                &mut self.pending_consent,
                                &mut self.in_flight,
                                &mut self.updating,
                                &mut self.issue_modal,
                                now,
                            )
                        });
                    });
            }
            Selection::Repo(slug) => {
                let Some(r) = state.plugins.repos.iter().find(|r| &r.slug == slug) else {
                    widgets::empty_state(
                        ui,
                        &t!("plugins.empty_title"),
                        Some(&t!("plugins.empty_hint")),
                    );
                    return;
                };
                let now = ui.input(|i| i.time);
                // Drop the spinner once the check lands (last_sync advanced) or
                // times out.
                if let Some(c) = &self.checking_repo {
                    let landed = c.slug == r.slug && r.last_sync != c.prev_sync;
                    if landed || now - c.started > REPO_CHECK_TIMEOUT {
                        self.checking_repo = None;
                    }
                }
                let checking = self
                    .checking_repo
                    .as_ref()
                    .is_some_and(|c| c.slug == r.slug);
                let updates_enabled = plugin_updates_enabled(state.gui.plugin_downloads);

                let mut select_plugin = None;
                let mut start_check = false;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        widgets::card(ui, |ui| {
                            select_plugin = repo_detail_body(
                                ui,
                                r,
                                &state.plugins.plugins,
                                &mut self.pending_repo_delete,
                                checking,
                                updates_enabled,
                                &mut start_check,
                            );
                        });
                    });
                if start_check {
                    self.checking_repo = Some(RepoCheck {
                        slug: r.slug.clone(),
                        prev_sync: r.last_sync.clone(),
                        started: now,
                    });
                    crate::runtime::ipc::send(
                        cmd,
                        halod_shared::commands::DaemonCommand::CheckPluginRepoUpdates,
                    );
                }
                if self.checking_repo.is_some() {
                    // Keep animating the spinner and advancing the timeout.
                    ui.ctx().request_repaint();
                }
                if let Some(id) = select_plugin {
                    self.selection = Selection::Plugin(id);
                }
            }
            Selection::None => {
                widgets::empty_state(
                    ui,
                    &t!("plugins.empty_title"),
                    Some(&t!("plugins.empty_hint")),
                );
            }
        }
    }

    // ── Dialogs ─────────────────────────────────────────────────────────────

    fn add_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(add) = self.add.take() else {
            return;
        };
        let mut pick_folder = false;
        let mut cancel = false;

        let dismissed = widgets::dialog(
            ctx,
            "add_plugin",
            &t!("plugins.add_title"),
            480.0,
            add_body,
            |ui| {
                // `dialog` lays actions out right-to-left; add the primary first.
                if widgets::button(
                    ui,
                    &t!("plugins.choose_folder"),
                    ButtonKind::Primary,
                    Vec2::new(150.0, 34.0),
                )
                .clicked()
                {
                    pick_folder = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );

        if pick_folder {
            // Optimistic: the folder dialog may still be cancelled, in which
            // case no plugin ever appears and this flag is simply never consumed.
            self.awaiting_import = true;
            spawn_import_plugin(ctx, cmd.clone());
            return; // modal closes; import completes when the user picks a folder
        }
        if cancel || dismissed {
            return;
        }
        self.add = Some(add);
    }

    fn add_repo_modal(
        &mut self,
        ctx: &egui::Context,
        cmd: &CommandTx,
        repo_branches: &HashMap<String, Vec<String>>,
    ) {
        let Some(mut form) = self.add_repo.take() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;
        let mut fetch_url: Option<String> = None;
        let now = ctx.input(|i| i.time);

        let dismissed = widgets::dialog(
            ctx,
            "add_plugin_repo",
            &t!("plugins.repos_add_title"),
            420.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.repos_add_sub"))
                        .font(theme::body(11.5))
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(14.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut form.url)
                        .desired_width(f32::INFINITY)
                        .margin(egui::vec2(12.0, 9.0))
                        .hint_text(t!("plugins.repos_url_hint")),
                );
                if resp.changed() {
                    // A newly-typed URL invalidates the previous branch choice.
                    form.branch.clear();
                    form.fetch_at = Some(now + REPO_BRANCH_FETCH_DEBOUNCE);
                }
                if let Some(url) = branch_fetch_due(&form, now) {
                    form.fetched_url = Some(url.clone());
                    form.fetch_at = None;
                    fetch_url = Some(url);
                }
                ui.add_space(12.0);
                widgets::caps_label(ui, &t!("plugins.repos_branch_label"));
                ui.add_space(6.0);
                if let Some(picked) = branch_selector(ui, &form, repo_branches) {
                    form.branch = picked;
                }
            },
            |ui| {
                // `dialog` lays actions out right-to-left; add the primary first.
                if widgets::button(
                    ui,
                    &t!("plugins.repos_add"),
                    ButtonKind::Primary,
                    Vec2::new(110.0, 34.0),
                )
                .clicked()
                {
                    confirm = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );

        if let Some(url) = fetch_url {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::ListRepoBranches { url },
            );
        }
        // Keep repainting while a fetch is pending so the debounce deadline
        // fires even without further keystrokes.
        if form.fetch_at.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                REPO_BRANCH_FETCH_DEBOUNCE,
            ));
        }

        if confirm {
            let url = form.url.trim().to_owned();
            if !url.is_empty() {
                let branch = form.branch.trim();
                let branch = if branch.is_empty() {
                    None
                } else {
                    Some(branch.to_owned())
                };
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::AddPluginRepo { url, branch },
                );
                return;
            }
        }
        if cancel || dismissed {
            return;
        }
        self.add_repo = Some(form);
    }

    fn delete_modal(&mut self, ctx: &egui::Context, state: &AppState, cmd: &CommandTx) {
        if self.pending_delete.is_none() {
            return;
        }
        let name = self
            .pending_delete
            .as_deref()
            .and_then(|id| state.plugins.plugins.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_default();

        let mut confirm = false;
        let mut cancel = false;
        let dismissed = widgets::dialog(
            ctx,
            "delete_plugin",
            &t!("plugins.delete_title"),
            420.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.delete_body", name = name))
                        .font(theme::body(12.5))
                        .color(theme::TEXT_DIM),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("plugins.delete"),
                    ButtonKind::Danger,
                    Vec2::new(110.0, 34.0),
                )
                .clicked()
                {
                    confirm = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );
        if let Some(id) =
            widgets::resolve_delete_confirm(&mut self.pending_delete, confirm, cancel || dismissed)
        {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::DeletePlugin { id },
            );
        }
    }

    /// Confirm removing a repository before unregistering it (this uninstalls
    /// every plugin the repo contributed, so it's not silently destructive).
    fn repo_delete_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(slug) = self.pending_repo_delete.clone() else {
            return;
        };

        let mut confirm = false;
        let mut cancel = false;
        let dismissed = widgets::dialog(
            ctx,
            "remove_plugin_repo",
            &t!("plugins.repos_remove_title"),
            420.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.repos_remove_body", name = slug.clone()))
                        .font(theme::body(12.5))
                        .color(theme::TEXT_DIM),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("plugins.repos_remove"),
                    ButtonKind::Danger,
                    Vec2::new(150.0, 34.0),
                )
                .clicked()
                {
                    confirm = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );
        if let Some(slug) = widgets::resolve_delete_confirm(
            &mut self.pending_repo_delete,
            confirm,
            cancel || dismissed,
        ) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::RemovePluginRepo { slug },
            );
        }
    }

    /// Confirm restoring only the malformed package directory, leaving every
    /// sibling path in the repository untouched.
    fn repo_repair_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(pending) = self.pending_repo_repair.clone() else {
            return;
        };

        let mut confirm = false;
        let mut cancel = false;
        let dismissed = widgets::dialog(
            ctx,
            "repair_plugin_repo",
            &t!("plugins.repos_repair_title"),
            440.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!(
                        "plugins.repos_repair_body",
                        name = pending.name.clone()
                    ))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_DIM),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("plugins.repos_repair"),
                    ButtonKind::Danger,
                    Vec2::new(130.0, 34.0),
                )
                .clicked()
                {
                    confirm = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );
        if let Some(pending) = widgets::resolve_delete_confirm(
            &mut self.pending_repo_repair,
            confirm,
            cancel || dismissed,
        ) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::UpdatePluginRepo { slug: pending.slug },
            );
        }
    }

    /// Grant-permission prompt: shown when the user turns on a plugin that
    /// declares permissions (or right after importing one). Lists each
    /// permission with what it lets the plugin do; "Grant & Enable" accepts and
    /// activates, "Cancel" leaves the plugin installed but off.
    fn consent_modal(&mut self, ctx: &egui::Context, state: &AppState, cmd: &CommandTx) {
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
                    egui::RichText::new(t!("plugins.consent_body", name = p.name.clone()))
                        .font(theme::body(12.5))
                        .color(theme::TEXT_DIM),
                );
                match consent_reason(p) {
                    ConsentReason::PermissionAdded => {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(t!("plugins.consent_permission_added"))
                                .font(theme::body(11.5))
                                .color(theme::STAT_AMBER),
                        );
                    }
                    ConsentReason::ContentChanged => {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(t!("plugins.consent_modified"))
                                .font(theme::body(11.5))
                                .color(theme::STAT_AMBER),
                        );
                    }
                    ConsentReason::New => {}
                }
                ui.add_space(12.0);
                for perm in &p.declared_permissions {
                    permission_card(ui, *perm);
                    ui.add_space(8.0);
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(t!("plugins.consent_warning"))
                        .font(theme::body(11.5))
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
                    ButtonKind::Ghost,
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

/// Map a skipped absolute path back to one of the package layouts accepted by
/// the daemon's repo scanner: `<repo>/<package>` or `<repo>/plugins/<package>`.
fn skipped_repo_location(path: &str, repos: &[PluginRepoInfo]) -> Option<(String, String)> {
    let parts: Vec<String> = std::path::Path::new(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    for repo in repos {
        for i in 0..parts.len().saturating_sub(1) {
            if !parts[i].eq_ignore_ascii_case("plugin_repos")
                || !parts[i + 1].eq_ignore_ascii_case(&repo.slug)
            {
                continue;
            }
            let relative = &parts[i + 2..];
            let valid = relative.len() == 1
                || (relative.len() == 2 && relative[0].eq_ignore_ascii_case("plugins"));
            if valid {
                let subpath: std::path::PathBuf = relative.iter().collect();
                return Some((repo.slug.clone(), subpath.display().to_string()));
            }
        }
    }
    None
}

// ── Plugin repository rows ──────────────────────────────────────────────────

/// One repo row as shown in the repositories list.
struct RepoRow<'a> {
    slug: &'a str,
    branch: Option<&'a str>,
    official: bool,
    locked_short: String,
    remote_short: Option<String>,
    behind: bool,
}

/// A commit SHA truncated to a short, still-unambiguous display form.
fn truncate_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Pair each repo with its update status, sorted by slug for a stable order
/// — except the official repo, which always sorts first.
fn repo_rows<'a>(repos: &'a [PluginRepoInfo], updates: &[RepoUpdateStatus]) -> Vec<RepoRow<'a>> {
    let mut rows: Vec<RepoRow> = repos
        .iter()
        .map(|r| {
            let status = updates.iter().find(|u| u.slug == r.slug);
            let behind = status.is_some_and(|s| s.behind);
            RepoRow {
                slug: &r.slug,
                branch: r.branch.as_deref(),
                official: r.official,
                locked_short: truncate_sha(&r.locked_sha).to_owned(),
                remote_short: status
                    .filter(|_| behind)
                    .map(|s| truncate_sha(&s.remote_sha).to_owned()),
                behind,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.official.cmp(&a.official).then_with(|| a.slug.cmp(b.slug)));
    rows
}

/// A small colored tile with a fork glyph, for a repo row/detail header.
fn repo_icon_tile(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, 9.0, theme::hex(0x161320));
    ui.painter().rect_stroke(
        rect,
        9.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    icons::draw_fork(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(size * 0.44)),
        theme::TEXT_MUT,
    );
}

/// One selectable repo row in the list column. Returns whether it was clicked.
fn repo_row(ui: &mut egui::Ui, row: &RepoRow, selected: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
    if row.official {
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::PluginsOfficialRepo,
            rect,
        );
    }
    if selected {
        ui.painter().rect_filled(rect, 9.0, theme::ROW_ACTIVE);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 9.0, theme::a(theme::ROW_ACTIVE, 0.55));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let icon_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 8.0, rect.center().y - 12.0),
        Vec2::splat(24.0),
    );
    ui.painter()
        .rect_filled(icon_rect, 6.0, theme::hex(0x161320));
    ui.painter().rect_stroke(
        icon_rect,
        6.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    // Keep the fork comfortably inset from the holder so the glyph does not
    // visually crowd the rounded tile at the compact row size.
    icons::draw_fork(ui.painter(), icon_rect.shrink(5.0), theme::TEXT_DIM);

    let text_x = rect.left() + 40.0;
    let sha_text = match &row.remote_short {
        Some(remote) => format!("{} → {}", row.locked_short, remote),
        None => row.locked_short.clone(),
    };
    if sha_text.is_empty() {
        // No commit yet (e.g. the official repo before its first sync) — center
        // the single title line against the icon instead of pinning it to the top.
        ui.painter().text(
            Pos2::new(text_x, rect.center().y),
            Align2::LEFT_CENTER,
            row.slug,
            theme::semibold(12.0),
            theme::TEXT,
        );
    } else {
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 12.0),
            Align2::LEFT_TOP,
            row.slug,
            theme::semibold(12.0),
            theme::TEXT,
        );
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 26.0),
            Align2::LEFT_TOP,
            sha_text,
            theme::mono(9.5),
            if row.behind {
                theme::STAT_AMBER
            } else {
                theme::TEXT_FAINT2
            },
        );
    }

    if let Some(branch) = row.branch {
        let branch_pos = Pos2::new(rect.right() - 10.0, rect.center().y);
        ui.painter().text(
            branch_pos,
            Align2::RIGHT_CENTER,
            branch,
            theme::mono(9.5),
            theme::TEXT_FAINT,
        );
    }

    resp.clicked()
}

/// The URL whose branches should be fetched now: `Some` once the debounce
/// deadline has passed for a non-empty URL we haven't already enumerated.
fn branch_fetch_due(form: &AddRepoState, now: f64) -> Option<String> {
    let at = form.fetch_at?;
    if now < at {
        return None;
    }
    let url = form.url.trim();
    if url.is_empty() || form.fetched_url.as_deref() == Some(url) {
        return None;
    }
    Some(url.to_owned())
}

/// Combo (id, display) pairs from branch names — id and display are identical
/// since the branch name is exactly what gets sent.
fn branch_options(branches: &[String]) -> Vec<(String, String)> {
    branches.iter().map(|b| (b.clone(), b.clone())).collect()
}

/// The Add-repository branch picker. Once the fetched URL's branches arrive it
/// renders a combo (with a leading "default branch" entry mapping to empty);
/// until then it shows a disabled placeholder combo. Returns the newly-picked
/// branch (empty = repo default), if it changed this frame.
fn branch_selector(
    ui: &mut egui::Ui,
    form: &AddRepoState,
    repo_branches: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let branches = form
        .fetched_url
        .as_deref()
        .and_then(|u| repo_branches.get(u));
    if let Some(branches) = branches.filter(|b| !b.is_empty()) {
        let options = branch_options(branches);
        return widgets::combo_picker(
            ui,
            "repo_branch",
            &options,
            &form.branch,
            Some(&t!("plugins.repos_branch_default")),
        );
    }
    let placeholder = if form.url.trim().is_empty() {
        t!("plugins.repos_branch_enter_url")
    } else {
        t!("plugins.repos_branch_loading")
    };
    ui.add_enabled_ui(false, |ui| {
        egui::ComboBox::from_id_salt("repo_branch_disabled")
            .selected_text(placeholder)
            .show_ui(ui, |_| {});
    });
    None
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
        if let Some(img) = image::load_from_memory(bytes).ok().map(|i| i.into_rgba8()) {
            let (w, h) = (img.width() as usize, img.height() as usize);
            let pixels: Vec<egui::Color32> = img
                .into_raw()
                .chunks_exact(4)
                .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
                .collect();
            let tex = ctx.load_texture(
                key.clone(),
                egui::ColorImage::new([w, h], pixels),
                egui::TextureOptions::LINEAR,
            );
            textures.insert(key.clone(), tex);
        }
    }
}

/// The centered, aspect-preserving sub-rect of `into` that a `tex`-sized image
/// fills (letterbox). Keeps a non-square logo from being stretched to the
/// square tile. Pure so the geometry is unit-testable.
pub(crate) fn logo_fit_rect(tex: Vec2, into: Rect) -> Rect {
    if tex.x <= 0.0 || tex.y <= 0.0 {
        return into;
    }
    let scale = (into.width() / tex.x).min(into.height() / tex.y);
    Rect::from_center_size(into.center(), Vec2::new(tex.x * scale, tex.y * scale))
}

/// Paint a logo texture into `rect`, letterboxed to preserve its aspect ratio.
pub(crate) fn draw_logo_fit(painter: &egui::Painter, rect: Rect, tex: &egui::TextureHandle) {
    painter.image(
        tex.id(),
        logo_fit_rect(tex.size_vec2(), rect),
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

#[cfg(test)]
fn plugin_file_name(p: &PluginInfo) -> &str {
    std::path::Path::new(&p.path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p.path)
}

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
                plugin: p.name.clone(),
            },
            timestamp_ms,
        })
        .collect()
}

/// True when `p` declares permissions the user hasn't consented to (never
/// granted, or the script changed since it was granted). It stays inert until
/// the user grants them. Shared with the Integrations screen, whose
/// integrations are plugins too.
pub(crate) fn plugin_needs_permission(p: &PluginInfo) -> bool {
    !p.declared_permissions.is_empty() && !p.consented
}

/// True when a previously approved plugin is inert until the user reviews its
/// permissions again after an update or edit. Unlike [`plugin_needs_permission`],
/// this deliberately excludes a newly installed plugin's first-time consent.
pub(crate) fn plugin_requires_regrant(p: &PluginInfo) -> bool {
    let has_new_permission = p
        .declared_permissions
        .iter()
        .any(|perm| !p.granted_permissions.contains(perm));
    let was_approved = !p.granted_permissions.is_empty();
    !p.enabled && (p.content_changed || (was_approved && has_new_permission))
}

/// Why the consent modal is being shown, so it can explain the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentReason {
    /// Never approved before — a first-time grant.
    New,
    /// Previously approved, and an update/edit added permissions beyond those
    /// already granted.
    PermissionAdded,
    /// Previously approved, but the script content changed since (no new
    /// permissions).
    ContentChanged,
}

/// Classify why `p` is asking for consent, from its granted vs declared
/// permissions and whether its content changed since the last acknowledgment.
pub(crate) fn consent_reason(p: &PluginInfo) -> ConsentReason {
    let has_new_permission = p
        .declared_permissions
        .iter()
        .any(|perm| !p.granted_permissions.contains(perm));
    // A non-empty granted set means the user approved this plugin before.
    let approved_before = !p.granted_permissions.is_empty();
    if approved_before && has_new_permission {
        ConsentReason::PermissionAdded
    } else if p.content_changed && !has_new_permission {
        ConsentReason::ContentChanged
    } else {
        ConsentReason::New
    }
}

/// The permissions `p` declares that haven't been granted yet — the ones an
/// update newly requires.
#[cfg(test)]
fn newly_required_permissions(p: &PluginInfo) -> Vec<halod_shared::types::Permission> {
    p.declared_permissions
        .iter()
        .filter(|perm| !p.granted_permissions.contains(perm))
        .copied()
        .collect()
}

/// True when the plugin is actually running: toggled on AND consent-satisfied
/// (a permissioned plugin needs its grant; a permission-free one always is).
fn plugin_active(p: &PluginInfo) -> bool {
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
    if plugin_active(p) {
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
    plugin_active(p) == target && !discovering
}

/// Drop landed (or vanished) in-flight toggles, unlocking them. Pure/testable.
fn reconcile_in_flight(
    in_flight: &mut HashMap<String, bool>,
    plugins: &[PluginInfo],
    discovering: bool,
) {
    in_flight.retain(|id, target| match plugins.iter().find(|p| &p.id == id) {
        Some(p) => !plugin_toggle_landed(p, *target, discovering),
        None => false,
    });
}

fn status_dot(p: &PluginInfo) -> egui::Color32 {
    if plugin_active(p) {
        theme::ONLINE
    } else {
        theme::TEXT_FAINT2
    }
}

fn list_row(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    selected: bool,
    needs_action: bool,
    logo_tex: Option<&egui::TextureHandle>,
    // `Some(target)` while applying: toggle shown at `target`, dimmed, click-blocked.
    locked_target: Option<bool>,
) -> RowAction {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 9.0, theme::ROW_ACTIVE);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 9.0, theme::a(theme::ROW_ACTIVE, 0.55));
    }
    let center_y = rect.center().y;

    let tile_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 8.0, center_y - 14.0),
        Vec2::splat(28.0),
    );
    match logo_tex {
        Some(tex) => draw_logo_fit(ui.painter(), tile_rect, tex),
        None => initials_tile_at(ui, tile_rect, &p.name, &p.id),
    }

    let text_x = tile_rect.right() + 10.0;
    ui.painter().text(
        Pos2::new(text_x, rect.top() + 9.0),
        Align2::LEFT_TOP,
        compact_plugin_name(&p.name),
        theme::semibold(12.5),
        theme::TEXT,
    );
    if !p.version.is_empty() {
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 27.0),
            Align2::LEFT_TOP,
            &p.version,
            theme::mono(9.5),
            theme::TEXT_FAINT,
        );
    }
    if needs_action {
        ui.painter().circle_filled(
            Pos2::new(tile_rect.right() - 2.0, tile_rect.top() + 2.0),
            4.0,
            theme::STAT_AMBER,
        );
    }
    if p.issue.is_some() {
        ui.painter().circle_filled(
            Pos2::new(tile_rect.left() + 2.0, tile_rect.top() + 2.0),
            4.0,
            theme::TRAFFIC_RED,
        );
    }
    let _ = status_dot(p); // kept for the toggle animation's initial state below

    // Toggle sits on top of the row; handle it before the row-select click.
    let toggle_rect = Rect::from_min_size(
        Pos2::new(rect.right() - 42.0, center_y - 9.0),
        Vec2::new(34.0, 18.0),
    );
    let locked = locked_target.is_some();
    let toggle_sense = if locked {
        Sense::hover()
    } else {
        Sense::click()
    };
    let tresp = ui.interact(
        toggle_rect,
        ui.id().with(("plugin_toggle", &p.id)),
        toggle_sense,
    );
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::PluginsToggle,
        toggle_rect,
    );
    let toggle_on = locked_target.unwrap_or_else(|| plugin_active(p));
    let t = ui.ctx().animate_bool_with_time(tresp.id, toggle_on, 0.15);
    widgets::paint_toggle(ui.painter(), toggle_rect, t);
    if locked {
        // Wash out to read as disabled.
        ui.painter()
            .rect_filled(toggle_rect, 9.0, theme::a(theme::CARD_BG, 0.55));
        if tresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
    } else if tresp.hovered() || resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if !locked && tresp.clicked() {
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

/// Deterministic background color for a colored-initials tile, from a small
/// hash of `id` — stable across reloads/reorders since it never depends on
/// list position.
fn initials_color(id: &str) -> egui::Color32 {
    const PALETTE: [egui::Color32; 6] = [
        theme::STAT_CYAN,
        theme::STAT_PURPLE,
        theme::STAT_GREEN,
        theme::STAT_AMBER,
        theme::CYAN,
        theme::TRAFFIC_GREEN,
    ];
    let hash = id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
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

/// A colored-initials tile at an already-allocated `rect` (for the list rows,
/// which lay out several elements on one hand-painted row).
pub(crate) fn initials_tile_at(ui: &mut egui::Ui, rect: Rect, name: &str, id: &str) {
    let color = initials_color(id);
    ui.painter().rect_filled(rect, 8.0, theme::a(color, 0.16));
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, theme::a(color, 0.5)),
        egui::StrokeKind::Middle,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initials_for(name),
        theme::mono_semibold(rect.height() * 0.34),
        color,
    );
}

/// A colored-initials tile that allocates its own `size`×`size` space (for
/// the detail column's header, where nothing else shares the row).
fn initials_tile(ui: &mut egui::Ui, name: &str, id: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    initials_tile_at(ui, rect, name, id);
}

#[allow(clippy::too_many_arguments)] // detail pane mutates independent modal/task state slots
fn detail_body(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    cmd: &CommandTx,
    logo_tex: Option<&egui::TextureHandle>,
    update: Option<&PluginUpdateStatus>,
    pending_delete: &mut Option<String>,
    pending_consent: &mut Option<String>,
    in_flight: &mut HashMap<String, bool>,
    updating: &mut HashMap<String, f64>,
    issue_modal: &mut Option<(String, String)>,
    now: f64,
) {
    egui::Sides::new().show(
        ui,
        |ui| {
            match logo_tex {
                Some(tex) => {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                    draw_logo_fit(ui.painter(), rect, tex);
                }
                None => initials_tile(ui, &p.name, &p.id, 44.0),
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&p.name)
                            .font(theme::bold(18.0))
                            .color(theme::TEXT),
                    );
                    if !p.version.is_empty() {
                        ui.label(
                            egui::RichText::new(&p.version)
                                .font(theme::mono(11.0))
                                .color(theme::TEXT_FAINT),
                        );
                    }
                });
                ui.label(
                    egui::RichText::new(&p.path)
                        .font(theme::mono(10.0))
                        .color(theme::TEXT_FAINT2),
                );
            });
        },
        |ui| {
            let _ = widgets::chip_colored(
                ui,
                &plugin_type_label(p.plugin_type),
                plugin_type_color(p.plugin_type),
            );
        },
    );

    if is_load_failed(p) {
        if let Some(issue) = &p.issue {
            ui.add_space(14.0);
            if issue_banner(ui, issue) {
                *issue_modal = Some((
                    t!("plugins.issue_modal_title", plugin = &p.name).to_string(),
                    issue.detail.clone(),
                ));
            }
        }
        // A failed plugin cannot expose its normal controls, but an upstream
        // update may be the only way to make it compatible again. Keep that
        // recovery action available even when the checked-out files also count
        // as modified on disk.
        if let Some(update) = update.filter(|u| u.update_available) {
            ui.add_space(14.0);
            let in_flight = updating.contains_key(&update.plugin_id);
            if update_banner(
                ui,
                &update.current_version,
                &update.available_version,
                in_flight,
            ) {
                updating.insert(update.plugin_id.clone(), now);
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::UpdatePluginRepo {
                        slug: update.slug.clone(),
                    },
                );
            }
        }
        return;
    }

    if plugin_requires_regrant(p) {
        ui.add_space(14.0);
        regrant_warning_banner(ui, p);
    }

    ui.add_space(14.0);
    status_banner(ui, p);

    if let Some(issue) = &p.issue {
        ui.add_space(14.0);
        if issue_banner(ui, issue) {
            *issue_modal = Some((
                t!("plugins.issue_modal_title", plugin = &p.name).to_string(),
                issue.detail.clone(),
            ));
        }
    }

    if !p.license.is_empty() || !matches!(p.source, PluginSource::Local) {
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if !p.license.is_empty() {
                widgets::chip(ui, &format!("⚖ {}", p.license));
            }
            if let PluginSource::Repo { slug } = &p.source {
                widgets::chip(ui, slug);
            }
        });
    }

    // A local on-disk edit is surfaced ahead of an upstream update: it's the
    // surprising, security-relevant state (the daemon has disabled the plugin).
    // Informational — the user re-enables it with the normal toggle, which
    // accepts the current content (and re-prompts consent if it declares perms).
    // Only while the plugin is held inactive: once re-enabled, the risk is
    // accepted and the banner is gone.
    if update.is_some_and(|u| u.on_disk_changed) && !plugin_active(p) {
        ui.add_space(14.0);
        modified_on_disk_banner(ui);
    } else if let Some(update) = update.filter(|u| u.update_available) {
        ui.add_space(14.0);
        let in_flight = updating.contains_key(&update.plugin_id);
        if update_banner(
            ui,
            &update.current_version,
            &update.available_version,
            in_flight,
        ) {
            updating.insert(update.plugin_id.clone(), now);
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::UpdatePluginRepo {
                    slug: update.slug.clone(),
                },
            );
        }
    }

    if !p.description.is_empty() {
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(&p.description)
                .font(theme::body(12.5))
                .color(theme::TEXT_DIM),
        );
    }

    if !p.author.is_empty() {
        ui.add_space(16.0);
        widgets::caps_label(ui, &t!("plugins.author"));
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(&p.author)
                .font(theme::body(12.5))
                .color(theme::TEXT),
        );
    }

    if p.plugin_type == halod_shared::types::PluginKind::Device && !p.capabilities.is_empty() {
        ui.add_space(16.0);
        widgets::caps_label(ui, &t!("plugins.capabilities"));
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for c in &p.capabilities {
                widgets::chip(ui, c);
            }
        });
    }

    if !p.effect_names.is_empty() {
        ui.add_space(16.0);
        widgets::caps_label(ui, &t!("plugins.effects"));
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for name in &p.effect_names {
                widgets::chip(ui, name);
            }
        });
    }

    targets_permissions_row(ui, p, cmd);

    ui.add_space(20.0);
    ui.separator();
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        let active = plugin_active(p);
        let locked = in_flight.contains_key(&p.id);
        let label = if active {
            t!("plugins.disable")
        } else {
            t!("plugins.enable")
        };
        let kind = if active {
            ButtonKind::Ghost
        } else {
            ButtonKind::Primary
        };
        let clicked = ui
            .add_enabled_ui(!locked, |ui| {
                widgets::button(ui, &label, kind, Vec2::new(120.0, 34.0)).clicked()
            })
            .inner;
        if clicked {
            let out = request_toggle(cmd, p, pending_consent.take());
            *pending_consent = out.pending_consent;
            if let Some(target) = out.dispatched {
                apply_and_lock(in_flight, &p.id, target);
            }
        }
        if matches!(p.source, halod_shared::types::PluginSource::Local) {
            if widgets::button(
                ui,
                &t!("plugins.delete"),
                ButtonKind::Danger,
                Vec2::new(120.0, 34.0),
            )
            .clicked()
            {
                *pending_delete = Some(p.id.clone());
            }
        } else {
            widgets::caps_label_inline(ui, &t!("plugins.repo_sourced_note"));
        }
    });
}

fn plugin_type_label(kind: halod_shared::types::PluginKind) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::PluginKind;
    match kind {
        PluginKind::Device => t!("plugins.type_device"),
        PluginKind::Effect => t!("plugins.type_effect"),
        PluginKind::Integration => t!("plugins.type_integration"),
    }
}

fn plugin_type_color(kind: halod_shared::types::PluginKind) -> egui::Color32 {
    use halod_shared::types::PluginKind;
    match kind {
        PluginKind::Device => theme::STAT_CYAN,
        PluginKind::Effect => theme::STAT_PURPLE,
        PluginKind::Integration => theme::STAT_GREEN,
    }
}

fn permission_label(perm: halod_shared::types::Permission) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Hid => "HID device access".into(),
        Permission::Hwmon => "Hardware monitoring".into(),
        Permission::Lpcio => "LPC/SuperIO access".into(),
        Permission::AmdSmn => "AMD SMN access".into(),
        Permission::Command => "Run approved commands".into(),
        Permission::Network => t!("plugins.permission_network"),
        Permission::Os => t!("plugins.permission_os"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage"),
        Permission::Smbus => t!("plugins.permission_smbus"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing"),
    }
}

/// One line explaining what a permission lets the plugin do — and the risk —
/// so the user can make an informed grant decision.
fn permission_description(perm: halod_shared::types::Permission) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Hid => "Lets the plugin communicate with the matching HID device and receive its input reports.".into(),
        Permission::Hwmon => "Lets the plugin access the selected Linux hardware-monitoring device, including approved fan controls.".into(),
        Permission::Lpcio => "Lets the plugin use the typed broker interface for motherboard SuperIO hardware.".into(),
        Permission::AmdSmn => "Lets the plugin read supported AMD SMN registers through the broker.".into(),
        Permission::Command => "Lets the plugin run only the executable names declared in its manifest, without a shell.".into(),
        Permission::Network => t!("plugins.permission_network_desc"),
        Permission::Os => t!("plugins.permission_os_desc"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage_desc"),
        Permission::Smbus => t!("plugins.permission_smbus_desc"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing_desc"),
    }
}

/// A colored dot glyph, laid out inline.
fn dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 3.0, color);
}

/// One permission as a bullet: a colored dot + mono label, then its
/// explanation on the next line. `color` marks granted (green) vs requested
/// (amber) vs faint.
fn permission_bullet(
    ui: &mut egui::Ui,
    perm: halod_shared::types::Permission,
    color: egui::Color32,
) {
    ui.horizontal(|ui| {
        dot(ui, color);
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(permission_label(perm))
                .font(theme::mono(11.0))
                .color(theme::TEXT),
        );
    });
    ui.label(
        egui::RichText::new(permission_description(perm))
            .font(theme::body(11.0))
            .color(theme::TEXT_MUT),
    );
}

/// One permission as a full-width dark card for the grant modal: an amber dot +
/// mono label, then its explanation.
fn permission_card(ui: &mut egui::Ui, perm: halod_shared::types::Permission) {
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                dot(ui, theme::STAT_AMBER);
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(permission_label(perm))
                        .font(theme::mono(12.0))
                        .color(theme::TEXT),
                );
            });
            ui.add_space(3.0);
            ui.label(
                egui::RichText::new(permission_description(perm))
                    .font(theme::body(11.5))
                    .color(theme::TEXT_MUT),
            );
        });
}

/// Side-by-side "TARGET DEVICES | PERMISSIONS" row of the detail view. Either
/// column is omitted when empty.
fn targets_permissions_row(ui: &mut egui::Ui, p: &PluginInfo, cmd: &CommandTx) {
    let has_targets =
        p.plugin_type == halod_shared::types::PluginKind::Device && !p.targets.is_empty();
    let has_perms = !p.declared_permissions.is_empty();
    if !has_targets && !has_perms {
        return;
    }
    ui.add_space(16.0);
    ui.columns(2, |cols| {
        if has_targets {
            widgets::caps_label(&mut cols[0], &t!("plugins.targets"));
            cols[0].add_space(6.0);
            for target in &p.targets {
                cols[0].label(
                    egui::RichText::new(target)
                        .font(theme::body(12.0))
                        .color(theme::TEXT_DIM),
                );
            }
        }
        if has_perms {
            permissions_section(&mut cols[1], p, cmd);
        }
    });
}

/// Declared permissions as a bulleted list with a granted/not-granted marker,
/// plus a Revoke control once granted. Grants happen through the enable-time
/// consent modal, not here. Shared with the Integrations screen.
pub(crate) fn permissions_section(ui: &mut egui::Ui, p: &PluginInfo, cmd: &CommandTx) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::PluginsPermissions,
        ui.max_rect(),
    );
    ui.horizontal(|ui| {
        widgets::caps_label_inline(ui, &t!("plugins.permissions"));
        let (text, color) = if p.consented {
            (t!("plugins.permissions_granted"), theme::ONLINE_TEXT)
        } else {
            (t!("plugins.permissions_not_granted_tag"), theme::STAT_AMBER)
        };
        ui.label(
            egui::RichText::new(text)
                .font(theme::body(11.0))
                .color(color),
        );
    });
    ui.add_space(6.0);

    for perm in &p.declared_permissions {
        let color = if p.granted_permissions.contains(perm) {
            theme::ONLINE
        } else {
            theme::STAT_AMBER
        };
        permission_bullet(ui, *perm, color);
        ui.add_space(6.0);
    }

    if !p.consented {
        if p.content_changed {
            ui.label(
                egui::RichText::new(t!("plugins.consent_modified"))
                    .font(theme::body(11.0))
                    .color(theme::STAT_AMBER),
            );
        } else {
            ui.label(
                egui::RichText::new(t!("plugins.permissions_enable_hint"))
                    .font(theme::body(11.0))
                    .color(theme::TEXT_MUT),
            );
        }
    } else if widgets::button(
        ui,
        &t!("plugins.revoke"),
        ButtonKind::Ghost,
        Vec2::new(150.0, 28.0),
    )
    .clicked()
    {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::SetPluginEnabled {
                id: p.id.clone(),
                enabled: false,
            },
        );
    }
}

fn status_banner(ui: &mut egui::Ui, p: &PluginInfo) {
    let (dot, text, color) = if plugin_active(p) {
        (
            theme::ONLINE,
            t!("plugins.status_active"),
            theme::ONLINE_TEXT,
        )
    } else {
        (
            theme::TEXT_FAINT2,
            t!("plugins.status_disabled"),
            theme::TEXT_MUT,
        )
    };
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, dot);
                ui.label(
                    egui::RichText::new(text)
                        .font(theme::body(12.0))
                        .color(color),
                );
            });
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
    let mut clicked = false;
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            // Match the band height to the button so the text centers against it.
            egui::Sides::new().height(32.0).show(
                ui,
                |ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter()
                        .circle_filled(r.center(), 3.5, theme::STAT_AMBER);
                    ui.label(
                        egui::RichText::new(t!(updates_available_key(count), count = count))
                            .font(theme::body(12.5))
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    let size = Vec2::new(120.0, 32.0);
                    if updating {
                        widgets::button_loading(
                            ui,
                            &t!("plugins.updating"),
                            ButtonKind::Warn,
                            size,
                        );
                    } else if widgets::button(ui, &t!("plugins.update_all"), ButtonKind::Warn, size)
                        .clicked()
                    {
                        clicked = true;
                    }
                },
            );
        });
    clicked
}

/// Amber "Update available vX → vY / Update" banner in a plugin's detail. Never
/// automatic.
/// Banner shown when a plugin's on-disk content differs from its checked-out
/// baseline — a local edit or tampering. The daemon has disabled the plugin for
/// safety; this explains why and how to recover. Purely informational: the user
/// re-enables it with the normal toggle, which accepts the current content.
fn modified_on_disk_banner(ui: &mut egui::Ui) {
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(t!("plugins.modified_on_disk"))
                    .font(theme::semibold(12.0))
                    .color(theme::STAT_AMBER),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(t!("plugins.modified_on_disk_sub"))
                    .font(theme::body(11.5))
                    .color(theme::TEXT_DIM),
            );
        });
}

/// Prominent recovery hint for a plugin disabled after its previously approved
/// content or permission set changed. The row dot gets the user here; this
/// banner makes the required action explicit before the normal detail content.
fn regrant_warning_banner(ui: &mut egui::Ui, p: &PluginInfo) {
    let detail = match consent_reason(p) {
        ConsentReason::PermissionAdded => t!("plugins.consent_permission_added"),
        ConsentReason::ContentChanged | ConsentReason::New => t!("plugins.consent_modified"),
    };
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(t!("plugins.regrant_required"))
                    .font(theme::semibold(12.0))
                    .color(theme::STAT_AMBER),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(detail)
                    .font(theme::body(11.5))
                    .color(theme::TEXT_DIM),
            );
        });
}

/// The i18n label key for a plugin issue banner, by kind.
fn issue_label_key(kind: &PluginIssueKind) -> &'static str {
    match kind {
        PluginIssueKind::ConnectFailed => "plugins.issue_connect_failed",
        PluginIssueKind::RuntimeError => "plugins.issue_runtime_error",
        PluginIssueKind::LoadWarning => "plugins.issue_load_warning",
        PluginIssueKind::LoadFailed => "plugins.issue_load_failed",
    }
}

fn is_load_failed(p: &PluginInfo) -> bool {
    p.issue
        .as_ref()
        .is_some_and(|i| i.kind == PluginIssueKind::LoadFailed)
}

/// A per-plugin issue banner with a "Details" button; returns `true` when
/// Details is clicked. Load warnings tint amber, connect/runtime errors red.
fn issue_banner(ui: &mut egui::Ui, issue: &PluginIssue) -> bool {
    let accent = match issue.kind {
        PluginIssueKind::LoadWarning => theme::STAT_AMBER,
        _ => theme::TRAFFIC_RED,
    };
    let mut clicked = false;
    egui::Frame::NONE
        .fill(theme::a(accent, 0.10))
        .stroke(Stroke::new(1.0, theme::a(accent, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            egui::Sides::new().height(30.0).show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!(issue_label_key(&issue.kind)))
                            .font(theme::semibold(12.0))
                            .color(accent),
                    );
                },
                |ui| {
                    if widgets::button(
                        ui,
                        &t!("plugins.issue_details"),
                        ButtonKind::Ghost,
                        Vec2::new(90.0, 30.0),
                    )
                    .clicked()
                    {
                        clicked = true;
                    }
                },
            );
        });
    clicked
}

fn update_banner(ui: &mut egui::Ui, current: &str, available: &str, updating: bool) -> bool {
    let mut clicked = false;
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            egui::Sides::new().height(30.0).show(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(t!("plugins.update_available"))
                                .font(theme::body(12.0))
                                .color(theme::TEXT),
                        );
                        if !current.is_empty() || !available.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("{current} → {available}"))
                                    .font(theme::mono(11.0))
                                    .color(theme::STAT_AMBER),
                            );
                        }
                    });
                },
                |ui| {
                    let size = Vec2::new(90.0, 30.0);
                    if updating {
                        widgets::button_loading(
                            ui,
                            &t!("plugins.updating"),
                            ButtonKind::Warn,
                            size,
                        );
                    } else if widgets::button(
                        ui,
                        &t!("plugins.repos_update"),
                        ButtonKind::Warn,
                        size,
                    )
                    .clicked()
                    {
                        clicked = true;
                    }
                },
            );
        });
    clicked
}

/// `last_sync` is stored as an RFC3339 timestamp. Keep the stat compact by
/// showing a relative age (the exact value remains available in the source
/// data and can be inspected in diagnostics).
fn format_last_sync(raw: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.to_owned();
    };
    format_last_sync_at(parsed.with_timezone(&chrono::Utc), chrono::Utc::now())
}

fn format_last_sync_at(
    parsed: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let seconds = (now - parsed).num_seconds().max(0);
    if seconds < 60 {
        t!("plugins.repo_sync_just_now").to_string()
    } else if seconds < 3_600 {
        t!("plugins.repo_sync_minutes", count = seconds / 60).to_string()
    } else if seconds < 86_400 {
        t!("plugins.repo_sync_hours", count = seconds / 3_600).to_string()
    } else {
        t!("plugins.repo_sync_days", count = seconds / 86_400).to_string()
    }
}

/// A repo's stat box (SOURCE / LAST SYNC / DRIVERS).
fn stat_box(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::caps_label(ui, label);
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(value)
                    .font(theme::body(12.5))
                    .color(theme::TEXT),
            );
        });
}

/// Empty-state panel under "Drivers from this repository" when the repo has
/// contributed no plugins yet: a dashed border box with centered muted text.
fn repo_no_drivers_box(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
    let stroke = Stroke::new(1.0, theme::BORDER);
    let r = rect.shrink(0.5);
    let corners = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    for seg in corners.windows(2) {
        ui.painter()
            .extend(egui::Shape::dashed_line(seg, stroke, 6.0, 4.0));
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        t!("plugins.repo_no_drivers"),
        theme::body(12.0),
        theme::TEXT_MUT,
    );
}

/// Repository update checks only run when the user has allowed the daemon to
/// contact GitHub; any other consent state (unset or denied) leaves them off.
fn plugin_updates_enabled(consent: PluginDownloadConsent) -> bool {
    matches!(consent, PluginDownloadConsent::Allowed)
}

/// A muted info band explaining that update checks are turned off, shown above
/// the greyed-out "Check for updates" button when downloads are not allowed.
fn updates_disabled_note(ui: &mut egui::Ui) {
    egui::Frame::NONE
        .fill(theme::a(theme::TEXT_MUT, 0.08))
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 3.5, theme::TEXT_FAINT);
                ui.label(
                    egui::RichText::new(t!("plugins.repos_updates_disabled"))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_MUT),
                );
            });
        });
}

/// The repo detail panel: header, stat boxes, "Check for updates", the list
/// of plugins it provides, and Remove (hidden for the official repo). Returns
/// a clicked plugin id, if any, so the caller can switch the selection to it.
fn repo_detail_body(
    ui: &mut egui::Ui,
    r: &PluginRepoInfo,
    plugins: &[PluginInfo],
    pending_repo_delete: &mut Option<String>,
    checking: bool,
    updates_enabled: bool,
    start_check: &mut bool,
) -> Option<String> {
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                repo_icon_tile(ui, 44.0);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&r.slug)
                                .font(theme::bold(18.0))
                                .color(theme::TEXT),
                        );
                        if let Some(branch) = &r.branch {
                            widgets::chip(ui, branch);
                        }
                    });
                    let url = ui.add(
                        egui::Label::new(
                            egui::RichText::new(&r.url)
                                .font(theme::mono(10.0))
                                .color(theme::CYAN)
                                .underline(),
                        )
                        .sense(Sense::click()),
                    );
                    if url.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if url.clicked() {
                        ui.ctx().open_url(egui::OpenUrl {
                            url: r.url.clone(),
                            new_tab: true,
                        });
                    }
                });
            });
        },
        |_ui| {},
    );

    ui.add_space(16.0);
    let repo_plugins: Vec<&PluginInfo> = plugins
        .iter()
        .filter(|p| matches!(&p.source, PluginSource::Repo { slug } if *slug == r.slug))
        .collect();
    ui.columns(3, |cols| {
        stat_box(&mut cols[0], &t!("plugins.repo_source"), "Git remote");
        stat_box(
            &mut cols[1],
            &t!("plugins.repo_last_sync"),
            &r.last_sync
                .as_deref()
                .map(format_last_sync)
                .unwrap_or_else(|| t!("plugins.repo_never_synced").to_string()),
        );
        stat_box(
            &mut cols[2],
            &t!("plugins.repo_drivers"),
            &t!("plugins.repo_drivers_count", count = repo_plugins.len()),
        );
    });

    ui.add_space(14.0);
    if !updates_enabled {
        updates_disabled_note(ui);
        ui.add_space(10.0);
        widgets::button_disabled(
            ui,
            &t!("plugins.repos_check_updates"),
            ButtonKind::Primary,
            Vec2::new(180.0, 32.0),
        );
    } else if checking {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(18.0).color(theme::CYAN));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(t!("plugins.repos_checking"))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_MUT),
            );
        });
    } else if widgets::button(
        ui,
        &t!("plugins.repos_check_updates"),
        ButtonKind::Primary,
        Vec2::new(180.0, 32.0),
    )
    .clicked()
    {
        *start_check = true;
    }

    ui.add_space(20.0);
    widgets::caps_label(ui, &t!("plugins.repo_drivers_from"));
    ui.add_space(8.0);

    if repo_plugins.is_empty() {
        repo_no_drivers_box(ui);
    }

    let mut clicked = None;
    for p in &repo_plugins {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 8.0, theme::a(theme::ROW_ACTIVE, 0.55));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let tile_rect = Rect::from_min_size(
            Pos2::new(rect.left() + 6.0, rect.center().y - 12.0),
            Vec2::splat(24.0),
        );
        initials_tile_at(ui, tile_rect, &p.name, &p.id);
        let text_x = tile_rect.right() + 10.0;
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 6.0),
            Align2::LEFT_TOP,
            &p.name,
            theme::semibold(12.0),
            theme::TEXT,
        );
        let sub = match (p.version.is_empty(), p.license.is_empty()) {
            (false, false) => format!("{} · {}", p.version, p.license),
            (false, true) => p.version.clone(),
            (true, false) => p.license.clone(),
            (true, true) => String::new(),
        };
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 22.0),
            Align2::LEFT_TOP,
            sub,
            theme::mono(9.5),
            theme::TEXT_FAINT,
        );
        ui.painter().circle_filled(
            Pos2::new(rect.right() - 12.0, rect.center().y),
            3.5,
            status_dot(p),
        );
        if resp.clicked() {
            clicked = Some(p.id.clone());
        }
    }

    if !r.official {
        ui.add_space(20.0);
        ui.separator();
        ui.add_space(14.0);
        if widgets::button(
            ui,
            &t!("plugins.repos_remove"),
            ButtonKind::Danger,
            Vec2::new(150.0, 34.0),
        )
        .clicked()
        {
            *pending_repo_delete = Some(r.slug.clone());
        }
    }

    clicked
}

// ── Add-plugin modal body ───────────────────────────────────────────────────

fn add_body(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new(t!("plugins.add_sub"))
            .font(theme::body(11.5))
            .color(theme::TEXT_MUT),
    );
    ui.add_space(14.0);

    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(20, 26))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.upload_hint"))
                        .font(theme::body(12.5))
                        .color(theme::TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(t!("plugins.upload_sub"))
                        .font(theme::body(11.0))
                        .color(theme::TEXT_MUT),
                );
            });
        });
}

/// Open a native folder picker on a background thread and send an import
/// command for the chosen plugin package directory straight from the thread
/// (the command channel is cheap to clone). Mirrors `effect_designer::spawn_import`.
fn spawn_import_plugin(ctx: &egui::Context, cmd: CommandTx) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            let source_dir = path.to_string_lossy().into_owned();
            crate::runtime::ipc::send(
                &cmd,
                halod_shared::commands::DaemonCommand::ImportPlugin { source_dir },
            );
        }
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: &str, update_available: bool) -> PluginUpdateStatus {
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
    fn plugin_updates_enabled_only_when_downloads_are_allowed() {
        assert!(plugin_updates_enabled(PluginDownloadConsent::Allowed));
        assert!(!plugin_updates_enabled(PluginDownloadConsent::Denied));
        assert!(!plugin_updates_enabled(PluginDownloadConsent::Unset));
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
    fn format_last_sync_trims_fractional_seconds_and_offset() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-11T23:46:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let parsed = chrono::DateTime::parse_from_rfc3339("2026-07-11T23:44:00.021314418+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(format_last_sync_at(parsed, now), "1m ago");
    }

    #[test]
    fn format_last_sync_handles_whole_seconds_without_fraction() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:44:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let parsed = chrono::DateTime::parse_from_rfc3339("2026-07-11T23:44:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(format_last_sync_at(parsed, now), "1h ago");
    }

    #[test]
    fn branch_fetch_due_waits_for_the_debounce_deadline() {
        let form = AddRepoState {
            url: "https://example.com/repo.git".into(),
            fetch_at: Some(10.0),
            ..Default::default()
        };
        assert_eq!(branch_fetch_due(&form, 9.9), None, "before the deadline");
        assert_eq!(
            branch_fetch_due(&form, 10.0),
            Some("https://example.com/repo.git".to_owned()),
            "at the deadline"
        );
    }

    #[test]
    fn branch_fetch_due_none_without_a_deadline_or_for_an_empty_or_repeat_url() {
        let url = "https://example.com/repo.git";
        // No armed deadline.
        assert_eq!(
            branch_fetch_due(
                &AddRepoState {
                    url: url.into(),
                    fetch_at: None,
                    ..Default::default()
                },
                5.0
            ),
            None
        );
        // Blank URL.
        assert_eq!(
            branch_fetch_due(
                &AddRepoState {
                    url: "   ".into(),
                    fetch_at: Some(1.0),
                    ..Default::default()
                },
                5.0
            ),
            None
        );
        // Already fetched this exact URL.
        assert_eq!(
            branch_fetch_due(
                &AddRepoState {
                    url: url.into(),
                    fetch_at: Some(1.0),
                    fetched_url: Some(url.into()),
                    ..Default::default()
                },
                5.0
            ),
            None
        );
    }

    #[test]
    fn branch_options_maps_each_name_to_an_identical_id_and_display() {
        let opts = branch_options(&["main".to_owned(), "dev".to_owned()]);
        assert_eq!(
            opts,
            vec![
                ("main".to_owned(), "main".to_owned()),
                ("dev".to_owned(), "dev".to_owned()),
            ]
        );
    }

    fn info(id: &str, enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} device"),
            path: format!("/home/u/.config/halod/plugins/{id}.lua"),
            plugin_type: halod_shared::types::PluginKind::Device,
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
            accepted_authority: None,
            granted_permissions: vec![],
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
            integration_enabled: true,
            consented: true,
            content_changed: false,
            issue: None,
            integration_issue: None,
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
        p.issue = Some(PluginIssue {
            kind: PluginIssueKind::RuntimeError,
            detail: "x".into(),
            timestamp_ms: 0,
        });
        assert!(!is_load_failed(&p));
        p.issue = Some(PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "x".into(),
            timestamp_ms: 0,
        });
        assert!(is_load_failed(&p));
    }

    #[test]
    fn consent_reason_classifies_new_added_and_changed() {
        use halod_shared::types::Permission;

        // Never approved: a first-time grant.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network];
        p.granted_permissions = vec![];
        assert_eq!(consent_reason(&p), ConsentReason::New);

        // Approved before (has a granted perm) and an update declares a new one.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network, Permission::Os];
        p.granted_permissions = vec![Permission::Network];
        p.content_changed = true;
        assert_eq!(consent_reason(&p), ConsentReason::PermissionAdded);
        assert_eq!(newly_required_permissions(&p), vec![Permission::Os]);

        // Approved before, content changed, but no new permission.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network];
        p.granted_permissions = vec![Permission::Network];
        p.content_changed = true;
        assert_eq!(consent_reason(&p), ConsentReason::ContentChanged);
        assert!(newly_required_permissions(&p).is_empty());
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

    #[test]
    fn logo_fit_rect_fills_square_and_centers() {
        let into = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::splat(40.0));
        // A square logo fills the tile exactly.
        assert_eq!(logo_fit_rect(Vec2::splat(64.0), into), into);
        // A wide logo keeps its aspect: full width, centered vertically.
        let wide = logo_fit_rect(Vec2::new(100.0, 50.0), into);
        assert_eq!(wide.width(), 40.0);
        assert_eq!(wide.height(), 20.0);
        assert_eq!(wide.center(), into.center());
        // A degenerate size falls back to the whole tile rather than dividing by zero.
        assert_eq!(logo_fit_rect(Vec2::ZERO, into), into);
    }

    fn repo(slug: &str, locked_sha: &str) -> PluginRepoInfo {
        PluginRepoInfo {
            url: format!("https://example.com/{slug}.git"),
            slug: slug.to_owned(),
            repository_id: None,
            branch: None,
            locked_sha: locked_sha.to_owned(),
            active_revision: None,
            previous_verified_sha: None,
            last_sync: None,
            official: false,
        }
    }

    #[test]
    fn truncate_sha_shortens_a_full_hash_and_passes_through_a_short_one() {
        assert_eq!(truncate_sha("0123456789abcdef"), "01234567");
        assert_eq!(truncate_sha("abc"), "abc");
    }

    #[test]
    fn repo_rows_sorts_by_slug_and_marks_up_to_date_repos_unbehind() {
        let repos = vec![repo("zebra", "aaaaaaaa1111"), repo("alpha", "bbbbbbbb2222")];
        let rows = repo_rows(&repos, &[]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].slug, "alpha");
        assert_eq!(rows[1].slug, "zebra");
        assert!(!rows[0].behind);
        assert!(rows[0].remote_short.is_none());
        assert_eq!(rows[0].locked_short, "bbbbbbbb");
    }

    #[test]
    fn repo_rows_surfaces_the_remote_sha_only_when_behind() {
        let repos = vec![repo("foo", "aaaaaaaa")];
        let up_to_date = [RepoUpdateStatus {
            slug: "foo".into(),
            locked_sha: "aaaaaaaa".into(),
            remote_sha: "aaaaaaaa".into(),
            behind: false,
        }];
        let rows = repo_rows(&repos, &up_to_date);
        assert!(!rows[0].behind);
        assert!(rows[0].remote_short.is_none());

        let behind = [RepoUpdateStatus {
            slug: "foo".into(),
            locked_sha: "aaaaaaaa".into(),
            remote_sha: "cccccccc9999".into(),
            behind: true,
        }];
        let rows = repo_rows(&repos, &behind);
        assert!(rows[0].behind);
        assert_eq!(rows[0].remote_short.as_deref(), Some("cccccccc"));
    }

    #[test]
    fn repo_rows_puts_the_official_repo_first_regardless_of_slug() {
        let mut official = repo("aaa-not-alphabetically-first", "aaaaaaaa");
        official.official = true;
        let repos = vec![repo("alpha", "bbbbbbbb"), official];
        let rows = repo_rows(&repos, &[]);
        assert!(rows[0].official, "the official repo must sort first");
        assert_eq!(rows[1].slug, "alpha");
    }

    #[test]
    fn initials_for_takes_first_letter_of_first_two_words() {
        assert_eq!(initials_for("WLED UDP"), "WU");
        assert_eq!(initials_for("kraken"), "K");
        assert_eq!(initials_for(""), "?");
        assert_eq!(initials_for("  "), "?");
    }

    #[test]
    fn initials_color_is_deterministic_and_not_constant() {
        assert_eq!(initials_color("wled_udp"), initials_color("wled_udp"));
        // Not every id needs a different color, but the derivation must be
        // sensitive to the id (not just returning the same palette entry).
        let colors: std::collections::HashSet<_> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|id| initials_color(id).to_srgba_unmultiplied())
            .collect();
        assert!(
            colors.len() > 1,
            "distinct ids must not all collapse to one color"
        );
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
    fn file_name_is_basename() {
        assert_eq!(plugin_file_name(&info("kraken", true)), "kraken.lua");
        let mut p = info("x", true);
        p.path = "ene_smbus.lua".into();
        assert_eq!(plugin_file_name(&p), "ene_smbus.lua");
    }

    #[test]
    fn status_dot_reflects_enabled() {
        assert_eq!(status_dot(&info("a", true)), theme::ONLINE);
        assert_eq!(status_dot(&info("a", false)), theme::TEXT_FAINT2);
    }

    #[test]
    fn needs_permission_only_when_declaring_ungranted_permissions() {
        use halod_shared::types::Permission;
        // No declared permissions → never needs permission (runs freely).
        assert!(!plugin_needs_permission(&info("a", true)));

        // Declares a permission but the daemon reports it unconsented.
        let mut p = info("a", true);
        p.declared_permissions = vec![Permission::Network];
        p.consented = false;
        assert!(plugin_needs_permission(&p));

        // Consented → satisfied.
        p.consented = true;
        assert!(!plugin_needs_permission(&p));
    }

    #[test]
    fn regrant_attention_only_marks_previously_approved_plugins() {
        let mut updated = info("updated", false);
        updated.declared_permissions = vec![halod_shared::types::Permission::Os];
        updated.granted_permissions = vec![halod_shared::types::Permission::Os];
        updated.consented = false;
        updated.content_changed = true;
        assert!(plugin_requires_regrant(&updated));

        let mut added_permission = info("added-permission", false);
        added_permission.declared_permissions = vec![
            halod_shared::types::Permission::Os,
            halod_shared::types::Permission::Network,
        ];
        added_permission.granted_permissions = vec![halod_shared::types::Permission::Os];
        added_permission.consented = false;
        assert!(plugin_requires_regrant(&added_permission));

        let mut first_install = info("first-install", false);
        first_install.declared_permissions = vec![halod_shared::types::Permission::Network];
        first_install.consented = false;
        assert!(!plugin_requires_regrant(&first_install));
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
    fn plugin_active_requires_enabled_and_consented() {
        let mut p = info("a", true);
        assert!(plugin_active(&p));
        p.consented = false;
        assert!(!plugin_active(&p), "unconsented is inert even if enabled");
        p.consented = true;
        p.enabled = false;
        assert!(!plugin_active(&p), "disabled is not active");
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
        assert_ne!(
            plugin_type_color(PluginKind::Device),
            plugin_type_color(PluginKind::Effect)
        );
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

impl PluginsUi {
    pub(super) fn repositories(&mut self, ui: &mut egui::Ui, cx: &PluginsCtx) {
        let now = ui.input(|i| i.time);
        self.reconcile_repo_check(&cx.state.plugins.repos, now);
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.repos_title"))
                        .font(theme::semibold(15.0))
                        .color(theme::TEXT),
                );
            },
            |ui| {
                let add_repo_resp = widgets::button(
                    ui,
                    &t!("plugins.repos_add_title"),
                    ButtonKind::Secondary,
                    Vec2::new(120.0, 30.0),
                );
                Self::anchor_add_plugin_source(ui.ctx(), add_repo_resp.rect);
                if add_repo_resp.clicked() {
                    self.add_repo = Some(AddRepoState::default());
                }
            },
        );
        ui.add_space(theme::SPACE_4);

        let rows = repo_rows(
            &cx.state.plugins.repos,
            cx.repo_updates(),
            &cx.state.plugins.plugins,
        );
        if rows.is_empty() {
            ui.label(
                egui::RichText::new(t!("plugins.repos_empty"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
            return;
        }
        let updates_enabled = plugin_updates_enabled(cx.state.gui.plugin_downloads);
        for row in rows {
            let selected = self.selection == Selection::Repo(row.slug.to_owned());
            let checking = self
                .checking_repo
                .as_ref()
                .is_some_and(|c| c.slug == row.slug);
            match repo_row(ui, &row, selected, updates_enabled, checking) {
                RepoRowAction::Select => self.selection = Selection::Repo(row.slug.to_owned()),
                RepoRowAction::Check => {
                    if let Some(r) = cx.state.plugins.repos.iter().find(|r| r.slug == row.slug) {
                        self.apply_repo_action(now, cx.cmd, r, RepoAction::Check);
                    }
                }
                RepoRowAction::None => {}
            }
        }
    }

    /// Drop the check spinner once the check lands (last_sync advanced) or
    /// times out.
    fn reconcile_repo_check(&mut self, repos: &[PluginRepoInfo], now: f64) {
        if let Some(c) = &self.checking_repo {
            let landed = repos
                .iter()
                .any(|r| r.slug == c.slug && r.last_sync != c.prev_sync);
            if landed || now - c.started > REPO_CHECK_TIMEOUT {
                self.checking_repo = None;
            }
        }
    }

    /// The repository-source button is covered by two consecutive tour steps:
    /// first as the way to add a plugin source, then specifically as the way to
    /// add another repository. Register both anchors so neither step is treated
    /// as missing and silently skipped.
    pub(super) fn anchor_add_plugin_source(ctx: &egui::Context, rect: Rect) {
        crate::domain::tour::anchor(ctx, crate::domain::tour::AnchorId::PluginsAddPlugin, rect);
        crate::domain::tour::anchor(ctx, crate::domain::tour::AnchorId::PluginsAddRepo, rect);
    }

    pub(super) fn skipped_notice(&mut self, ui: &mut egui::Ui, state: &TopicStore) {
        if state.plugins.skipped.is_empty() {
            return;
        }
        ui.add_space(theme::SPACE_9);
        widgets::section_label_inline(ui, &t!("plugins.skipped_heading"));
        ui.add_space(theme::SPACE_3);
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
                                .font(theme::body_md())
                                .color(theme::TEXT_DIM),
                        );
                    });
                },
                |ui| {
                    if let Some((slug, _)) = skipped_repo_location(&s.path, &state.plugins.repos) {
                        if let Some(repo) = state.plugins.repos.iter().find(|r| r.slug == slug) {
                            let verified = repo.signature == RepoSignatureStatus::Verified;
                            let label = if verified {
                                t!("plugins.repos_repair")
                            } else {
                                t!("plugins.repos_remove_reimport")
                            };
                            if widgets::button(
                                ui,
                                &label,
                                ButtonKind::Warn,
                                Vec2::new(if verified { 86.0 } else { 138.0 }, 26.0),
                            )
                            .clicked()
                            {
                                if verified {
                                    self.pending_repo_repair = Some(PendingRepoRepair { slug });
                                } else {
                                    self.pending_repo_delete = Some(slug);
                                }
                            }
                        }
                        ui.add_space(theme::SPACE_3);
                    }
                    if widgets::button(
                        ui,
                        &t!("plugins.issue_details"),
                        ButtonKind::Secondary,
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
            ui.add_space(theme::SPACE_2);
        }
    }

    // ── Right: detail ───────────────────────────────────────────────────────

    pub(super) fn repo_detail(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PluginsCtx,
        r: &PluginRepoInfo,
    ) -> Option<RepoAction> {
        if r.location == PluginRepoLocation::RemoteRelease
            && self.requested_releases.insert(r.url.clone())
        {
            ipc::send(
                cx.cmd,
                DaemonCommand::ListPluginReleases { url: r.url.clone() },
            );
        }
        let status = cx.repo_updates().iter().find(|s| s.slug == r.slug);
        let view = RepoView {
            checking: self
                .checking_repo
                .as_ref()
                .is_some_and(|c| c.slug == r.slug),
            updates_enabled: plugin_updates_enabled(cx.state.gui.plugin_downloads),
            has_updates: status.is_some_and(|s| !s.pinned && s.available_tag.is_some()),
            pinned_update: status
                .filter(|s| s.pinned)
                .and_then(|s| s.available_tag.as_deref()),
            updating: self.updating_repos.contains_key(&r.slug),
        };
        if view.checking || view.updating {
            // Keep animating the spinner and advancing the timeout.
            ui.ctx().request_repaint();
        }
        let releases = cx
            .releases
            .get(&r.url)
            .map(Vec::as_slice)
            .unwrap_or_default();
        repo_detail_body(ui, r, cx.cmd, releases, &cx.state.plugins.plugins, &view)
    }

    pub(super) fn apply_repo_action(
        &mut self,
        now: f64,
        cmd: &CommandTx,
        r: &PluginRepoInfo,
        action: RepoAction,
    ) {
        let slug = r.slug.clone();
        match action {
            RepoAction::SelectPlugin(id) => self.selection = Selection::Plugin(id),
            RepoAction::Check => {
                self.checking_repo = Some(RepoCheck {
                    slug,
                    prev_sync: r.last_sync.clone(),
                    started: now,
                });
                ipc::send(cmd, DaemonCommand::CheckPluginRepoUpdates);
            }
            RepoAction::Update => {
                self.updating_repos.insert(slug.clone(), now);
                ipc::send(cmd, DaemonCommand::UpdatePluginRepo { slug });
            }
            RepoAction::MovePin(tag) => {
                self.updating_repos.insert(slug.clone(), now);
                ipc::send(cmd, DaemonCommand::InstallPluginRelease { slug, tag });
            }
            RepoAction::Repair => self.pending_repo_repair = Some(PendingRepoRepair { slug }),
            RepoAction::Delete => self.pending_repo_delete = Some(slug),
        }
    }

    pub(super) fn add_repo_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(mut form) = self.add_repo.take() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;
        let mut pick_archive = false;

        let dismissed = widgets::dialog(
            ctx,
            "add_plugin_repo",
            &t!("plugins.repos_add_title"),
            620.0,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.repos_add_sub"))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(theme::SPACE_7);
                ui.add(
                    egui::TextEdit::singleline(&mut form.url)
                        .desired_width(f32::INFINITY)
                        .margin(egui::vec2(12.0, 9.0))
                        .hint_text(t!("plugins.repos_url_hint")),
                );
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
                    &t!("plugins.repos_import_archive"),
                    ButtonKind::Secondary,
                    Vec2::new(130.0, 34.0),
                )
                .clicked()
                {
                    pick_archive = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.cancel"),
                    ButtonKind::Secondary,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    cancel = true;
                }
            },
        );

        if confirm {
            let url = form.url.trim().to_owned();
            if !url.is_empty() {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::AddPluginRepo { url },
                );
                return;
            }
        }
        if pick_archive {
            spawn_import_repository_archive(ctx, cmd.clone());
            return;
        }
        if cancel || dismissed {
            return;
        }
        self.add_repo = Some(form);
    }

    /// Confirm removing a repository before unregistering it (this uninstalls
    /// every plugin the repo contributed, so it's not silently destructive).
    pub(super) fn repo_delete_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(slug) = self.pending_repo_delete.clone() else {
            return;
        };
        if let Some(slug) = widgets::confirm_delete_dialog(
            ctx,
            "remove_plugin_repo",
            &t!("plugins.repos_remove_title"),
            &t!("plugins.repos_remove_body", name = slug),
            &t!("plugins.repos_remove"),
            &mut self.pending_repo_delete,
        ) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::RemovePluginRepo { slug },
            );
        }
    }

    /// Confirm restoring the active immutable repository revision. Recovery
    /// is repository-scoped: packages are never repaired in isolation.
    pub(super) fn repo_repair_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        if self.pending_repo_repair.is_none() {
            return;
        }
        if let Some(pending) = widgets::confirm_delete_dialog(
            ctx,
            "repair_plugin_repo",
            &t!("plugins.repos_repair_title"),
            &t!("plugins.repos_repair_body"),
            &t!("plugins.repos_repair"),
            &mut self.pending_repo_repair,
        ) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::UpdatePluginRepo { slug: pending.slug },
            );
        }
    }
}

/// Map a skipped absolute path back to one of the package layouts accepted by
/// the daemon's repo scanner: `<repo>/<package>` or `<repo>/plugins/<package>`.
pub(super) fn skipped_repo_location(
    path: &str,
    repos: &[PluginRepoInfo],
) -> Option<(String, String)> {
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
pub(super) struct RepoRow<'a> {
    slug: &'a str,
    official: bool,
    last_sync: Option<&'a str>,
    locked_short: String,
    remote_short: Option<String>,
    signature: &'a RepoSignatureStatus,
    integrity_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoIntegrityProblem {
    pub(super) slug: String,
    pub(super) repairable: bool,
}

pub(super) fn repo_integrity_failed(slug: &str, plugins: &[PluginInfo]) -> bool {
    plugins.iter().any(|plugin| {
        matches!(&plugin.source, PluginSource::Repo { slug: source } if source == slug)
            && plugin.health.issue.as_ref().is_some_and(|issue| {
                matches!(
                    issue.context.as_ref(),
                    Some(
                        halod_shared::types::PluginIssueContext::RepositoryHashMismatch { .. }
                            | halod_shared::types::PluginIssueContext::RepositoryManifestMismatch { .. }
                    )
                )
            })
    })
}

pub(super) fn repository_integrity_problem(state: &TopicStore) -> Option<RepoIntegrityProblem> {
    state.plugins.repos.iter().find_map(|repo| {
        repo_integrity_failed(&repo.slug, &state.plugins.plugins).then(|| RepoIntegrityProblem {
            slug: repo.slug.clone(),
            repairable: repo.signature == RepoSignatureStatus::Verified,
        })
    })
}

/// Pair each repo with its update status, sorted by slug for a stable order
/// — except the official repo, which always sorts first.
pub(super) fn repo_rows<'a>(
    repos: &'a [PluginRepoInfo],
    updates: &[RepoUpdateStatus],
    plugins: &[PluginInfo],
) -> Vec<RepoRow<'a>> {
    let mut rows: Vec<RepoRow> = repos
        .iter()
        .map(|r| {
            let status = updates.iter().find(|u| u.slug == r.slug);
            let available = status.and_then(|s| s.available_tag.clone());
            RepoRow {
                slug: &r.slug,
                official: r.official,
                last_sync: r.last_sync.as_deref(),
                locked_short: r.release_tag.clone().unwrap_or_default(),
                remote_short: available,
                signature: &r.signature,
                integrity_failed: repo_integrity_failed(&r.slug, plugins),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.official.cmp(&a.official).then_with(|| a.slug.cmp(b.slug)));
    rows
}

/// A small colored tile with a fork glyph, for a repo row/detail header.
pub(super) fn repo_icon_tile(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, 9.0, theme::TILE_BG);
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

pub(super) fn repo_signature_tooltip(status: &RepoSignatureStatus) -> String {
    match status {
        RepoSignatureStatus::Verified => t!("plugins.repo_signature_verified").to_string(),
        RepoSignatureStatus::Unsigned => t!("plugins.repo_signature_unsigned").to_string(),
        RepoSignatureStatus::Invalid { .. } => {
            t!("plugins.repo_signature_verification_failed").to_string()
        }
    }
}

pub(super) fn draw_repo_lock(
    ui: &mut egui::Ui,
    rect: Rect,
    status: &RepoSignatureStatus,
    id: impl std::hash::Hash + std::fmt::Debug,
) {
    let color = match status {
        RepoSignatureStatus::Verified => theme::ONLINE,
        RepoSignatureStatus::Unsigned => theme::STAT_AMBER,
        RepoSignatureStatus::Invalid { .. } => theme::OFFLINE,
    };
    icons::draw(ui, rect, icons::Icon::Lock, color);

    ui.interact(rect, ui.scope_id().with(id), Sense::hover())
        .on_hover_text(repo_signature_tooltip(status));
}

pub(super) fn draw_repo_integrity(
    ui: &mut egui::Ui,
    rect: Rect,
    failed: bool,
    id: impl std::hash::Hash + std::fmt::Debug,
) {
    let (color, tooltip) = if failed {
        (theme::TRAFFIC_RED, t!("plugins.integrity_failed"))
    } else {
        (theme::ONLINE, t!("plugins.integrity_ok"))
    };
    icons::draw(ui, rect, icons::Icon::IntegrityShield, color);
    ui.interact(rect, ui.scope_id().with(id), Sense::hover())
        .on_hover_text(tooltip);
}

pub(super) enum RepoRowAction {
    Select,
    Check,
    None,
}

fn repo_row_subtitle(row: &RepoRow) -> String {
    let mut parts = vec![match row.signature {
        RepoSignatureStatus::Verified => t!("plugins.repo_signed").into_owned(),
        RepoSignatureStatus::Unsigned => t!("plugins.repo_unsigned").into_owned(),
        RepoSignatureStatus::Invalid { .. } => t!("plugins.provenance_invalid").into_owned(),
    }];
    match &row.remote_short {
        Some(remote) if !row.locked_short.is_empty() => {
            parts.push(format!("{} → {remote}", row.locked_short))
        }
        Some(remote) => parts.push(remote.clone()),
        None if !row.locked_short.is_empty() => parts.push(row.locked_short.clone()),
        None => {}
    }
    if let Some(sync) = row.last_sync {
        parts.push(t!("plugins.repo_synced", age = format_last_sync(sync)).into_owned());
    }
    parts.join(" · ")
}

/// One selectable repo row in the list column.
pub(super) fn repo_row(
    ui: &mut egui::Ui,
    row: &RepoRow,
    selected: bool,
    updates_enabled: bool,
    checking: bool,
) -> RepoRowAction {
    let (rect, resp) = widgets::hover_row(ui, 48.0, selected);
    if row.official {
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::PluginsOfficialRepo,
            rect,
        );
    }
    let mut check = false;
    let mut right = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(8.0, 9.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    if updates_enabled {
        let label = t!("plugins.repos_check_updates");
        let size = Vec2::new(130.0, 30.0);
        if checking {
            widgets::button_loading(&mut right, &label, ButtonKind::Secondary, size);
        } else {
            check = widgets::button(&mut right, &label, ButtonKind::Secondary, size).clicked();
        }
    }
    if row.integrity_failed {
        right.add_space(theme::SPACE_4);
        let (icon, _) = right.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
        draw_repo_integrity(ui, icon, true, ("repo_integrity", row.slug));
    }
    let clip = Rect::from_min_max(
        rect.min,
        Pos2::new(right.min_rect().left() - theme::SPACE_6, rect.bottom()),
    );
    let painter = ui.painter().with_clip_rect(clip);
    let name = if row.official {
        t!("plugins.repo_official").into_owned()
    } else {
        row.slug.to_owned()
    };
    painter.text(
        Pos2::new(rect.left() + 8.0, rect.center().y + 1.0),
        Align2::LEFT_BOTTOM,
        name,
        theme::medium(14.0),
        theme::TEXT,
    );
    painter.text(
        Pos2::new(rect.left() + 8.0, rect.center().y + 2.0),
        Align2::LEFT_TOP,
        repo_row_subtitle(row),
        theme::body_sm(),
        if row.remote_short.is_some() {
            theme::WARN_TEXT
        } else {
            theme::TEXT_MUT
        },
    );

    if check {
        RepoRowAction::Check
    } else if resp.clicked() {
        RepoRowAction::Select
    } else {
        RepoRowAction::None
    }
}

/// A repo update is in flight until the reason it was offered is gone: the repo
/// is no longer behind its selected release and no package it owns is still
/// changed on disk. Keyed off the repo-level status, not per-plugin
/// `update_available` — a repo can be behind while every package is unchanged.
pub(super) fn clear_finished_repo_updates(
    updating_repos: &mut HashMap<String, f64>,
    repo_updates: &[RepoUpdateStatus],
    plugin_updates: &[PluginUpdateStatus],
    now: f64,
) {
    updating_repos.retain(|slug, started| {
        let behind = repo_updates
            .iter()
            .any(|status| status.slug == *slug && status.available_tag.is_some());
        let dirty = plugin_updates.iter().any(|status| {
            status.slug == *slug && (status.update_available || status.on_disk_changed)
        });
        (behind || dirty) && now - *started < UPDATE_TIMEOUT
    });
}

pub(super) fn update_all_seed_slugs(
    repo_updates: &[RepoUpdateStatus],
    plugin_updates: &[PluginUpdateStatus],
) -> Vec<String> {
    let mut slugs: Vec<String> = plugin_updates
        .iter()
        .filter(|status| status.update_available)
        .map(|status| status.slug.clone())
        .collect();
    for repo in repo_updates
        .iter()
        .filter(|repo| repo.available_tag.is_some() && !repo.pinned)
    {
        if !slugs.contains(&repo.slug) {
            slugs.push(repo.slug.clone());
        }
    }
    slugs
}

pub(super) fn repository_integrity_banner(
    ui: &mut egui::Ui,
    problem: &RepoIntegrityProblem,
) -> bool {
    let title = t!("plugins.integrity_banner_title");
    let subtitle = integrity_subtitle(&problem.slug, problem.repairable);
    let action = if problem.repairable {
        t!("plugins.repos_resolve")
    } else {
        t!("plugins.repos_remove_reimport")
    };
    widgets::Banner::danger(&title)
        .subtitle(&subtitle)
        .dot(theme::TRAFFIC_RED)
        .show_with(ui, |ui| {
            ui.add_space(theme::SPACE_5);
            widgets::button(
                ui,
                &action,
                ButtonKind::Warn,
                Vec2::new(if problem.repairable { 112.0 } else { 150.0 }, 30.0),
            )
            .clicked()
        })
}

/// `last_sync` is stored as an RFC3339 timestamp. Keep the stat compact by
/// showing a relative age (the exact value remains available in the source
/// data and can be inspected in diagnostics).
pub(super) fn format_last_sync(raw: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.to_owned();
    };
    format_last_sync_at(parsed.with_timezone(&chrono::Utc), chrono::Utc::now())
}

pub(super) fn format_last_sync_at(
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
pub(super) fn stat_box(ui: &mut egui::Ui, label: &str, value: &str) {
    widgets::inset(ui, theme::PAD_BANNER, |ui| {
        widgets::section_label(ui, label);
        ui.add_space(theme::SPACE_2);
        ui.label(
            egui::RichText::new(value)
                .font(theme::body_md())
                .color(theme::TEXT),
        );
    });
}

/// Empty-state panel under "Drivers from this repository" when the repo has
/// contributed no plugins yet: a dashed border box with centered muted text.
pub(super) fn repo_no_drivers_box(ui: &mut egui::Ui) {
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
        theme::body_md(),
        theme::TEXT_MUT,
    );
}

/// Repository update checks only run when the user has allowed the daemon to
/// contact GitHub; any other consent state (unset or denied) leaves them off.
pub(super) fn plugin_updates_enabled(consent: PluginDownloadConsent) -> bool {
    matches!(consent, PluginDownloadConsent::Allowed)
}

pub(super) fn updates_due_count(
    repo_updates: &[RepoUpdateStatus],
    plugin_updates: &[PluginUpdateStatus],
) -> usize {
    let package_updates = plugin_updates
        .iter()
        .filter(|status| status.update_available)
        .count();
    package_updates
        + repo_updates
            .iter()
            .filter(|repo| {
                !repo.pinned
                    && repo.available_tag.is_some()
                    && !plugin_updates
                        .iter()
                        .any(|plugin| plugin.slug == repo.slug && plugin.update_available)
            })
            .count()
}

/// A muted info band explaining that update checks are turned off, shown above
/// the greyed-out "Check for updates" button when downloads are not allowed.
pub(super) fn updates_disabled_note(ui: &mut egui::Ui) {
    widgets::Banner::neutral(&t!("plugins.repos_updates_disabled"))
        .title_color(theme::TEXT_MUT)
        .title_font(theme::body_md())
        .dot(theme::TEXT_FAINT)
        .show(ui);
}

/// What the user asked for in a repository's detail pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RepoAction {
    SelectPlugin(String),
    Check,
    Update,
    MovePin(String),
    Repair,
    Delete,
}

/// Update state of the repository shown in the detail pane.
pub(super) struct RepoView<'a> {
    checking: bool,
    updates_enabled: bool,
    has_updates: bool,
    pinned_update: Option<&'a str>,
    updating: bool,
}

/// The repo detail panel: header, stat boxes, "Check for updates", the list
/// of plugins it provides, and Remove (hidden for the official repo).
pub(super) fn repo_detail_body(
    ui: &mut egui::Ui,
    r: &PluginRepoInfo,
    cmd: &CommandTx,
    releases: &[halod_shared::types::PluginReleaseInfo],
    plugins: &[PluginInfo],
    view: &RepoView,
) -> Option<RepoAction> {
    let mut action = None;
    let integrity_failed = repo_integrity_failed(&r.slug, plugins);
    let repairable = r.signature == RepoSignatureStatus::Verified;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                repo_icon_tile(ui, 44.0);
                ui.add_space(theme::SPACE_2);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&r.slug)
                                .font(theme::bold(18.0))
                                .color(theme::TEXT),
                        );
                        let (lock_rect, _) =
                            ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        draw_repo_lock(
                            ui,
                            lock_rect,
                            &r.signature,
                            ("repo_detail_signature", &r.slug),
                        );
                        let (integrity_rect, _) =
                            ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        draw_repo_integrity(
                            ui,
                            integrity_rect,
                            integrity_failed,
                            ("repo_detail_integrity", &r.slug),
                        );
                    });
                    let url = ui.add(
                        egui::Label::new(
                            egui::RichText::new(&r.url)
                                .font(theme::body(10.0))
                                .color(theme::CYAN)
                                .underline(),
                        )
                        .sense(Sense::click()),
                    );
                    if url.hovered() && r.location == PluginRepoLocation::RemoteRelease {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if url.clicked() && r.location == PluginRepoLocation::RemoteRelease {
                        ui.ctx().open_url(egui::OpenUrl {
                            url: r.url.clone(),
                            new_tab: true,
                        });
                    }
                    if let Some(fingerprint) = &r.signing_key_fingerprint {
                        ui.label(
                            egui::RichText::new(t!(
                                "plugins.repo_signing_fingerprint",
                                fingerprint = fingerprint
                            ))
                            .font(theme::mono(9.0))
                            .color(theme::TEXT_MUT),
                        );
                    }
                });
            });
        },
        |_ui| {},
    );

    ui.add_space(theme::SPACE_8);
    if r.location == PluginRepoLocation::RemoteRelease {
        const LATEST: &str = "__latest__";
        let current = match &r.release_policy {
            halod_shared::types::PluginReleasePolicy::Latest => LATEST,
            halod_shared::types::PluginReleasePolicy::Pinned(tag) => tag,
        };
        let selected_text = if current == LATEST {
            match &r.release_tag {
                Some(tag) => format!("Latest ({tag})"),
                None => "Latest".to_owned(),
            }
        } else {
            current.to_owned()
        };
        let mut selected = current.to_owned();
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("plugin_release", &r.slug))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut selected, LATEST.to_owned(), "Latest");
                    for release in releases {
                        let label = if release.prerelease {
                            format!("{} (prerelease)", release.tag)
                        } else {
                            release.tag.clone()
                        };
                        ui.selectable_value(&mut selected, release.tag.clone(), label);
                    }
                });
            if current != LATEST {
                ui.add_space(theme::SPACE_3);
                let _ = widgets::chip_colored(ui, &t!("plugins.repo_pinned"), theme::STAT_AMBER);
            }
        });
        if current != LATEST {
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(t!("plugins.repo_pinned_hint"))
                    .font(theme::body(10.0))
                    .color(theme::TEXT_MUT),
            );
        }
        if selected != current {
            let command = if selected == LATEST {
                DaemonCommand::FollowLatestPluginRelease {
                    slug: r.slug.clone(),
                }
            } else {
                DaemonCommand::InstallPluginRelease {
                    slug: r.slug.clone(),
                    tag: selected,
                }
            };
            ipc::send(cmd, command);
        }
        ui.add_space(theme::SPACE_5);
    }
    let repo_plugins: Vec<&PluginInfo> = plugins
        .iter()
        .filter(|p| matches!(&p.source, PluginSource::Repo { slug } if *slug == r.slug))
        .collect();
    ui.columns(3, |cols| {
        let source = match r.location {
            PluginRepoLocation::RemoteRelease => "GitHub release".into(),
            PluginRepoLocation::LocalArchive => t!("plugins.repo_source_local_archive"),
        };
        stat_box(&mut cols[0], &t!("plugins.repo_source"), &source);
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

    ui.add_space(theme::SPACE_7);
    if integrity_failed {
        let title = t!("plugins.integrity_banner_title");
        let subtitle = integrity_subtitle(&r.slug, repairable);
        widgets::Banner::danger(&title)
            .subtitle(&subtitle)
            .dot(theme::TRAFFIC_RED)
            .show(ui);
        ui.add_space(theme::SPACE_5);
    }
    if !view.updates_enabled {
        updates_disabled_note(ui);
        ui.add_space(theme::SPACE_5);
    }
    if let Some(tag) = view.pinned_update {
        widgets::Banner::warn(&t!("plugins.repo_pinned_update_title", tag = tag))
            .subtitle(&t!(
                "plugins.repo_pinned_update_body",
                pinned = r.release_tag.clone().unwrap_or_default(),
                tag = tag
            ))
            .dot(theme::STAT_AMBER)
            .show(ui);
        ui.add_space(theme::SPACE_5);
    }
    ui.horizontal(|ui| {
        let size = Vec2::new(180.0, 32.0);
        if view.checking && view.updates_enabled {
            ui.add(egui::Spinner::new().size(18.0).color(theme::CYAN));
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(t!("plugins.repos_checking"))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
        } else if widgets::button_enabled(
            ui,
            &t!("plugins.repos_check_updates"),
            ButtonKind::Primary,
            size,
            view.updates_enabled,
        ) {
            action = Some(RepoAction::Check);
        }

        // The repo is the unit of update: pull it here rather than per plugin.
        let idle = view.updates_enabled && !view.checking;
        let update_button = |ui: &mut egui::Ui, label: &str, width: f32| {
            ui.add_space(theme::SPACE_5);
            let size = Vec2::new(width, 32.0);
            if view.updating {
                widgets::button_loading(ui, &t!("plugins.updating"), ButtonKind::Warn, size);
                false
            } else {
                widgets::button(ui, label, ButtonKind::Warn, size).clicked()
            }
        };
        if view.has_updates
            && (view.updating || idle)
            && update_button(ui, &t!("plugins.repos_update_repo"), 150.0)
        {
            action = Some(RepoAction::Update);
        }
        if let Some(tag) = view.pinned_update.filter(|_| !view.has_updates && idle) {
            if update_button(ui, &t!("plugins.repos_move_pin", tag = tag), 190.0) {
                action = Some(RepoAction::MovePin(tag.to_owned()));
            }
        }

        if integrity_failed {
            ui.add_space(theme::SPACE_5);
            let label = if repairable {
                t!("plugins.repos_resolve")
            } else {
                t!("plugins.repos_remove_reimport")
            };
            if widgets::button(
                ui,
                &label,
                ButtonKind::Warn,
                Vec2::new(if repairable { 130.0 } else { 170.0 }, 32.0),
            )
            .clicked()
            {
                action = Some(if repairable {
                    RepoAction::Repair
                } else {
                    RepoAction::Delete
                });
            }
        }
    });

    ui.add_space(20.0);
    widgets::section_label(ui, &t!("plugins.repo_drivers_from"));
    ui.add_space(theme::SPACE_4);

    if repo_plugins.is_empty() {
        repo_no_drivers_box(ui);
    }

    for p in &repo_plugins {
        let (rect, resp) = widgets::hover_row(ui, 42.0, false);
        let tile_rect = Rect::from_min_size(
            Pos2::new(rect.left() + 6.0, rect.center().y - 12.0),
            Vec2::splat(24.0),
        );
        initials_tile_at(ui, tile_rect, plugin_name(p));
        let text_x = tile_rect.right() + 10.0;
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 6.0),
            Align2::LEFT_TOP,
            plugin_name(p),
            theme::subhead(),
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
            theme::value_xs(),
            theme::TEXT_FAINT,
        );
        ui.painter().circle_filled(
            Pos2::new(rect.right() - 12.0, rect.center().y),
            3.5,
            status_dot(p),
        );
        if resp.clicked() {
            action = Some(RepoAction::SelectPlugin(p.id.clone()));
        }
    }

    if !r.official {
        ui.add_space(20.0);
        ui.separator();
        ui.add_space(theme::SPACE_7);
        if widgets::button(
            ui,
            &t!("plugins.repos_remove"),
            ButtonKind::Danger,
            Vec2::new(150.0, 34.0),
        )
        .clicked()
        {
            action = Some(RepoAction::Delete);
        }
    }

    action
}

pub(super) fn integrity_subtitle(slug: &str, repairable: bool) -> std::borrow::Cow<'static, str> {
    if repairable {
        t!("plugins.integrity_banner_signed", repository = slug)
    } else {
        t!("plugins.integrity_banner_unsigned", repository = slug)
    }
}

pub(super) fn spawn_import_repository_archive(ctx: &egui::Context, cmd: CommandTx) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Repository archive", &["tar", "gz", "tgz"])
            .pick_file()
        {
            crate::runtime::ipc::send(
                &cmd,
                halod_shared::commands::DaemonCommand::ImportPluginRepository {
                    source_path: path.to_string_lossy().into_owned(),
                },
            );
        }
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::plugins::tests::*;

    #[test]
    fn add_source_button_registers_both_plugin_tour_steps() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(28.0, 26.0));

        PluginsUi::anchor_add_plugin_source(&ctx, rect);

        assert_eq!(
            crate::domain::tour::take_anchor(&ctx, crate::domain::tour::AnchorId::PluginsAddPlugin),
            Some(rect)
        );
        assert_eq!(
            crate::domain::tour::take_anchor(&ctx, crate::domain::tour::AnchorId::PluginsAddRepo),
            Some(rect)
        );
    }

    #[test]
    fn plugin_updates_enabled_only_when_downloads_are_allowed() {
        assert!(plugin_updates_enabled(PluginDownloadConsent::Allowed));
        assert!(!plugin_updates_enabled(PluginDownloadConsent::Denied));
        assert!(!plugin_updates_enabled(PluginDownloadConsent::Unset));
    }

    #[test]
    fn update_banner_counts_repo_only_updates_without_counting_pins_or_duplicates() {
        let repos = [
            RepoUpdateStatus {
                slug: "repo-only".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: false,
            },
            RepoUpdateStatus {
                slug: "package-update".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: false,
            },
            RepoUpdateStatus {
                slug: "pinned".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: true,
            },
        ];
        let packages = [PluginUpdateStatus {
            slug: "package-update".into(),
            ..status("plugin", true)
        }];

        assert_eq!(updates_due_count(&repos, &packages), 2);
    }

    #[test]
    fn update_all_seeds_package_updates_and_unpinned_behind_repos() {
        let repos = [
            RepoUpdateStatus {
                slug: "repo-only".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: false,
            },
            RepoUpdateStatus {
                slug: "pinned".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: true,
            },
            RepoUpdateStatus {
                slug: "with-package".into(),
                installed_tag: "v1".into(),
                available_tag: Some("v2".into()),
                pinned: false,
            },
        ];
        let packages = [PluginUpdateStatus {
            slug: "with-package".into(),
            ..status("plugin", true)
        }];

        let seeded = update_all_seed_slugs(&repos, &packages);
        assert!(seeded.contains(&"repo-only".to_owned()), "repo-only behind");
        assert!(
            seeded.contains(&"with-package".to_owned()),
            "per-plugin update"
        );
        assert!(!seeded.contains(&"pinned".to_owned()), "pins are excluded");
        assert_eq!(
            seeded.iter().filter(|s| *s == "with-package").count(),
            1,
            "a repo with both a package update and a repo-behind flag seeds once"
        );
    }

    fn repo_status(slug: &str, behind: bool) -> RepoUpdateStatus {
        RepoUpdateStatus {
            slug: slug.to_owned(),
            installed_tag: "v1".to_owned(),
            available_tag: behind.then(|| "v2".to_owned()),
            pinned: false,
        }
    }

    #[test]
    fn repository_update_progress_tracks_multiple_repositories_independently() {
        let mut updating = HashMap::from([("repo-a".to_owned(), 0.0), ("repo-b".to_owned(), 0.0)]);

        clear_finished_repo_updates(
            &mut updating,
            &[repo_status("repo-a", false), repo_status("repo-b", true)],
            &[],
            1.0,
        );

        assert!(!updating.contains_key("repo-a"));
        assert!(updating.contains_key("repo-b"));
    }

    #[test]
    fn repository_update_progress_survives_a_repo_with_no_per_plugin_updates() {
        let mut updating = HashMap::from([("repo-a".to_owned(), 0.0)]);

        // A repo can be behind while every package it owns is unchanged; the
        // spinner must not be retired the frame after the click.
        clear_finished_repo_updates(&mut updating, &[repo_status("repo-a", true)], &[], 1.0);
        assert!(updating.contains_key("repo-a"));

        clear_finished_repo_updates(&mut updating, &[repo_status("repo-a", false)], &[], 2.0);
        assert!(updating.is_empty());
    }

    #[test]
    fn repository_update_progress_holds_while_a_package_is_changed_on_disk() {
        let mut updating = HashMap::from([("repo-a".to_owned(), 0.0)]);
        let mut dirty = status("a", false);
        dirty.slug = "repo-a".into();
        dirty.on_disk_changed = true;

        clear_finished_repo_updates(
            &mut updating,
            &[repo_status("repo-a", false)],
            std::slice::from_ref(&dirty),
            1.0,
        );
        assert!(updating.contains_key("repo-a"));

        clear_finished_repo_updates(
            &mut updating,
            &[repo_status("repo-a", false)],
            &[],
            UPDATE_TIMEOUT + 1.0,
        );
        assert!(updating.is_empty());
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
    fn repo_actions_update_state_and_dispatch_matching_commands() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = repo("acme", "v1");
        let mut ui = PluginsUi::default();
        ui.apply_repo_action(1.0, &cmd, &r, RepoAction::Check);
        assert_eq!(
            ui.checking_repo.as_ref().map(|c| c.slug.as_str()),
            Some("acme")
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::CheckPluginRepoUpdates)
        ));
        ui.apply_repo_action(2.0, &cmd, &r, RepoAction::MovePin("v2".into()));
        assert_eq!(ui.updating_repos.get("acme"), Some(&2.0));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::InstallPluginRelease { slug, tag }) if slug == "acme" && tag == "v2"
        ));
        ui.apply_repo_action(3.0, &cmd, &r, RepoAction::Repair);
        ui.apply_repo_action(3.0, &cmd, &r, RepoAction::Delete);
        ui.apply_repo_action(3.0, &cmd, &r, RepoAction::SelectPlugin("p".into()));
        assert_eq!(
            ui.pending_repo_repair.map(|p| p.slug).as_deref(),
            Some("acme")
        );
        assert_eq!(ui.pending_repo_delete.as_deref(), Some("acme"));
        assert_eq!(ui.selection, Selection::Plugin("p".into()));
        assert!(rx.try_recv().is_err());
    }

    fn integrity_failed_plugin(slug: &str) -> PluginInfo {
        let mut plugin = info("broken", false);
        plugin.source = PluginSource::Repo {
            slug: slug.to_owned(),
        };
        plugin.health.issue = Some(PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "repository failed integrity validation".into(),
            context: Some(
                halod_shared::types::PluginIssueContext::RepositoryHashMismatch {
                    package: "broken".into(),
                    expected: "aaaa".into(),
                    actual: "bbbb".into(),
                },
            ),
            timestamp_ms: 0,
        });
        plugin
    }

    #[test]
    fn invalid_signature_tooltip_does_not_expose_backend_reason() {
        let tooltip = repo_signature_tooltip(&RepoSignatureStatus::Invalid {
            reason: "release signature does not match release.yaml".into(),
        });
        assert_eq!(tooltip, "Repository signature verification failed.");
        assert!(!tooltip.contains("release.yaml"));
    }

    #[test]
    fn repo_rows_sorts_by_slug_and_marks_up_to_date_repos_unbehind() {
        let repos = vec![repo("zebra", "aaaaaaaa1111"), repo("alpha", "bbbbbbbb2222")];
        let rows = repo_rows(&repos, &[], &[]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].slug, "alpha");
        assert_eq!(rows[1].slug, "zebra");
        assert!(rows[0].remote_short.is_none());
        assert_eq!(rows[0].locked_short, "bbbbbbbb2222");
        assert_eq!(*rows[0].signature, RepoSignatureStatus::Unsigned);
    }

    #[test]
    fn repo_rows_surfaces_only_an_available_release_tag() {
        let repos = vec![repo("foo", "v1")];
        let up_to_date = [RepoUpdateStatus {
            slug: "foo".into(),
            installed_tag: "v1".into(),
            available_tag: None,
            pinned: false,
        }];
        let rows = repo_rows(&repos, &up_to_date, &[]);
        assert!(rows[0].remote_short.is_none());

        let behind = [RepoUpdateStatus {
            slug: "foo".into(),
            installed_tag: "v1".into(),
            available_tag: Some("v2".into()),
            pinned: false,
        }];
        let rows = repo_rows(&repos, &behind, &[]);
        assert_eq!(rows[0].remote_short.as_deref(), Some("v2"));
    }

    #[test]
    fn repo_rows_surfaces_a_release_available_beyond_a_pin_without_claiming_behind() {
        let repos = vec![repo("foo", "v1")];
        let pinned = [RepoUpdateStatus {
            slug: "foo".into(),
            installed_tag: "v1".into(),
            available_tag: Some("v2".into()),
            pinned: true,
        }];
        let rows = repo_rows(&repos, &pinned, &[]);
        assert_eq!(rows[0].remote_short.as_deref(), Some("v2"));
    }

    #[test]
    fn repository_update_progress_holds_while_a_pin_is_being_moved() {
        let mut updating = HashMap::from([("repo-a".to_owned(), 0.0)]);
        let mut pinned = repo_status("repo-a", false);
        pinned.available_tag = Some("v2".into());
        pinned.pinned = true;

        clear_finished_repo_updates(&mut updating, std::slice::from_ref(&pinned), &[], 1.0);
        assert!(updating.contains_key("repo-a"));

        clear_finished_repo_updates(&mut updating, &[repo_status("repo-a", false)], &[], 2.0);
        assert!(updating.is_empty());
    }

    #[test]
    fn repo_rows_puts_the_official_repo_first_regardless_of_slug() {
        let mut official = repo("aaa-not-alphabetically-first", "aaaaaaaa");
        official.official = true;
        let repos = vec![repo("alpha", "bbbbbbbb"), official];
        let rows = repo_rows(&repos, &[], &[]);
        assert!(rows[0].official, "the official repo must sort first");
        assert_eq!(rows[1].slug, "alpha");
    }

    #[test]
    fn repository_integrity_problem_repairs_only_verified_sources() {
        let mut state = TopicStore::default();
        state.plugins.repos = vec![repo("publisher", "aaaaaaaa")];
        state.plugins.plugins = vec![integrity_failed_plugin("publisher")];

        let unsigned = repository_integrity_problem(&state).unwrap();
        assert_eq!(unsigned.slug, "publisher");
        assert!(!unsigned.repairable);

        state.plugins.repos[0].signature = RepoSignatureStatus::Verified;
        let verified = repository_integrity_problem(&state).unwrap();
        assert!(verified.repairable);

        state.plugins.plugins[0].health.issue = None;
        assert!(repository_integrity_problem(&state).is_none());
    }

    #[test]
    fn repo_row_marks_package_hash_mismatch_as_failed_integrity() {
        let repos = vec![repo("publisher", "aaaaaaaa")];
        let plugins = vec![integrity_failed_plugin("publisher")];
        let rows = repo_rows(&repos, &[], &plugins);
        assert!(rows[0].integrity_failed);
    }
}

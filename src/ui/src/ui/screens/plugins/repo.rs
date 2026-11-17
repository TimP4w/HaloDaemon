// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::domain::tour;
use crate::domain::tour::AnchorId;

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

        let rows = repo_rows(&cx.state.plugins.repos);
        if rows.is_empty() {
            ui.label(
                egui::RichText::new(t!("plugins.repos_empty"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
            return;
        }
        for repo in rows {
            let selected = self.selection == Selection::Repo(repo.slug.clone());
            let checking = self.checking_repo.is_some();
            match repo_row(ui, repo, selected, checking) {
                RepoRowAction::Select => self.selection = Selection::Repo(repo.slug.clone()),
                RepoRowAction::Refresh => {
                    self.apply_repo_action(now, cx.cmd, repo, RepoAction::Refresh)
                }
                RepoRowAction::None => {}
            }
        }
    }

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

    /// The add-source button backs two consecutive tour steps; register both
    /// anchors so neither is skipped as missing.
    pub(super) fn anchor_add_plugin_source(ctx: &egui::Context, rect: Rect) {
        tour::anchor(ctx, AnchorId::PluginsAddPlugin, rect);
        tour::anchor(ctx, AnchorId::PluginsAddRepo, rect);
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

    pub(super) fn repo_detail(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PluginsCtx,
        r: &PluginRepoInfo,
    ) -> Option<RepoAction> {
        let view = RepoView {
            checking: self.checking_repo.is_some(),
            installing: &self.installing,
        };
        if view.checking || !view.installing.is_empty() {
            ui.ctx().request_repaint();
        }
        repo_detail_body(ui, r, &cx.state.plugins, &view)
    }

    pub(super) fn apply_repo_action(
        &mut self,
        now: f64,
        cmd: &CommandTx,
        r: &PluginRepoInfo,
        action: RepoAction,
    ) {
        match action {
            RepoAction::SelectPlugin(id) => self.selection = Selection::Plugin(id),
            RepoAction::Refresh => {
                self.checking_repo = Some(RepoCheck {
                    slug: r.slug.clone(),
                    prev_sync: r.last_sync.clone(),
                    started: now,
                });
                ipc::send(cmd, DaemonCommand::RefreshPluginRepos);
            }
            RepoAction::Install(id) => self.install(now, cmd, &r.slug, &id),
            RepoAction::Delete => self.pending_repo_delete = Some(r.slug.clone()),
        }
    }

    pub(super) fn add_repo_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(mut form) = self.add_repo.take() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;
        let mut pick_folder = false;

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
                    &t!("plugins.repos_add_folder"),
                    ButtonKind::Secondary,
                    Vec2::new(130.0, 34.0),
                )
                .clicked()
                {
                    pick_folder = true;
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
            if let Some(source) = url_source(&form.url) {
                ipc::send(cmd, DaemonCommand::AddPluginRepo { source });
                return;
            }
        }
        if pick_folder {
            spawn_add_folder(ctx, cmd.clone());
            return;
        }
        if cancel || dismissed {
            return;
        }
        self.add_repo = Some(form);
    }

    /// Removing a repository uninstalls every plugin it provided.
    pub(super) fn repo_delete_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        if self.pending_repo_delete.is_none() {
            return;
        }
        if let Some(slug) = widgets::confirm_delete_dialog(
            ctx,
            "remove_plugin_repo",
            &t!("plugins.repos_remove_title"),
            &t!(
                "plugins.repos_remove_body",
                name = self.pending_repo_delete.as_deref().unwrap_or_default()
            ),
            &t!("plugins.repos_remove"),
            &mut self.pending_repo_delete,
        ) {
            ipc::send(cmd, DaemonCommand::RemovePluginRepo { slug });
        }
    }
}

pub(super) fn url_source(input: &str) -> Option<PluginRepoLocation> {
    let url = input.trim();
    (!url.is_empty()).then(|| PluginRepoLocation::Url(url.to_owned()))
}

/// The official repository first, then the rest by slug.
pub(super) fn repo_rows(repos: &[PluginRepoInfo]) -> Vec<&PluginRepoInfo> {
    let mut rows: Vec<&PluginRepoInfo> = repos.iter().collect();
    rows.sort_by(|a, b| {
        b.official
            .cmp(&a.official)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    rows
}

pub(super) fn repo_name(r: &PluginRepoInfo) -> std::borrow::Cow<'_, str> {
    if r.official {
        t!("plugins.repo_official")
    } else {
        std::borrow::Cow::Borrowed(r.slug.as_str())
    }
}

pub(super) fn repo_location_text(r: &PluginRepoInfo) -> &str {
    match &r.location {
        PluginRepoLocation::Url(url) => url,
        PluginRepoLocation::Local(path) => path,
    }
}

fn is_local(r: &PluginRepoInfo) -> bool {
    matches!(r.location, PluginRepoLocation::Local(_))
}

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

pub(super) fn trust_label(r: &PluginRepoInfo) -> std::borrow::Cow<'static, str> {
    match (is_local(r), r.trust) {
        (true, _) => t!("plugins.repo_local"),
        (false, RepoTrust::Official) => t!("plugins.repo_signature_official"),
        (false, RepoTrust::Signed) => t!("plugins.repo_signed"),
        (false, RepoTrust::Unsigned) => t!("plugins.repo_unsigned"),
    }
}

fn repo_provenance(r: &PluginRepoInfo) -> PluginProvenance {
    match (is_local(r), r.trust) {
        (true, _) => PluginProvenance::Local,
        (false, RepoTrust::Official) => PluginProvenance::Official,
        (false, RepoTrust::Signed) => PluginProvenance::Signed,
        (false, RepoTrust::Unsigned) => PluginProvenance::Unsigned,
    }
}

pub(super) fn trust_badges(
    provenance: PluginProvenance,
) -> &'static [(icons::Icon, egui::Color32)] {
    match provenance {
        PluginProvenance::Local => &[],
        PluginProvenance::Official => &[
            (icons::Icon::Lock, theme::ONLINE),
            (icons::Icon::Verified, theme::ONLINE),
        ],
        PluginProvenance::Signed => &[(icons::Icon::Lock, theme::ONLINE)],
        PluginProvenance::Unsigned => &[(icons::Icon::LockOpen, theme::STAT_AMBER)],
    }
}

/// Trust icons starting at `left`, centered on `center_y`, with `hover` as tooltip.
pub(super) fn draw_trust_badges(
    ui: &mut egui::Ui,
    left: f32,
    center_y: f32,
    size: f32,
    provenance: PluginProvenance,
    hover: &str,
) {
    let badges = trust_badges(provenance);
    if badges.is_empty() {
        return;
    }
    let mut x = left;
    for &(icon, color) in badges {
        icons::draw(
            ui,
            Rect::from_min_size(Pos2::new(x, center_y - size / 2.0), Vec2::splat(size)),
            icon,
            color,
        );
        x += size + 3.0;
    }
    let area = Rect::from_min_max(
        Pos2::new(left, center_y - size / 2.0),
        Pos2::new(x, center_y + size / 2.0),
    );
    ui.interact(area, ui.scope_id().with(("trust", hover)), Sense::hover())
        .on_hover_text(hover);
}

pub(super) enum RepoRowAction {
    Select,
    Refresh,
    None,
}

fn repo_row_subtitle(r: &PluginRepoInfo) -> String {
    match &r.last_sync {
        Some(sync) => t!("plugins.repo_synced", age = format_last_sync(sync)).into_owned(),
        None if is_local(r) => trust_label(r).into_owned(),
        None => String::new(),
    }
}

pub(super) fn repo_row(
    ui: &mut egui::Ui,
    r: &PluginRepoInfo,
    selected: bool,
    checking: bool,
) -> RepoRowAction {
    let (rect, resp) = widgets::hover_row(ui, 48.0, selected);
    if r.official {
        tour::anchor(ui.ctx(), AnchorId::PluginsOfficialRepo, rect);
    }
    let mut refresh = false;
    let mut right = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(8.0, 9.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    if !is_local(r) {
        let label = t!("plugins.repos_refresh");
        let size = Vec2::new(100.0, 30.0);
        if checking {
            widgets::button_loading(&mut right, &label, ButtonKind::Secondary, size);
        } else {
            refresh = widgets::button(&mut right, &label, ButtonKind::Secondary, size).clicked();
        }
    }
    let clip = Rect::from_min_max(
        rect.min,
        Pos2::new(right.min_rect().left() - theme::SPACE_6, rect.bottom()),
    );
    let painter = ui.painter().with_clip_rect(clip);
    let subtitle = repo_row_subtitle(r);
    let name = if subtitle.is_empty() {
        painter.text(
            Pos2::new(rect.left() + 8.0, rect.center().y),
            Align2::LEFT_CENTER,
            repo_name(r),
            theme::medium(14.0),
            theme::TEXT,
        )
    } else {
        painter.text(
            Pos2::new(rect.left() + 8.0, rect.center().y + 2.0),
            Align2::LEFT_TOP,
            subtitle,
            theme::body_sm(),
            theme::TEXT_MUT,
        );
        painter.text(
            Pos2::new(rect.left() + 8.0, rect.center().y + 1.0),
            Align2::LEFT_BOTTOM,
            repo_name(r),
            theme::medium(14.0),
            theme::TEXT,
        )
    };
    draw_trust_badges(
        ui,
        name.right() + 6.0,
        name.center().y,
        14.0,
        repo_provenance(r),
        &trust_label(r),
    );

    if refresh {
        RepoRowAction::Refresh
    } else if resp.clicked() {
        RepoRowAction::Select
    } else {
        RepoRowAction::None
    }
}

/// `last_sync` is RFC 3339; show it as a compact relative age.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RepoAction {
    SelectPlugin(String),
    Refresh,
    Install(String),
    Delete,
}

pub(super) struct RepoView<'a> {
    checking: bool,
    installing: &'a HashMap<String, f64>,
}

/// One row of a repository's plugin list: what it offers and what is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoEntry<'a> {
    pub(super) id: &'a str,
    pub(super) name: &'a str,
    pub(super) installed: Option<&'a str>,
    pub(super) available: Option<&'a AvailablePlugin>,
}

impl RepoEntry<'_> {
    pub(super) fn action(&self) -> Option<EntryAction> {
        let available = self.available?;
        if !available.compatible {
            return Some(EntryAction::NeedsNewerDaemon);
        }
        match self.installed {
            None => Some(EntryAction::Install),
            Some(installed) if is_newer(&available.version, installed) => Some(EntryAction::Update),
            Some(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryAction {
    Install,
    Update,
    NeedsNewerDaemon,
}

pub(super) fn repo_entries<'a>(slug: &str, state: &'a PluginsState) -> Vec<RepoEntry<'a>> {
    let installed = state.plugins.iter().filter(|p| p.repo == slug);
    let mut entries: Vec<RepoEntry> = installed
        .map(|p| RepoEntry {
            id: &p.id,
            name: plugin_name(p),
            installed: Some(&p.version),
            available: state
                .available
                .iter()
                .find(|a| a.repo == slug && a.id == p.id),
        })
        .collect();
    for available in state.available.iter().filter(|a| a.repo == slug) {
        if !entries.iter().any(|e| e.id == available.id) {
            entries.push(RepoEntry {
                id: &available.id,
                name: &available.id,
                installed: None,
                available: Some(available),
            });
        }
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

pub(super) fn repo_detail_body(
    ui: &mut egui::Ui,
    r: &PluginRepoInfo,
    state: &PluginsState,
    view: &RepoView,
) -> Option<RepoAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        repo_icon_tile(ui, 44.0);
        ui.add_space(theme::SPACE_2);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(repo_name(r))
                        .font(theme::bold(18.0))
                        .color(theme::TEXT),
                );
                let (badges, _) = ui.allocate_exact_size(Vec2::new(39.0, 18.0), Sense::hover());
                draw_trust_badges(
                    ui,
                    badges.left(),
                    badges.center().y,
                    18.0,
                    repo_provenance(r),
                    &trust_label(r),
                );
            });
            ui.label(
                egui::RichText::new(repo_location_text(r))
                    .font(theme::body(10.0))
                    .color(theme::TEXT_MUT),
            );
            if let Some(fingerprint) = &r.fingerprint {
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

    let entries = repo_entries(&r.slug, state);
    ui.add_space(theme::SPACE_8);
    ui.columns(3, |cols| {
        stat_box(
            &mut cols[0],
            &t!("plugins.repo_last_sync"),
            &if is_local(r) {
                t!("plugins.repo_local").into_owned()
            } else {
                r.last_sync
                    .as_deref()
                    .map(format_last_sync)
                    .unwrap_or_else(|| t!("plugins.repo_never_synced").to_string())
            },
        );
        stat_box(
            &mut cols[1],
            &t!("plugins.repo_commit"),
            r.commit
                .as_deref()
                .map_or("—", |commit| &commit[..commit.len().min(8)]),
        );
        stat_box(
            &mut cols[2],
            &t!("plugins.repo_drivers"),
            &t!("plugins.repo_drivers_count", count = entries.len()),
        );
    });

    if !is_local(r) {
        ui.add_space(theme::SPACE_7);
        let size = Vec2::new(150.0, 32.0);
        if view.checking {
            widgets::button_loading(ui, &t!("plugins.repos_refresh"), ButtonKind::Primary, size);
        } else if widgets::button(ui, &t!("plugins.repos_refresh"), ButtonKind::Primary, size)
            .clicked()
        {
            action = Some(RepoAction::Refresh);
        }
    }

    ui.add_space(20.0);
    widgets::section_label(ui, &t!("plugins.repo_drivers_from"));
    ui.add_space(theme::SPACE_4);
    if entries.is_empty() {
        repo_no_drivers_box(ui);
    }
    for entry in &entries {
        let (rect, resp) = widgets::hover_row(ui, 42.0, false);
        let tile_rect = Rect::from_min_size(
            Pos2::new(rect.left() + 6.0, rect.center().y - 12.0),
            Vec2::splat(24.0),
        );
        initials_tile_at(ui, tile_rect, entry.name);
        let text_x = tile_rect.right() + 10.0;
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 6.0),
            Align2::LEFT_TOP,
            entry.name,
            theme::subhead(),
            theme::TEXT,
        );
        ui.painter().text(
            Pos2::new(text_x, rect.top() + 22.0),
            Align2::LEFT_TOP,
            entry_subtitle(entry),
            theme::value_xs(),
            theme::TEXT_FAINT,
        );
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(8.0, 6.0)))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        let installing = view.installing.contains_key(entry.id);
        let mut clicked = false;
        match entry.action() {
            Some(EntryAction::NeedsNewerDaemon) => {
                right.label(
                    egui::RichText::new(t!("plugins.requires_newer_daemon"))
                        .font(theme::body_sm())
                        .color(theme::WARN_TEXT),
                );
            }
            Some(kind) => {
                let label = if kind == EntryAction::Install {
                    t!("plugins.install")
                } else {
                    t!("plugins.update")
                };
                let size = Vec2::new(90.0, 28.0);
                if installing {
                    widgets::button_loading(&mut right, &label, ButtonKind::Secondary, size);
                } else {
                    clicked =
                        widgets::button(&mut right, &label, ButtonKind::Secondary, size).clicked();
                }
            }
            None => {}
        }
        if clicked {
            action = Some(RepoAction::Install(entry.id.to_owned()));
        } else if resp.clicked() && entry.installed.is_some() {
            action = Some(RepoAction::SelectPlugin(entry.id.to_owned()));
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

fn entry_subtitle(entry: &RepoEntry) -> String {
    match (entry.installed, entry.available) {
        (Some(installed), Some(available)) if is_newer(&available.version, installed) => {
            format!("{installed} → {}", available.version)
        }
        (Some(installed), _) => installed.to_owned(),
        (None, Some(available)) => {
            t!("plugins.not_installed_version", version = available.version).into_owned()
        }
        (None, None) => String::new(),
    }
}

pub(super) fn spawn_add_folder(ctx: &egui::Context, cmd: CommandTx) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            ipc::send(
                &cmd,
                DaemonCommand::AddPluginRepo {
                    source: PluginRepoLocation::Local(path.to_string_lossy().into_owned()),
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
    fn only_the_official_repo_carries_the_verified_badge_and_unsigned_is_open() {
        let icons_for = |trust, local: bool| {
            let mut r = repo("r");
            r.trust = trust;
            if local {
                r.location = PluginRepoLocation::Local("/tmp/r".into());
            }
            trust_badges(repo_provenance(&r))
                .iter()
                .map(|b| b.0)
                .collect::<Vec<_>>()
        };
        use icons::Icon::*;
        assert_eq!(icons_for(RepoTrust::Official, false), vec![Lock, Verified]);
        assert_eq!(icons_for(RepoTrust::Signed, false), vec![Lock]);
        assert_eq!(icons_for(RepoTrust::Unsigned, false), vec![LockOpen]);
        assert!(icons_for(RepoTrust::Official, true).is_empty());
    }

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
    fn repo_rows_puts_the_official_repo_first_regardless_of_slug() {
        let mut official = repo("zzz-official");
        official.official = true;
        let repos = vec![repo("alpha"), official];
        let rows = repo_rows(&repos);
        assert!(rows[0].official);
        assert_eq!(rows[1].slug, "alpha");
    }

    #[test]
    fn blank_urls_are_not_submitted() {
        assert_eq!(url_source("   "), None);
        assert_eq!(
            url_source(" https://x/catalog.yaml "),
            Some(PluginRepoLocation::Url("https://x/catalog.yaml".into()))
        );
    }

    #[test]
    fn repo_entries_merge_installed_and_available_plugins() {
        let mut state = PluginsState::default();
        let mut installed = info("kraken", true);
        installed.repo = "official".into();
        installed.version = "1.0.0".into();
        state.plugins = vec![installed, info("elsewhere", true)];
        state.available = vec![
            available("official", "kraken", "1.1.0", true),
            available("official", "aura", "2.0.0", true),
            available("official", "future", "1.0.0", false),
            available("other", "kraken", "9.0.0", true),
        ];

        let entries = repo_entries("official", &state);
        let summary: Vec<_> = entries.iter().map(|e| (e.id, e.action())).collect();
        assert_eq!(
            summary,
            [
                ("aura", Some(EntryAction::Install)),
                ("future", Some(EntryAction::NeedsNewerDaemon)),
                ("kraken", Some(EntryAction::Update)),
            ]
        );
    }

    #[test]
    fn repo_actions_dispatch_matching_commands() {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = repo("acme");
        let mut ui = PluginsUi::default();
        ui.apply_repo_action(1.0, &cmd, &r, RepoAction::Refresh);
        assert!(ui.checking_repo.is_some());
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::RefreshPluginRepos)
        ));
        ui.apply_repo_action(2.0, &cmd, &r, RepoAction::Install("p".into()));
        assert!(ui.installing.contains_key("p"));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonCommand::InstallPlugin { repo, id }) if repo == "acme" && id == "p"
        ));
        ui.apply_repo_action(3.0, &cmd, &r, RepoAction::Delete);
        assert_eq!(ui.pending_repo_delete.as_deref(), Some("acme"));
    }
}

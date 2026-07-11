// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugins page — a master–detail view of the Lua device plugins found in the
//! plugins directory (plus built-ins). The left column lists every plugin with
//! an enable toggle; the right column shows the selected plugin's detail. User
//! scripts can be added (upload a `.lua` file or paste source) and deleted;
//! built-ins can be toggled but not deleted.

use std::collections::{HashMap, HashSet};

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    AppState, PluginInfo, PluginRepoInfo, PluginSource, PluginUpdateStatus, RepoUpdateStatus,
};

use crate::runtime::ipc::{self, CommandTx};
use crate::ui::components::{self as widgets, ButtonKind};
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
    branch: String,
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
    /// Id pending a permission consent decision, when the dialog is open.
    pending_consent: Option<String>,
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
}

impl PluginsUi {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        plugin_assets: &HashMap<String, Vec<u8>>,
        repo_updates: &[RepoUpdateStatus],
        plugin_updates: &[PluginUpdateStatus],
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
        self.add_repo_modal(ui.ctx(), cmd);
        self.delete_modal(ui.ctx(), state, cmd);
        self.consent_modal(ui.ctx(), state, cmd);
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
                    crate::domain::actions::plugins::get_plugin_asset(cmd, plugin_id, name);
                }
            }
        }
        for (key, bytes) in plugin_assets {
            if self.asset_textures.contains_key(key) {
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
                self.asset_textures.insert(key.clone(), tex);
            }
        }
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

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd: &CommandTx,
        repo_updates: &[RepoUpdateStatus],
        plugin_updates: &[PluginUpdateStatus],
    ) {
        ui.label(
            egui::RichText::new(t!("plugins.title"))
                .font(theme::bold(22.0))
                .color(theme::TEXT),
        );
        ui.add_space(3.0);
        ui.label(
            egui::RichText::new(t!("plugins.subtitle"))
                .font(theme::body(12.0))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(18.0);

        if state.plugins.rediscover_pending {
            pending_changes_banner(ui, cmd);
            ui.add_space(18.0);
        }

        let due = plugin_updates.iter().filter(|s| s.update_available).count();
        if due > 0 {
            update_all_banner(ui, cmd, due);
            ui.add_space(18.0);
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
                    if widgets::button(
                        ui,
                        &t!("plugins.add"),
                        ButtonKind::Primary,
                        Vec2::new(96.0, 30.0),
                    )
                    .clicked()
                    {
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
                let updating: HashSet<&str> = plugin_updates
                    .iter()
                    .filter(|s| s.update_available)
                    .map(|s| s.plugin_id.as_str())
                    .collect();
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for p in &state.plugins.plugins {
                            let selected = self.selection == Selection::Plugin(p.id.clone());
                            let has_update = updating.contains(p.id.as_str());
                            let logo_tex = p
                                .logo
                                .as_deref()
                                .map(|name| ipc::plugin_asset_cache_key(&p.id, name))
                                .and_then(|key| self.asset_textures.get(&key));
                            match list_row(ui, p, selected, has_update, logo_tex) {
                                RowAction::Select => {
                                    self.selection = Selection::Plugin(p.id.clone())
                                }
                                RowAction::Toggle => {
                                    self.pending_consent =
                                        request_toggle(cmd, p, self.pending_consent.take())
                                }
                                RowAction::None => {}
                            }
                        }
                    });
            }

            ui.add_space(18.0);
            egui::Sides::new().show(
                ui,
                |ui| {
                    widgets::caps_label_inline(ui, &t!("plugins.repos_title"));
                },
                |ui| {
                    if widgets::button(ui, "+", ButtonKind::Ghost, Vec2::new(28.0, 26.0)).clicked()
                    {
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
                let mut select_plugin = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        widgets::card(ui, |ui| {
                            select_plugin = repo_detail_body(ui, r, &state.plugins.plugins, cmd);
                        });
                    });
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

    fn add_repo_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(mut form) = self.add_repo.take() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;

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
                ui.add(
                    egui::TextEdit::singleline(&mut form.url)
                        .desired_width(f32::INFINITY)
                        .hint_text(t!("plugins.repos_url_hint")),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut form.branch)
                        .desired_width(f32::INFINITY)
                        .hint_text(t!("plugins.repos_branch_hint")),
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

        if confirm {
            let url = form.url.trim().to_owned();
            if !url.is_empty() {
                let branch = form.branch.trim();
                let branch = if branch.is_empty() {
                    None
                } else {
                    Some(branch.to_owned())
                };
                crate::domain::actions::plugins::add_plugin_repo(cmd, url, branch);
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
            crate::domain::actions::plugins::delete_plugin(cmd, id);
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
                if p.content_changed {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(t!("plugins.consent_modified"))
                            .font(theme::body(11.5))
                            .color(theme::STAT_AMBER),
                    );
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
            crate::domain::actions::plugins::grant_and_enable(
                cmd,
                id,
                p.declared_permissions.clone(),
            );
            self.pending_consent = None;
        } else if cancel || dismissed {
            self.pending_consent = None;
        }
    }
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
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "⑂",
        theme::body(size * 0.36),
        theme::TEXT_MUT,
    );
}

/// One selectable repo row in the list column. Returns whether it was clicked.
fn repo_row(ui: &mut egui::Ui, row: &RepoRow, selected: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
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
    ui.painter().text(
        icon_rect.center(),
        Align2::CENTER_CENTER,
        "⑂",
        theme::body(12.0),
        theme::TEXT_MUT,
    );

    let text_x = rect.left() + 40.0;
    ui.painter().text(
        Pos2::new(text_x, rect.top() + 12.0),
        Align2::LEFT_TOP,
        row.slug,
        theme::semibold(12.0),
        theme::TEXT,
    );
    let sha_text = match &row.remote_short {
        Some(remote) => format!("{} → {}", row.locked_short, remote),
        None => row.locked_short.clone(),
    };
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

/// The file name shown for a plugin (the basename of its script path).
fn plugin_file_name(p: &PluginInfo) -> &str {
    std::path::Path::new(&p.path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p.path)
}

// ── Row + detail painters ───────────────────────────────────────────────────

enum RowAction {
    None,
    Select,
    Toggle,
}

/// True when `p` declares permissions the user hasn't consented to (never
/// granted, or the script changed since it was granted). It stays inert until
/// the user grants them. Shared with the Integrations screen, whose
/// integrations are plugins too.
pub(crate) fn plugin_needs_permission(p: &PluginInfo) -> bool {
    !p.declared_permissions.is_empty() && !p.consented
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
    /// Off and consent-satisfied → turn it on.
    Enable,
    /// Off and declares ungranted permissions → open the grant modal first.
    NeedsConsent,
}

/// Pure toggle logic: an active plugin turns off; a permission-free (or already
/// granted) one turns on; one needing permission must be granted first.
fn toggle_decision(p: &PluginInfo) -> ToggleDecision {
    if plugin_active(p) {
        ToggleDecision::Disable
    } else if plugin_needs_permission(p) {
        ToggleDecision::NeedsConsent
    } else {
        ToggleDecision::Enable
    }
}

/// Apply a toggle click through the consent gate. Enabling/disabling dispatches
/// immediately; a plugin needing permission returns its id as the new
/// `pending_consent` so the grant modal opens instead. Returns the
/// `pending_consent` to keep.
fn request_toggle(cmd: &CommandTx, p: &PluginInfo, pending: Option<String>) -> Option<String> {
    use crate::domain::actions::plugins::set_plugin_enabled;
    match toggle_decision(p) {
        ToggleDecision::Disable => {
            set_plugin_enabled(cmd, p.id.clone(), false);
            pending
        }
        ToggleDecision::Enable => {
            set_plugin_enabled(cmd, p.id.clone(), true);
            pending
        }
        ToggleDecision::NeedsConsent => Some(p.id.clone()),
    }
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
    has_update: bool,
    logo_tex: Option<&egui::TextureHandle>,
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
        Some(tex) => {
            ui.painter().image(
                tex.id(),
                tile_rect,
                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => initials_tile_at(ui, tile_rect, &p.name, &p.id),
    }

    let text_x = tile_rect.right() + 10.0;
    ui.painter().text(
        Pos2::new(text_x, rect.top() + 9.0),
        Align2::LEFT_TOP,
        &p.name,
        theme::semibold(12.5),
        theme::TEXT,
    );
    ui.painter().text(
        Pos2::new(text_x, rect.top() + 27.0),
        Align2::LEFT_TOP,
        plugin_file_name(p),
        theme::mono(9.5),
        theme::TEXT_FAINT,
    );
    if has_update {
        ui.painter().circle_filled(
            Pos2::new(tile_rect.right() - 2.0, tile_rect.top() + 2.0),
            4.0,
            theme::STAT_AMBER,
        );
    }
    let _ = status_dot(p); // kept for the toggle animation's initial state below

    // Toggle sits on top of the row; handle it before the row-select click.
    let toggle_rect = Rect::from_min_size(
        Pos2::new(rect.right() - 42.0, center_y - 9.0),
        Vec2::new(34.0, 18.0),
    );
    let tresp = ui.interact(
        toggle_rect,
        ui.id().with(("plugin_toggle", &p.id)),
        Sense::click(),
    );
    let t = ui
        .ctx()
        .animate_bool_with_time(tresp.id, plugin_active(p), 0.15);
    widgets::paint_toggle(ui.painter(), toggle_rect, t);
    if tresp.hovered() || resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if tresp.clicked() {
        RowAction::Toggle
    } else if resp.clicked() {
        RowAction::Select
    } else {
        RowAction::None
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
fn initials_tile_at(ui: &mut egui::Ui, rect: Rect, name: &str, id: &str) {
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

fn detail_body(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    cmd: &CommandTx,
    logo_tex: Option<&egui::TextureHandle>,
    update: Option<&PluginUpdateStatus>,
    pending_delete: &mut Option<String>,
    pending_consent: &mut Option<String>,
) {
    egui::Sides::new().show(
        ui,
        |ui| {
            match logo_tex {
                Some(tex) => {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                    ui.painter().image(
                        tex.id(),
                        rect,
                        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
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

    ui.add_space(14.0);
    status_banner(ui, p);

    if !p.license.is_empty() || !matches!(p.source, PluginSource::Local) {
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if !p.license.is_empty() {
                widgets::chip(ui, &format!("⚖ {}", p.license));
            }
            if let PluginSource::Repo { slug } = &p.source {
                widgets::chip(ui, &format!("⑂ {slug}"));
            }
        });
    }

    if let Some(update) = update.filter(|u| u.update_available) {
        ui.add_space(14.0);
        update_banner(
            ui,
            cmd,
            &update.plugin_id,
            &update.current_version,
            &update.available_version,
        );
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
        if widgets::button(ui, &label, kind, Vec2::new(120.0, 34.0)).clicked() {
            *pending_consent = request_toggle(cmd, p, pending_consent.take());
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
        Permission::Network => t!("plugins.permission_network"),
        Permission::Os => t!("plugins.permission_os"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage"),
    }
}

/// One line explaining what a permission lets the plugin do — and the risk —
/// so the user can make an informed grant decision.
fn permission_description(perm: halod_shared::types::Permission) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Network => t!("plugins.permission_network_desc"),
        Permission::Os => t!("plugins.permission_os_desc"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage_desc"),
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
        crate::domain::actions::plugins::revoke_and_disable(cmd, p.id.clone());
    }
}

/// Full-width call to action shown when one or more staged plugin edits
/// (enable/disable, grant/revoke, import, delete) haven't been applied to
/// live devices yet.
fn pending_changes_banner(ui: &mut egui::Ui, cmd: &CommandTx) {
    egui::Frame::NONE
        .fill(theme::a(theme::CYAN, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::CYAN, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("plugins.pending_changes"))
                            .font(theme::body(12.5))
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    if widgets::button(
                        ui,
                        &t!("plugins.apply_changes"),
                        ButtonKind::Primary,
                        Vec2::new(160.0, 32.0),
                    )
                    .clicked()
                    {
                        crate::domain::actions::plugins::apply_pending_plugin_changes(cmd);
                    }
                },
            );
        });
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

/// Amber "N plugin update(s) available / Update all" banner at the top of the page.
fn update_all_banner(ui: &mut egui::Ui, cmd: &CommandTx, count: usize) {
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter()
                            .circle_filled(r.center(), 3.5, theme::STAT_AMBER);
                        ui.label(
                            egui::RichText::new(t!("plugins.updates_available", count = count))
                                .font(theme::body(12.5))
                                .color(theme::TEXT),
                        );
                    });
                },
                |ui| {
                    if widgets::button(
                        ui,
                        &t!("plugins.update_all"),
                        ButtonKind::Primary,
                        Vec2::new(120.0, 32.0),
                    )
                    .clicked()
                    {
                        crate::domain::actions::plugins::update_all_plugins(cmd);
                    }
                },
            );
        });
}

/// Amber "Update available vX → vY / Update" banner in a plugin's detail. Never automatic.
fn update_banner(
    ui: &mut egui::Ui,
    cmd: &CommandTx,
    plugin_id: &str,
    current: &str,
    available: &str,
) {
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.10))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.35)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            egui::Sides::new().show(
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
                    if widgets::button(
                        ui,
                        &t!("plugins.repos_update"),
                        ButtonKind::Primary,
                        Vec2::new(90.0, 30.0),
                    )
                    .clicked()
                    {
                        crate::domain::actions::plugins::update_plugin(cmd, plugin_id.to_owned());
                    }
                },
            );
        });
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

/// The repo detail panel: header, stat boxes, "Check for updates", the list
/// of plugins it provides, and Remove (hidden for the official repo). Returns
/// a clicked plugin id, if any, so the caller can switch the selection to it.
fn repo_detail_body(
    ui: &mut egui::Ui,
    r: &PluginRepoInfo,
    plugins: &[PluginInfo],
    cmd: &CommandTx,
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
                    ui.label(
                        egui::RichText::new(&r.url)
                            .font(theme::mono(10.0))
                            .color(theme::TEXT_FAINT2),
                    );
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
            r.last_sync
                .as_deref()
                .unwrap_or(&t!("plugins.repo_never_synced")),
        );
        stat_box(
            &mut cols[2],
            &t!("plugins.repo_drivers"),
            &t!("plugins.repo_drivers_count", count = repo_plugins.len()),
        );
    });

    ui.add_space(14.0);
    if widgets::button(
        ui,
        &t!("plugins.repos_check_updates"),
        ButtonKind::Primary,
        Vec2::new(180.0, 32.0),
    )
    .clicked()
    {
        crate::domain::actions::plugins::check_plugin_updates(cmd, Some(r.slug.clone()));
    }

    ui.add_space(20.0);
    widgets::caps_label(ui, &t!("plugins.repo_drivers_from"));
    ui.add_space(8.0);

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
        let sub = if p.license.is_empty() {
            plugin_file_name(p).to_owned()
        } else {
            format!("{} · {}", plugin_file_name(p), p.license)
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
            crate::domain::actions::plugins::remove_plugin_repo(cmd, r.slug.clone());
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
            crate::domain::actions::plugins::import_plugin(&cmd, source_dir);
        }
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} device"),
            path: format!("/home/u/.config/halod/plugins/{id}.lua"),
            plugin_type: halod_shared::types::PluginKind::Device,
            capabilities: vec!["RGB".into()],
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
            declared_permissions: vec![],
            granted_permissions: vec![],
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
            integration_enabled: true,
            consented: true,
            content_changed: false,
        }
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

    fn repo(slug: &str, locked_sha: &str) -> PluginRepoInfo {
        PluginRepoInfo {
            url: format!("https://example.com/{slug}.git"),
            slug: slug.to_owned(),
            branch: None,
            locked_sha: locked_sha.to_owned(),
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
    fn toggle_decision_routes_through_the_consent_gate() {
        use halod_shared::types::Permission;
        // Permission-free, off → straight enable.
        let mut p = info("a", false);
        assert_eq!(toggle_decision(&p), ToggleDecision::Enable);

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

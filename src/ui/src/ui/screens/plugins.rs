// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugins page — a master–detail view of the Lua device plugins found in the
//! plugins directory (plus built-ins). The left column lists every plugin with
//! an enable toggle; the right column shows the selected plugin's detail. User
//! scripts can be added (upload a `.lua` file or paste source) and deleted;
//! built-ins can be toggled but not deleted.

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{AppState, PluginInfo};

use crate::runtime::ipc::CommandTx;
use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;

/// Which input the add-plugin modal is collecting.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum AddTab {
    #[default]
    Upload,
    Paste,
}

/// In-progress state of the add-plugin modal.
#[derive(Default)]
struct AddState {
    tab: AddTab,
    name: String,
    code: String,
}

/// Local UI state for the Plugins screen (selection + open dialogs).
#[derive(Default)]
pub struct PluginsUi {
    /// Id of the plugin shown in the detail column.
    selected: Option<String>,
    /// The add-plugin modal, when open.
    add: Option<AddState>,
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
    /// Local edit buffer for the selected plugin's config fields: `(plugin_id,
    /// field key -> typed value)`. Reset whenever the selection changes (see
    /// `seed_config_edit_if_needed`) — a secure field's buffer starts blank
    /// (never seeded from the stored secret), so leaving it blank on Save
    /// keeps the existing secret untouched.
    config_edit: Option<(String, std::collections::HashMap<String, String>)>,
}

impl PluginsUi {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        self.selected = resolve_selection(self.selected.as_deref(), &state.plugins);
        self.detect_new_plugin_needing_consent(&state.plugins);

        widgets::page_frame(ui, |ui| self.body(ui, state, cmd));

        self.add_modal(ui.ctx(), cmd);
        self.delete_modal(ui.ctx(), state, cmd);
        self.consent_modal(ui.ctx(), state, cmd);
    }

    /// A plugin id that appears now but wasn't present last frame opens the
    /// consent modal only when it arrived via our own import (see
    /// `awaiting_import`) — an auto-discovered plugin gets a toast instead
    /// (pushed by the daemon), not a blocking dialog.
    fn detect_new_plugin_needing_consent(&mut self, plugins: &[PluginInfo]) {
        let ids: std::collections::HashSet<String> = plugins.iter().map(|p| p.id.clone()).collect();
        if let Some(known) = &self.known_ids {
            let mut new_ids = plugins.iter().filter(|p| !known.contains(&p.id)).peekable();
            if self.awaiting_import && new_ids.peek().is_some() {
                if let Some(p) = new_ids.find(|p| plugin_needs_permission(p)) {
                    self.pending_consent = Some(p.id.clone());
                }
                self.awaiting_import = false;
            }
        }
        self.known_ids = Some(ids);
    }

    fn body(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
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

        if state.plugins_rediscover_pending {
            pending_changes_banner(ui, cmd);
            ui.add_space(18.0);
        }

        widgets::split_columns(ui, 320.0, 18.0, |left, right| {
            self.list_column(left, state, cmd);
            self.detail_column(right, state, cmd);
        });
    }

    // ── Left: plugin list ───────────────────────────────────────────────────

    fn list_column(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        widgets::card(ui, |ui| {
            let active = state.plugins.iter().filter(|p| plugin_active(p)).count();
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
                                count = state.plugins.len(),
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
                        self.add = Some(AddState::default());
                    }
                },
            );
            ui.add_space(12.0);

            if state.plugins.is_empty() {
                ui.label(
                    egui::RichText::new(t!("plugins.empty_title"))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_MUT),
                );
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    for p in &state.plugins {
                        let selected = self.selected.as_deref() == Some(p.id.as_str());
                        match list_row(ui, p, selected) {
                            RowAction::Select => self.selected = Some(p.id.clone()),
                            RowAction::Toggle => {
                                crate::domain::actions::plugins::set_plugin_enabled(
                                    cmd,
                                    p.id.clone(),
                                    !p.enabled,
                                );
                            }
                            RowAction::None => {}
                        }
                    }
                });
        });
    }

    // ── Right: detail ───────────────────────────────────────────────────────

    fn detail_column(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        let Some(p) = self
            .selected
            .as_deref()
            .and_then(|id| state.plugins.iter().find(|p| p.id == id))
        else {
            widgets::empty_state(
                ui,
                &t!("plugins.empty_title"),
                Some(&t!("plugins.empty_hint")),
            );
            return;
        };

        seed_config_edit_if_needed(&mut self.config_edit, &p.id, &p.config_values);
        let edits = &mut self.config_edit.as_mut().expect("just seeded above").1;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::card(ui, |ui| {
                    detail_body(ui, p, cmd, &mut self.pending_delete, edits)
                });
            });
    }

    // ── Dialogs ─────────────────────────────────────────────────────────────

    fn add_modal(&mut self, ctx: &egui::Context, cmd: &CommandTx) {
        let Some(mut add) = self.add.take() else {
            return;
        };
        let tab = add.tab;
        let mut pick_file = false;
        let mut confirm = false;
        let mut cancel = false;

        let dismissed = widgets::dialog(
            ctx,
            "add_plugin",
            &t!("plugins.add_title"),
            480.0,
            |ui| add_body(ui, &mut add),
            |ui| {
                // `dialog` lays actions out right-to-left; add the primary first.
                let label = match tab {
                    AddTab::Upload => t!("plugins.choose_file"),
                    AddTab::Paste => t!("plugins.add"),
                };
                if widgets::button(ui, &label, ButtonKind::Primary, Vec2::new(130.0, 34.0))
                    .clicked()
                {
                    match tab {
                        AddTab::Upload => pick_file = true,
                        AddTab::Paste => confirm = true,
                    }
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

        if pick_file {
            // Optimistic: the file dialog may still be cancelled, in which case
            // no plugin ever appears and this flag is simply never consumed.
            self.awaiting_import = true;
            spawn_import_plugin(ctx, cmd.clone());
            return; // modal closes; import completes when the user picks a file
        }
        if confirm {
            let code = add.code.trim();
            if !code.is_empty() {
                let filename = add_filename(&add.name);
                self.awaiting_import = true;
                crate::domain::actions::plugins::import_plugin(cmd, filename, add.code.clone());
                return;
            }
        }
        if cancel || dismissed {
            return;
        }
        self.add = Some(add);
    }

    fn delete_modal(&mut self, ctx: &egui::Context, state: &AppState, cmd: &CommandTx) {
        if self.pending_delete.is_none() {
            return;
        }
        let name = self
            .pending_delete
            .as_deref()
            .and_then(|id| state.plugins.iter().find(|p| p.id == id))
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

    /// Consent prompt shown the moment a freshly-imported plugin declares
    /// permissions. Grant accepts them; Deny leaves the plugin installed but
    /// inert; Remove deletes the script outright.
    fn consent_modal(&mut self, ctx: &egui::Context, state: &AppState, cmd: &CommandTx) {
        let Some(id) = self.pending_consent.clone() else {
            return;
        };
        let Some(p) = state.plugins.iter().find(|p| p.id == id) else {
            self.pending_consent = None;
            return;
        };

        let mut grant = false;
        let mut deny = false;
        let mut remove = false;
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
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    for perm in &p.declared_permissions {
                        let _ =
                            widgets::chip_colored(ui, &permission_label(*perm), theme::STAT_AMBER);
                    }
                });
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(t!("plugins.consent_warning"))
                        .font(theme::body(11.5))
                        .color(theme::TEXT_MUT),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("plugins.grant"),
                    ButtonKind::Primary,
                    Vec2::new(150.0, 34.0),
                )
                .clicked()
                {
                    grant = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.consent_deny"),
                    ButtonKind::Ghost,
                    Vec2::new(90.0, 34.0),
                )
                .clicked()
                {
                    deny = true;
                }
                if widgets::button(
                    ui,
                    &t!("plugins.delete"),
                    ButtonKind::Danger,
                    Vec2::new(120.0, 34.0),
                )
                .clicked()
                {
                    remove = true;
                }
            },
        );

        if grant {
            crate::domain::actions::plugins::grant_plugin_permissions(
                cmd,
                id,
                p.declared_permissions.clone(),
            );
            self.pending_consent = None;
        } else if remove {
            crate::domain::actions::plugins::delete_plugin(cmd, id);
            self.pending_consent = None;
        } else if deny || dismissed {
            self.pending_consent = None;
        }
    }
}

/// The plugin the detail column should show: keep the current selection if it
/// still exists, otherwise fall back to the first plugin (or `None` if empty).
fn resolve_selection(current: Option<&str>, plugins: &[PluginInfo]) -> Option<String> {
    if let Some(id) = current {
        if plugins.iter().any(|p| p.id == id) {
            return Some(id.to_owned());
        }
    }
    plugins.first().map(|p| p.id.clone())
}

/// The file name shown for a plugin (the basename of its script path).
fn plugin_file_name(p: &PluginInfo) -> &str {
    std::path::Path::new(&p.path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p.path)
}

/// Build the suggested script file name from the modal's name field. The daemon
/// re-sanitizes, so this only needs to be a reasonable default.
fn add_filename(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        "plugin.lua".to_owned()
    } else {
        format!("{name}.lua")
    }
}

// ── Row + detail painters ───────────────────────────────────────────────────

enum RowAction {
    None,
    Select,
    Toggle,
}

/// True when `p` declares at least one permission it hasn't been granted —
/// it stays inert regardless of its enable/disable toggle until resolved.
fn plugin_needs_permission(p: &PluginInfo) -> bool {
    !p.declared_permissions.is_empty() && !permissions_satisfied(p)
}

/// True when the plugin is actually running: toggled on AND (if it declares
/// any) every permission granted.
fn plugin_active(p: &PluginInfo) -> bool {
    p.enabled && !plugin_needs_permission(p)
}

fn status_dot(p: &PluginInfo) -> egui::Color32 {
    if plugin_needs_permission(p) {
        theme::STAT_AMBER
    } else if plugin_active(p) {
        theme::ONLINE
    } else {
        theme::TEXT_FAINT2
    }
}

fn list_row(ui: &mut egui::Ui, p: &PluginInfo, selected: bool) -> RowAction {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 9.0, theme::ROW_ACTIVE);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 9.0, theme::a(theme::ROW_ACTIVE, 0.55));
    }
    let center_y = rect.center().y;
    ui.painter()
        .circle_filled(Pos2::new(rect.left() + 12.0, center_y), 3.5, status_dot(p));

    let text_x = rect.left() + 26.0;
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
    let t = ui.ctx().animate_bool_with_time(tresp.id, p.enabled, 0.15);
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

fn lua_badge(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, 10.0, theme::hex(0x191527));
    ui.painter().rect_stroke(
        rect,
        10.0,
        Stroke::new(1.0, theme::a(theme::CYAN, 0.45)),
        egui::StrokeKind::Middle,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "lua",
        theme::mono_semibold(10.0),
        theme::CYAN,
    );
}

fn detail_body(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    cmd: &CommandTx,
    pending_delete: &mut Option<String>,
    config_edits: &mut std::collections::HashMap<String, String>,
) {
    egui::Sides::new().show(
        ui,
        |ui| {
            lua_badge(ui, 44.0);
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

    if p.plugin_type == halod_shared::types::PluginKind::Device {
        if !p.capabilities.is_empty() {
            ui.add_space(16.0);
            widgets::caps_label(ui, &t!("plugins.capabilities"));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for c in &p.capabilities {
                    widgets::chip(ui, c);
                }
            });
        }

        if !p.targets.is_empty() {
            ui.add_space(16.0);
            widgets::caps_label(ui, &t!("plugins.targets"));
            ui.add_space(4.0);
            for target in &p.targets {
                ui.label(
                    egui::RichText::new(target)
                        .font(theme::body(12.0))
                        .color(theme::TEXT_DIM),
                );
            }
        }
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

    if !p.declared_permissions.is_empty() {
        ui.add_space(16.0);
        permissions_section(ui, p, cmd);
    }

    if !p.config_fields.is_empty() {
        ui.add_space(16.0);
        config_section(ui, p, cmd, config_edits);
    }

    ui.add_space(20.0);
    ui.separator();
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        let label = if p.enabled {
            t!("plugins.disable")
        } else {
            t!("plugins.enable")
        };
        let kind = if p.enabled {
            ButtonKind::Ghost
        } else {
            ButtonKind::Primary
        };
        if widgets::button(ui, &label, kind, Vec2::new(120.0, 34.0)).clicked() {
            crate::domain::actions::plugins::set_plugin_enabled(cmd, p.id.clone(), !p.enabled);
        }
        if p.builtin {
            widgets::caps_label_inline(ui, &t!("plugins.builtin_note"));
        } else if widgets::button(
            ui,
            &t!("plugins.delete"),
            ButtonKind::Danger,
            Vec2::new(120.0, 34.0),
        )
        .clicked()
        {
            *pending_delete = Some(p.id.clone());
        }
    });
}

fn plugin_type_label(kind: halod_shared::types::PluginKind) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::PluginKind;
    match kind {
        PluginKind::Device => t!("plugins.type_device"),
        PluginKind::Effect => t!("plugins.type_effect"),
    }
}

fn plugin_type_color(kind: halod_shared::types::PluginKind) -> egui::Color32 {
    use halod_shared::types::PluginKind;
    match kind {
        PluginKind::Device => theme::STAT_CYAN,
        PluginKind::Effect => theme::STAT_PURPLE,
    }
}

/// Whether every permission `p` declares has been granted.
fn permissions_satisfied(p: &PluginInfo) -> bool {
    p.declared_permissions
        .iter()
        .all(|perm| p.granted_permissions.contains(perm))
}

fn permission_label(perm: halod_shared::types::Permission) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Network => t!("plugins.permission_network"),
        Permission::Os => t!("plugins.permission_os"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage"),
    }
}

fn permissions_section(ui: &mut egui::Ui, p: &PluginInfo, cmd: &CommandTx) {
    widgets::caps_label(ui, &t!("plugins.permissions"));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        for perm in &p.declared_permissions {
            let granted = p.granted_permissions.contains(perm);
            let color = if granted {
                theme::ONLINE
            } else {
                theme::TEXT_FAINT
            };
            let _ = widgets::chip_colored(ui, &permission_label(*perm), color);
        }
    });

    if !permissions_satisfied(p) {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(t!("plugins.permissions_pending"))
                .font(theme::body(11.5))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(8.0);
        if widgets::button(
            ui,
            &t!("plugins.grant"),
            ButtonKind::Primary,
            Vec2::new(160.0, 30.0),
        )
        .clicked()
        {
            crate::domain::actions::plugins::grant_plugin_permissions(
                cmd,
                p.id.clone(),
                p.declared_permissions.clone(),
            );
        }
    } else {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t!("plugins.permissions_granted"))
                    .font(theme::body(11.5))
                    .color(theme::ONLINE_TEXT),
            );
            if widgets::button(
                ui,
                &t!("plugins.revoke"),
                ButtonKind::Ghost,
                Vec2::new(100.0, 26.0),
            )
            .clicked()
            {
                crate::domain::actions::plugins::revoke_plugin_permissions(cmd, p.id.clone());
            }
        });
    }
}

/// Reset the edit buffer when the selection moves to a different plugin, so
/// stale text from a previous plugin's fields never leaks into another's. A
/// secure field's buffer always starts blank — never seeded from
/// `config_values` (which never carries a secret's plaintext) — so an
/// untouched secure field sends nothing and the stored secret is left alone.
fn seed_config_edit_if_needed(
    edit: &mut Option<(String, std::collections::HashMap<String, String>)>,
    plugin_id: &str,
    config_values: &std::collections::HashMap<String, String>,
) {
    if edit.as_ref().map(|(id, _)| id.as_str()) != Some(plugin_id) {
        *edit = Some((plugin_id.to_owned(), config_values.clone()));
    }
}

/// What `Save` actually sends: every non-secure field's current buffer value,
/// plus a secure field's value only when the user typed something into it —
/// an empty secure buffer means "leave the stored secret unchanged" (see
/// `SetPluginConfig`).
fn config_values_to_send(
    edits: &std::collections::HashMap<String, String>,
    fields: &[halod_shared::types::PluginConfigField],
) -> std::collections::HashMap<String, String> {
    fields
        .iter()
        .filter_map(|f| {
            let v = edits.get(&f.key)?;
            if f.secure && v.is_empty() {
                None
            } else {
                Some((f.key.clone(), v.clone()))
            }
        })
        .collect()
}

fn config_section(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    cmd: &CommandTx,
    edits: &mut std::collections::HashMap<String, String>,
) {
    use halod_shared::types::PluginConfigFieldKind;

    widgets::caps_label(ui, &t!("plugins.settings"));
    ui.add_space(6.0);

    let mut groups: std::collections::BTreeMap<
        String,
        Vec<&halod_shared::types::PluginConfigField>,
    > = std::collections::BTreeMap::new();
    for f in &p.config_fields {
        groups.entry(f.category.clone()).or_default().push(f);
    }

    for (category, fields) in &groups {
        if !category.is_empty() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(category.as_str())
                    .font(theme::body(11.0))
                    .color(theme::TEXT_FAINT),
            );
        }
        for f in fields {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(&f.label)
                    .font(theme::body(12.0))
                    .color(theme::TEXT_DIM),
            );
            let buf = edits.entry(f.key.clone()).or_default();
            let hint = if f.secure && p.secret_set.get(&f.key).copied().unwrap_or(false) {
                t!("plugins.secret_set_hint")
            } else {
                std::borrow::Cow::Borrowed("")
            };
            ui.horizontal(|ui| {
                let mut edit = egui::TextEdit::singleline(buf).desired_width(220.0);
                if f.secure {
                    edit = edit.password(true);
                }
                if matches!(f.kind, PluginConfigFieldKind::Number) {
                    // Numeric fields are still stored/sent as strings (the
                    // plugin/Lua side interprets them); this only nudges the
                    // on-screen keyboard/validation, it doesn't change the type.
                    edit = edit.hint_text("0");
                }
                ui.add(edit);
                if !hint.is_empty() {
                    ui.label(
                        egui::RichText::new(hint.as_ref())
                            .font(theme::body(11.0))
                            .color(theme::TEXT_FAINT),
                    );
                }
            });
        }
    }

    ui.add_space(10.0);
    if widgets::button(
        ui,
        &t!("plugins.save_settings"),
        ButtonKind::Primary,
        Vec2::new(140.0, 30.0),
    )
    .clicked()
    {
        let values = config_values_to_send(edits, &p.config_fields);
        crate::domain::actions::plugins::set_plugin_config(cmd, p.id.clone(), values);
        // Blank out secure buffers again post-send so a stored secret's typed
        // replacement doesn't linger on screen after Save.
        for f in &p.config_fields {
            if f.secure {
                edits.insert(f.key.clone(), String::new());
            }
        }
    }
}

/// Full-width call to action shown when one or more staged plugin edits
/// (enable/disable, grant/revoke, import, delete) haven't been applied to
/// live devices yet.
fn pending_changes_banner(ui: &mut egui::Ui, cmd: &CommandTx) {
    egui::Frame::NONE
        .fill(theme::a(theme::STAT_AMBER, 0.12))
        .stroke(Stroke::new(1.0, theme::a(theme::STAT_AMBER, 0.4)))
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
    let (dot, text, color) = if plugin_needs_permission(p) {
        (
            theme::STAT_AMBER,
            t!("plugins.status_needs_permission"),
            theme::STAT_AMBER,
        )
    } else if plugin_active(p) {
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

// ── Add-plugin modal body ───────────────────────────────────────────────────

fn add_body(ui: &mut egui::Ui, add: &mut AddState) {
    ui.label(
        egui::RichText::new(t!("plugins.add_sub"))
            .font(theme::body(11.5))
            .color(theme::TEXT_MUT),
    );
    ui.add_space(14.0);

    ui.horizontal(|ui| {
        if widgets::pill(ui, &t!("plugins.tab_upload"), add.tab == AddTab::Upload) {
            add.tab = AddTab::Upload;
        }
        if widgets::pill(ui, &t!("plugins.tab_paste"), add.tab == AddTab::Paste) {
            add.tab = AddTab::Paste;
        }
    });
    ui.add_space(14.0);

    match add.tab {
        AddTab::Upload => {
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
        AddTab::Paste => {
            ui.add(
                egui::TextEdit::multiline(&mut add.code)
                    .font(theme::mono(11.5))
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
                    .hint_text(t!("plugins.paste_hint")),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(t!("plugins.name"))
                        .font(theme::body(11.5))
                        .color(theme::TEXT_MUT),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut add.name)
                        .desired_width(f32::INFINITY)
                        .hint_text(t!("plugins.name_hint")),
                );
            });
        }
    }
}

/// Open a native `.lua` picker on a background thread, read the file, and send
/// an import command straight from the thread (the command channel is cheap to
/// clone). Mirrors `effect_designer::spawn_import`.
fn spawn_import_plugin(ctx: &egui::Context, cmd: CommandTx) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Lua plugin", &["lua"])
            .pick_file()
        {
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    let filename = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("plugin.lua")
                        .to_owned();
                    crate::domain::actions::plugins::import_plugin(&cmd, filename, source);
                }
                Err(e) => log::warn!("failed to read plugin {path:?}: {e}"),
            }
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
            builtin: false,
            declared_permissions: vec![],
            granted_permissions: vec![],
            config_fields: vec![],
            config_values: Default::default(),
            secret_set: Default::default(),
        }
    }

    #[test]
    fn selection_keeps_valid_current() {
        let plugins = vec![info("a", true), info("b", false)];
        assert_eq!(resolve_selection(Some("b"), &plugins).as_deref(), Some("b"));
    }

    #[test]
    fn selection_falls_back_to_first_when_missing_or_none() {
        let plugins = vec![info("a", true), info("b", false)];
        assert_eq!(
            resolve_selection(Some("gone"), &plugins).as_deref(),
            Some("a")
        );
        assert_eq!(resolve_selection(None, &plugins).as_deref(), Some("a"));
    }

    #[test]
    fn selection_is_none_for_empty_list() {
        assert_eq!(resolve_selection(Some("a"), &[]), None);
        assert_eq!(resolve_selection(None, &[]), None);
    }

    #[test]
    fn file_name_is_basename() {
        assert_eq!(plugin_file_name(&info("kraken", true)), "kraken.lua");
        let mut p = info("x", true);
        p.path = "ene_smbus.lua".into();
        assert_eq!(plugin_file_name(&p), "ene_smbus.lua");
    }

    #[test]
    fn add_filename_defaults_and_appends_extension() {
        assert_eq!(add_filename("  "), "plugin.lua");
        assert_eq!(add_filename(" Nanoleaf "), "Nanoleaf.lua");
    }

    #[test]
    fn status_dot_reflects_enabled() {
        assert_eq!(status_dot(&info("a", true)), theme::ONLINE);
        assert_eq!(status_dot(&info("a", false)), theme::TEXT_FAINT2);
    }

    #[test]
    fn permissions_satisfied_when_no_permissions_declared() {
        assert!(permissions_satisfied(&info("a", true)));
    }

    #[test]
    fn permissions_unsatisfied_until_every_declared_permission_is_granted() {
        use halod_shared::types::Permission;
        let mut p = info("a", true);
        p.declared_permissions = vec![Permission::Network, Permission::Os];
        assert!(!permissions_satisfied(&p), "nothing granted yet");

        p.granted_permissions = vec![Permission::Network];
        assert!(!permissions_satisfied(&p), "partial grant is not enough");

        p.granted_permissions = vec![Permission::Network, Permission::Os];
        assert!(permissions_satisfied(&p), "fully granted");
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

    fn field(key: &str, secure: bool) -> halod_shared::types::PluginConfigField {
        halod_shared::types::PluginConfigField {
            key: key.to_string(),
            label: key.to_string(),
            kind: halod_shared::types::PluginConfigFieldKind::Text,
            category: String::new(),
            secure,
        }
    }

    #[test]
    fn seed_config_edit_initializes_from_config_values_on_first_use() {
        let mut edit = None;
        let values = std::collections::HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);
        seed_config_edit_if_needed(&mut edit, "openrgb", &values);
        assert_eq!(edit, Some(("openrgb".to_string(), values)));
    }

    #[test]
    fn seed_config_edit_resets_on_plugin_change_but_not_same_plugin() {
        let mut edit = Some((
            "openrgb".to_string(),
            std::collections::HashMap::from([("host".to_string(), "typed-value".to_string())]),
        ));
        let stored = std::collections::HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);

        // Same plugin still selected: keep whatever the user is typing.
        seed_config_edit_if_needed(&mut edit, "openrgb", &stored);
        assert_eq!(
            edit.as_ref().unwrap().1.get("host"),
            Some(&"typed-value".to_string())
        );

        // Selection moved to a different plugin: reseed from its own values.
        seed_config_edit_if_needed(&mut edit, "other", &stored);
        assert_eq!(edit, Some(("other".to_string(), stored)));
    }

    #[test]
    fn config_values_to_send_always_includes_non_secure_fields() {
        let edits = std::collections::HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);
        let fields = vec![field("host", false)];
        let sent = config_values_to_send(&edits, &fields);
        assert_eq!(sent.get("host"), Some(&"1.2.3.4".to_string()));
    }

    #[test]
    fn config_values_to_send_omits_a_blank_secure_field() {
        let edits = std::collections::HashMap::from([("token".to_string(), String::new())]);
        let fields = vec![field("token", true)];
        let sent = config_values_to_send(&edits, &fields);
        assert!(
            !sent.contains_key("token"),
            "an untouched secure field must not be sent, so the stored secret is left alone"
        );
    }

    #[test]
    fn config_values_to_send_includes_a_typed_secure_field() {
        let edits =
            std::collections::HashMap::from([("token".to_string(), "new-secret".to_string())]);
        let fields = vec![field("token", true)];
        let sent = config_values_to_send(&edits, &fields);
        assert_eq!(sent.get("token"), Some(&"new-secret".to_string()));
    }

    #[test]
    fn config_values_to_send_skips_fields_with_no_buffer_entry() {
        let edits = std::collections::HashMap::new();
        let fields = vec![field("host", false)];
        assert!(config_values_to_send(&edits, &fields).is_empty());
    }
}

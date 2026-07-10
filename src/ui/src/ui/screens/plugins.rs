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
}

impl PluginsUi {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        self.selected = resolve_selection(self.selected.as_deref(), &state.plugins);

        widgets::page_frame(ui, |ui| self.body(ui, state, cmd));

        self.add_modal(ui.ctx(), cmd);
        self.delete_modal(ui.ctx(), state, cmd);
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

        widgets::split_columns(ui, 320.0, 18.0, |left, right| {
            self.list_column(left, state, cmd);
            self.detail_column(right, state, cmd);
        });
    }

    // ── Left: plugin list ───────────────────────────────────────────────────

    fn list_column(&mut self, ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
        widgets::card(ui, |ui| {
            let active = state.plugins.iter().filter(|p| p.enabled).count();
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

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::card(ui, |ui| detail_body(ui, p, cmd, &mut self.pending_delete));
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
            spawn_import_plugin(ctx, cmd.clone());
            return; // modal closes; import completes when the user picks a file
        }
        if confirm {
            let code = add.code.trim();
            if !code.is_empty() {
                let filename = add_filename(&add.name);
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

fn status_dot(p: &PluginInfo) -> egui::Color32 {
    if p.enabled {
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
) {
    ui.horizontal(|ui| {
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
    });

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

fn status_banner(ui: &mut egui::Ui, p: &PluginInfo) {
    let (dot, text, color) = if p.enabled {
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
            capabilities: vec!["RGB".into()],
            enabled,
            author: "Someone".into(),
            version: "1.0.0".into(),
            description: "desc".into(),
            targets: vec!["Acme K1".into()],
            builtin: false,
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
}

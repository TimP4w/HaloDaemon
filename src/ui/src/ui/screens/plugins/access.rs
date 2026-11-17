// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use halod_shared::ids::PluginId;

pub(super) fn permission_label(
    perm: halod_shared::types::Permission,
) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Hid => t!("plugins.permission_hid"),
        Permission::Usb => t!("plugins.permission_usb"),
        Permission::Hwmon => t!("plugins.permission_hwmon"),
        Permission::Lpcio => t!("plugins.permission_lpcio"),
        Permission::AmdSmn => t!("plugins.permission_amd_smn"),
        Permission::Command => t!("plugins.permission_command"),
        Permission::Serial => t!("plugins.permission_serial"),
        Permission::Network => t!("plugins.permission_network"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage"),
        Permission::Smbus => t!("plugins.permission_smbus"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing"),
    }
}

pub(super) fn permission_description(
    perm: halod_shared::types::Permission,
) -> std::borrow::Cow<'static, str> {
    use halod_shared::types::Permission;
    match perm {
        Permission::Hid => t!("plugins.permission_hid_desc"),
        Permission::Usb => t!("plugins.permission_usb_desc"),
        Permission::Hwmon => t!("plugins.permission_hwmon_desc"),
        Permission::Lpcio => t!("plugins.permission_lpcio_desc"),
        Permission::AmdSmn => t!("plugins.permission_amd_smn_desc"),
        Permission::Command => t!("plugins.permission_command_desc"),
        Permission::Serial => t!("plugins.permission_serial_desc"),
        Permission::Network => t!("plugins.permission_network_desc"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage_desc"),
        Permission::Smbus => t!("plugins.permission_smbus_desc"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing_desc"),
    }
}

pub(super) fn data_reads_detail(keys: &[String]) -> String {
    format!(
        "{} {}",
        t!("plugins.permission_data_reads_desc"),
        keys.join(", ")
    )
}

/// One `(title, detail)` row per thing the manifest declares it uses.
pub(super) fn access_rows(access: &halod_shared::types::PackageUses) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = access
        .permissions
        .iter()
        .map(|&perm| {
            (
                permission_label(perm).into_owned(),
                permission_description(perm).into_owned(),
            )
        })
        .collect();
    if !access.commands.is_empty() {
        rows.push((
            t!("plugins.uses_commands").into_owned(),
            access.commands.join(", "),
        ));
    }
    if !access.data_reads.is_empty() {
        rows.push((
            t!("plugins.permission_data_reads").into_owned(),
            data_reads_detail(&access.data_reads),
        ));
    }
    rows
}

/// "Capabilities | Target devices" side by side; either column is omitted
/// when empty.
pub(super) fn capabilities_targets_row(ui: &mut egui::Ui, p: &PluginInfo) {
    let has_caps =
        p.plugin_type == halod_shared::types::PluginKind::Device && !p.capabilities.is_empty();
    let has_targets =
        p.plugin_type == halod_shared::types::PluginKind::Device && !p.targets.is_empty();
    if !has_caps && !has_targets {
        return;
    }
    ui.add_space(theme::SPACE_9);
    ui.spacing_mut().item_spacing.x = 20.0;
    ui.columns(2, |cols| {
        if has_caps {
            widgets::section_label(&mut cols[0], &t!("plugins.capabilities"));
            cols[0].add_space(theme::SPACE_4);
            cols[0].horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(theme::SPACE_3);
                for c in &p.capabilities {
                    widgets::tag(ui, c, widgets::TagTone::Neutral);
                }
            });
        }
        if has_targets {
            widgets::section_label(&mut cols[1], &t!("plugins.targets"));
            cols[1].add_space(theme::SPACE_4);
            for target in &p.targets {
                cols[1].label(
                    egui::RichText::new(target)
                        .font(theme::body(14.0))
                        .color(theme::TEXT),
                );
            }
        }
    });
}

pub(super) fn uses_section(ui: &mut egui::Ui, p: &PluginInfo) {
    let rows = access_rows(&p.uses);
    if rows.is_empty() {
        return;
    }
    ui.add_space(theme::SPACE_9);
    widgets::divider(ui);
    ui.add_space(theme::SPACE_8);
    let top = ui.cursor().min;
    widgets::section_label(ui, &t!("plugins.uses"));
    ui.add_space(theme::SPACE_4);
    access_list(ui, &rows);
    tour::anchor(
        ui.ctx(),
        AnchorId::PluginsPermissions,
        egui::Rect::from_min_max(top, ui.cursor().min + Vec2::new(ui.available_width(), 0.0)),
    );
}

fn access_list(ui: &mut egui::Ui, rows: &[(String, String)]) {
    for (title, desc) in rows {
        ui.label(
            egui::RichText::new(title)
                .font(theme::medium(14.0))
                .color(theme::TEXT),
        );
        ui.label(
            egui::RichText::new(desc)
                .font(theme::body_md())
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::SPACE_4);
    }
}

/// Plugins waiting for the user to confirm enabling them.
#[derive(Default)]
pub(crate) struct EnablePrompt {
    queue: Vec<PluginId>,
}

impl EnablePrompt {
    pub(crate) fn request(&mut self, id: &PluginId) {
        if !self.queue.iter().any(|queued| queued == id) {
            self.queue.push(id.into());
        }
    }

    pub(super) fn current<'a>(&mut self, plugins: &'a [PluginInfo]) -> Option<&'a PluginInfo> {
        self.queue
            .retain(|id| !plugins.iter().any(|p| p.id == id && p.enabled));
        self.queue
            .iter()
            .find_map(|id| plugins.iter().find(|p| p.id == id))
    }

    pub(super) fn resolve(&mut self, cmd: &CommandTx, id: &PluginId, enable: bool) -> bool {
        self.queue.retain(|queued| queued != id);
        if enable {
            ipc::send(
                cmd,
                DaemonCommand::SetPluginEnabled {
                    id: id.clone(),
                    enabled: true,
                },
            );
        }
        enable
    }

    /// Returns the id the user just enabled.
    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        plugins: &[PluginInfo],
        cmd: &CommandTx,
    ) -> Option<PluginId> {
        let plugin = self.current(plugins)?;
        let enable = access_prompt(
            ctx,
            "plugin_enable_prompt",
            AccessPromptText {
                title: t!("plugins.enable_prompt_title", name = plugin_name(plugin)).into_owned(),
                intro: t!("plugins.enable_prompt_intro").into_owned(),
                confirm: t!("plugins.enable_prompt_confirm").into_owned(),
                decline: t!("plugins.enable_prompt_decline").into_owned(),
            },
            &access_rows(&plugin.uses),
        )?;
        let id = plugin.id.clone();
        self.resolve(cmd, &id, enable).then_some(id)
    }
}

struct AccessPromptText {
    title: String,
    intro: String,
    confirm: String,
    decline: String,
}

fn access_prompt(
    ctx: &egui::Context,
    id: &str,
    text: AccessPromptText,
    rows: &[(String, String)],
) -> Option<bool> {
    let mut choice = None;
    let dismissed = widgets::dialog(
        ctx,
        id,
        &text.title,
        460.0,
        |ui| {
            let intro = if rows.is_empty() {
                t!("plugins.enable_prompt_nothing").into_owned()
            } else {
                text.intro.clone()
            };
            ui.label(
                egui::RichText::new(intro)
                    .font(theme::body_md())
                    .color(theme::TEXT_SOFT),
            );
            ui.add_space(theme::SPACE_6);
            access_list(ui, rows);
        },
        |ui| {
            if widgets::button(ui, &text.confirm, ButtonKind::Primary, Vec2::new(0.0, 36.0))
                .clicked()
            {
                choice = Some(true);
            }
            if widgets::button(
                ui,
                &text.decline,
                ButtonKind::Secondary,
                Vec2::new(0.0, 36.0),
            )
            .clicked()
            {
                choice = Some(false);
            }
        },
    );
    choice.or(dismissed.then_some(false))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedUpdate {
    pub(super) repo: String,
    pub(super) id: PluginId,
    pub(super) needs_permissions: bool,
    pub(super) confirmed: Option<bool>,
}

impl PlannedUpdate {
    pub(super) fn permissions_confirmed(&self) -> bool {
        self.needs_permissions && self.confirmed == Some(true)
    }
}

pub(super) fn plan_updates(state: &PluginsState, ids: &[&PluginId]) -> Vec<PlannedUpdate> {
    ids.iter()
        .filter_map(|id| {
            let plugin = state.plugins.iter().find(|p| p.id == **id)?;
            let update = update_for(plugin, &state.available)?;
            Some(PlannedUpdate {
                repo: update.repo.clone(),
                id: plugin.id.clone(),
                needs_permissions: update.uses.widens(&plugin.uses)
                    || update.matches.widens(&plugin.matches),
                confirmed: None,
            })
        })
        .collect()
}

#[derive(Default)]
pub(super) struct UpdatePrompt {
    batch: Vec<PlannedUpdate>,
}

impl UpdatePrompt {
    pub(super) fn request(&mut self, planned: Vec<PlannedUpdate>) {
        for update in planned {
            if !self.batch.iter().any(|queued| queued.id == update.id) {
                self.batch.push(update);
            }
        }
    }

    pub(super) fn is_pending(&self) -> bool {
        !self.batch.is_empty()
    }

    pub(super) fn current(&self) -> Option<&PlannedUpdate> {
        self.batch
            .iter()
            .find(|update| update.needs_permissions && update.confirmed.is_none())
    }

    pub(super) fn resolve(&mut self, id: &PluginId, confirm: bool) {
        if let Some(update) = self.batch.iter_mut().find(|update| update.id == *id) {
            update.confirmed = Some(confirm);
        }
    }

    pub(super) fn take_ready(&mut self) -> Vec<PlannedUpdate> {
        if self.current().is_some() {
            return Vec::new();
        }
        std::mem::take(&mut self.batch)
            .into_iter()
            .filter(|update| !update.needs_permissions || update.confirmed == Some(true))
            .collect()
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, state: &PluginsState) {
        let Some(update) = self.current() else {
            return;
        };
        let Some(plugin) = state.plugins.iter().find(|p| p.id == update.id) else {
            let id = update.id.clone();
            self.resolve(&id, false);
            return;
        };
        let rows = state
            .available
            .iter()
            .find(|a| a.repo == update.repo && a.id == update.id)
            .map(|a| access_rows(&a.uses))
            .unwrap_or_default();
        let id = update.id.clone();
        if let Some(confirm) = access_prompt(
            ctx,
            "plugin_update_prompt",
            AccessPromptText {
                title: t!("plugins.update_prompt_title", name = plugin_name(plugin)).into_owned(),
                intro: t!("plugins.update_prompt_intro").into_owned(),
                confirm: t!("plugins.update_prompt_confirm").into_owned(),
                decline: t!("plugins.update_prompt_decline").into_owned(),
            },
            &rows,
        ) {
            self.resolve(&id, confirm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{PackageUses, Permission};

    #[test]
    fn every_declared_use_gets_one_row() {
        assert!(access_rows(&PackageUses::default()).is_empty());
        let rows = access_rows(&PackageUses {
            permissions: vec![Permission::Hid, Permission::Network],
            commands: vec!["nvidia-smi".into(), "liquidctl".into()],
            data_reads: vec!["host.sensors.*".into()],
        });
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2].1, "nvidia-smi, liquidctl");
    }
}

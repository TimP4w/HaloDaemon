// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// True when `p` declares permissions the user hasn't consented to (never
/// granted, or the script changed since it was granted). It stays inert until
/// the user grants them. Shared with the Integrations screen, whose
/// integrations are plugins too.
pub(crate) fn plugin_needs_permission(p: &PluginInfo) -> bool {
    (!p.declared_permissions.is_empty()
        || !p.authority.transport_scopes.is_empty()
        || !p.authority.data_reads.is_empty())
        && !p.consented
}

/// True when a previously approved plugin is inert until the user reviews its
/// permissions again after an update or edit. Unlike [`plugin_needs_permission`],
/// this deliberately excludes a newly installed plugin's first-time consent.
pub(crate) fn plugin_requires_regrant(p: &PluginInfo) -> bool {
    !p.enabled
        && p.accepted_authority
            .as_ref()
            .is_some_and(|accepted| !p.authority.is_subset_of(accepted))
}

/// Why the consent modal is being shown, so it can explain the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentReason {
    /// Never approved before — a first-time grant.
    New,
    /// Previously approved, and the current package expands its authority.
    AuthorityExpanded,
}

/// Classify why `p` is asking for consent, from its granted vs declared
/// permissions and whether its content changed since the last acknowledgment.
pub(crate) fn consent_reason(p: &PluginInfo) -> ConsentReason {
    if plugin_requires_regrant(p) {
        ConsentReason::AuthorityExpanded
    } else {
        ConsentReason::New
    }
}

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
        Permission::Os => t!("plugins.permission_os"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage"),
        Permission::Smbus => t!("plugins.permission_smbus"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing"),
    }
}

/// One line explaining what a permission lets the plugin do — and the risk —
/// so the user can make an informed grant decision.
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
        Permission::Os => t!("plugins.permission_os_desc"),
        Permission::SecureStorage => t!("plugins.permission_secure_storage_desc"),
        Permission::Smbus => t!("plugins.permission_smbus_desc"),
        Permission::AudioRouting => t!("plugins.permission_audio_routing_desc"),
    }
}

/// A colored dot + title line, then a muted explanation line.
pub(super) fn bullet(ui: &mut egui::Ui, title: &str, detail: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 3.0, color);
        ui.add_space(theme::SPACE_1);
        ui.label(
            egui::RichText::new(title)
                .font(theme::value_sm())
                .color(theme::TEXT),
        );
    });
    ui.label(
        egui::RichText::new(detail)
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
}

pub(super) fn data_reads_detail(keys: &[String]) -> String {
    format!(
        "{} {}",
        t!("plugins.permission_data_reads_desc"),
        keys.join(", ")
    )
}

/// Full-width card for the grant modal: an amber bullet, plus an optional
/// monospace line (e.g. the programs a command scope may run).
pub(super) fn authority_card(ui: &mut egui::Ui, title: &str, detail: &str, mono: Option<&str>) {
    widgets::inset(ui, egui::Margin::symmetric(14, 12), |ui| {
        bullet(ui, title, detail, theme::STAT_AMBER);
        if let Some(mono) = mono {
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(mono)
                    .font(theme::mono(12.0))
                    .color(theme::TEXT),
            );
        }
    });
    ui.add_space(theme::SPACE_4);
}

pub(super) fn command_scope_names(authority: &halod_shared::types::PluginAuthority) -> Vec<&str> {
    authority
        .transport_scopes
        .iter()
        .filter_map(|scope| scope.strip_prefix("command:"))
        .collect()
}

/// Render the exact permission and command authority summarized by the normal
/// consent dialog. The onboarding review page reuses this so both enable paths
/// present the same security-sensitive information.
pub(crate) fn authority_review_cards(ui: &mut egui::Ui, plugin: &PluginInfo) {
    for &perm in &plugin.declared_permissions {
        authority_card(
            ui,
            &permission_label(perm),
            &permission_description(perm),
            None,
        );
    }
    let commands = command_scope_names(&plugin.authority);
    if !commands.is_empty() {
        authority_card(
            ui,
            "Can run programs",
            "This plugin can start the following programs as your user account, with arguments it controls:",
            Some(&commands.join(", ")),
        );
    }
    if !plugin.authority.data_reads.is_empty() {
        authority_card(
            ui,
            &t!("plugins.permission_data_reads"),
            &data_reads_detail(&plugin.authority.data_reads),
            None,
        );
    }
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

/// The plugin's system access, one row per permission. Grants happen through
/// the enable-time consent modal; once granted the first row offers Revoke.
pub(super) fn permissions_section(ui: &mut egui::Ui, p: &PluginInfo, cmd: &CommandTx) {
    let mut rows: Vec<(String, String)> = p
        .declared_permissions
        .iter()
        .map(|&perm| {
            (
                permission_label(perm).into_owned(),
                permission_description(perm).into_owned(),
            )
        })
        .collect();
    if !p.authority.data_reads.is_empty() {
        rows.push((
            t!("plugins.permission_data_reads").into_owned(),
            data_reads_detail(&p.authority.data_reads),
        ));
    }
    if rows.is_empty() {
        return;
    }
    ui.add_space(theme::SPACE_9);
    widgets::divider(ui);
    ui.add_space(theme::SPACE_8);
    let top = ui.cursor().min;
    widgets::section_label(
        ui,
        &if p.consented {
            t!("plugins.access_granted")
        } else {
            t!("plugins.access_requested")
        },
    );
    ui.add_space(theme::SPACE_4);
    let mut revoke = false;
    for (i, (title, desc)) in rows.iter().enumerate() {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
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
                });
            },
            |ui| {
                if i == 0 && plugin_enabled_with_consent(p) {
                    revoke = widgets::button(
                        ui,
                        &t!("plugins.revoke"),
                        ButtonKind::Secondary,
                        Vec2::new(140.0, 30.0),
                    )
                    .clicked();
                }
            },
        );
        ui.add_space(theme::SPACE_4);
    }
    if !p.consented {
        ui.label(
            egui::RichText::new(t!("plugins.permissions_enable_hint"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
    }
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::PluginsPermissions,
        egui::Rect::from_min_max(top, ui.cursor().min + Vec2::new(ui.available_width(), 0.0)),
    );
    if revoke {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::SetPluginEnabled {
                id: p.id.clone(),
                enabled: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::plugins::tests::*;

    #[test]
    fn command_scopes_are_presented_as_executable_names() {
        let authority = halod_shared::types::PluginAuthority {
            permissions: vec![],
            transport_scopes: vec![
                "hid".into(),
                "command:nvidia-smi".into(),
                "command:liquidctl".into(),
            ],
            data_reads: vec![],
        };
        assert_eq!(
            command_scope_names(&authority),
            vec!["nvidia-smi", "liquidctl"]
        );
    }

    #[test]
    fn consent_reason_classifies_new_added_and_changed() {
        use halod_shared::types::Permission;

        // Never approved: a first-time grant.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network];
        assert_eq!(consent_reason(&p), ConsentReason::New);

        // Approved before (has a granted perm) and an update declares a new one.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network, Permission::Os];
        p.authority.permissions = p.declared_permissions.clone();
        p.accepted_authority = Some(halod_shared::types::PluginAuthority {
            permissions: vec![Permission::Network],
            transport_scopes: vec![],
            data_reads: vec![],
        });
        assert_eq!(consent_reason(&p), ConsentReason::AuthorityExpanded);

        // Approved before, content changed, but no new permission.
        let mut p = info("a", false);
        p.declared_permissions = vec![Permission::Network];
        p.authority.permissions = p.declared_permissions.clone();
        p.accepted_authority = Some(halod_shared::types::PluginAuthority {
            permissions: vec![Permission::Network],
            transport_scopes: vec![],
            data_reads: vec![],
        });
        assert_eq!(consent_reason(&p), ConsentReason::New);
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
        updated.authority.permissions = vec![
            halod_shared::types::Permission::Os,
            halod_shared::types::Permission::Network,
        ];
        updated.accepted_authority = Some(halod_shared::types::PluginAuthority {
            permissions: vec![halod_shared::types::Permission::Os],
            transport_scopes: vec![],
            data_reads: vec![],
        });
        updated.consented = false;
        assert!(plugin_requires_regrant(&updated));

        let mut added_permission = info("added-permission", false);
        added_permission.declared_permissions = vec![
            halod_shared::types::Permission::Os,
            halod_shared::types::Permission::Network,
        ];
        added_permission.authority.permissions = added_permission.declared_permissions.clone();
        added_permission.accepted_authority = Some(halod_shared::types::PluginAuthority {
            permissions: vec![halod_shared::types::Permission::Os],
            transport_scopes: vec![],
            data_reads: vec![],
        });
        added_permission.consented = false;
        assert!(plugin_requires_regrant(&added_permission));

        let mut first_install = info("first-install", false);
        first_install.declared_permissions = vec![halod_shared::types::Permission::Network];
        first_install.consented = false;
        assert!(!plugin_requires_regrant(&first_install));
    }
}

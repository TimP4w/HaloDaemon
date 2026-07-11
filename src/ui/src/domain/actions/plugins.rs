// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin management actions.

use std::collections::HashMap;

use halod_shared::commands::DaemonCommand;
use halod_shared::types::Permission;

use crate::runtime::ipc::{self, CommandTx};

/// Enable or disable a device plugin by id.
pub fn set_plugin_enabled(cmd: &CommandTx, id: String, enabled: bool) {
    ipc::send(cmd, DaemonCommand::SetPluginEnabled { id, enabled });
}

/// Install a plugin package from a local directory path.
pub fn import_plugin(cmd: &CommandTx, source_dir: String) {
    ipc::send(cmd, DaemonCommand::ImportPlugin { source_dir });
}

/// Delete a user plugin script by id (built-ins are rejected by the daemon).
pub fn delete_plugin(cmd: &CommandTx, id: String) {
    ipc::send(cmd, DaemonCommand::DeletePlugin { id });
}

/// Grant a plugin every permission it declares (accept the consent prompt).
pub fn grant_plugin_permissions(cmd: &CommandTx, id: String, declared: Vec<Permission>) {
    ipc::send(
        cmd,
        DaemonCommand::SetPluginPermissions {
            id,
            granted: declared,
        },
    );
}

/// Revoke every permission granted to a plugin (deny/undo consent).
pub fn revoke_plugin_permissions(cmd: &CommandTx, id: String) {
    ipc::send(
        cmd,
        DaemonCommand::SetPluginPermissions {
            id,
            granted: Vec::new(),
        },
    );
}

/// Accept a plugin's consent prompt: grant every permission it declares and
/// enable it in one step ("Grant & Enable").
pub fn grant_and_enable(cmd: &CommandTx, id: String, declared: Vec<Permission>) {
    grant_plugin_permissions(cmd, id.clone(), declared);
    set_plugin_enabled(cmd, id, true);
}

/// Revoke a plugin's permissions and disable it in one step, per the consent
/// model: withdrawing trust also takes the plugin offline.
pub fn revoke_and_disable(cmd: &CommandTx, id: String) {
    revoke_plugin_permissions(cmd, id.clone());
    set_plugin_enabled(cmd, id, false);
}

pub fn apply_pending_plugin_changes(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::ApplyPendingPluginChanges);
}

/// Replace a plugin's user-editable config values. Omit a `secure` field's key
/// entirely to leave its stored secret unchanged (see `SetPluginConfig`).
pub fn set_plugin_config(cmd: &CommandTx, id: String, values: HashMap<String, String>) {
    ipc::send(cmd, DaemonCommand::SetPluginConfig { id, values });
}

/// Fetch a plugin's display-only asset (logo/effect thumbnail).
pub fn get_plugin_asset(cmd: &CommandTx, plugin_id: String, name: String) {
    ipc::send(cmd, DaemonCommand::GetPluginAsset { plugin_id, name });
}

/// Register a git-repo plugin source.
pub fn add_plugin_repo(cmd: &CommandTx, url: String, branch: Option<String>) {
    ipc::send(cmd, DaemonCommand::AddPluginRepo { url, branch });
}

/// Unregister a git-repo plugin source by slug.
pub fn remove_plugin_repo(cmd: &CommandTx, slug: String) {
    ipc::send(cmd, DaemonCommand::RemovePluginRepo { slug });
}

/// Ask the daemon which registered repos are behind their `locked_sha`.
pub fn check_plugin_repo_updates(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::CheckPluginRepoUpdates);
}

/// Fetch and check out a repo's remote tip.
pub fn update_plugin_repo(cmd: &CommandTx, slug: String) {
    ipc::send(cmd, DaemonCommand::UpdatePluginRepo { slug });
}

/// Ask the daemon which repo-sourced plugins have a per-plugin content
/// update available. `slug` scopes the check to one repo; `None` checks all.
pub fn check_plugin_updates(cmd: &CommandTx, slug: Option<String>) {
    ipc::send(cmd, DaemonCommand::CheckPluginUpdates { slug });
}

/// Update one plugin (checks out only its subtree). Never automatic.
pub fn update_plugin(cmd: &CommandTx, plugin_id: String) {
    ipc::send(cmd, DaemonCommand::UpdatePlugin { plugin_id });
}

/// Update every plugin currently flagged with an update available.
pub fn update_all_plugins(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::UpdateAllPlugins);
}

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

/// Install a Lua plugin script (from a file or pasted source).
pub fn import_plugin(cmd: &CommandTx, filename: String, source: String) {
    ipc::send(cmd, DaemonCommand::ImportPlugin { filename, source });
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

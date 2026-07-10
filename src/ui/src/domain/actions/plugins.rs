// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin management actions.

use halod_shared::commands::DaemonCommand;

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

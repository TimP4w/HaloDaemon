// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin management actions.

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

/// Enable or disable a device plugin by id.
pub fn set_plugin_enabled(cmd: &CommandTx, id: String, enabled: bool) {
    ipc::send(cmd, DaemonCommand::SetPluginEnabled { id, enabled });
}

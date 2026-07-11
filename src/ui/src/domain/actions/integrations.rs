// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-integration actions — enable/disable and config, independent of the
//! generic plugin toggle (`actions::plugins::set_plugin_enabled`). Both apply
//! immediately on the daemon, scoped to just the one integration.

use std::collections::HashMap;

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

/// Enable or disable a single integration.
pub fn set_integration_enabled(cmd: &CommandTx, id: String, enabled: bool) {
    ipc::send(cmd, DaemonCommand::SetIntegrationEnabled { id, enabled });
}

/// Replace a single integration's user-editable config values and reconnect
/// just that integration. Omit a `secure` field's key entirely to leave its
/// stored secret unchanged (see `SetIntegrationConfig`).
pub fn set_integration_config(cmd: &CommandTx, id: String, values: HashMap<String, String>) {
    ipc::send(cmd, DaemonCommand::SetIntegrationConfig { id, values });
}

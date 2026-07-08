// SPDX-License-Identifier: GPL-3.0-or-later
//! Key/button remap and macro actions.
//!
//! The Keys tab and macro editor already name each specific intent via local
//! `make_cmd`-style closures that build a `SetButtonMapping`/`PlayMacro`
//! command; [`send`] is the single seam where that output reaches the daemon.

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

/// Send an already-built keys/macro command.
pub fn send(cmd: &CommandTx, keys_cmd: DaemonCommand) {
    ipc::send(cmd, keys_cmd);
}

pub fn reset_all_button_mappings(cmd: &CommandTx, id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::ResetAllButtonMappings { id: id.to_string() },
    );
}

pub fn play_macro(cmd: &CommandTx, steps: Vec<halod_shared::types::MacroStep>) {
    ipc::send(cmd, DaemonCommand::PlayMacro { steps });
}

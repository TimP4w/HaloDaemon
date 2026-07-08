// SPDX-License-Identifier: GPL-3.0-or-later
//! Profile lifecycle (add/rename/remove/switch) and app-rule actions.

use halod_shared::commands::{DaemonCommand, OverrideTarget};

use crate::runtime::ipc::{self, CommandTx};

/// Switch the active profile. Deduped from 6 call sites (profile dropdown,
/// profile settings page, tray menu on Linux and Windows).
pub fn switch_profile(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::SwitchProfile {
            name: name.to_string(),
        },
    );
}

pub fn add_profile(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::AddProfile {
            name: name.to_string(),
        },
    );
}

pub fn rename_profile(cmd: &CommandTx, old_name: &str, new_name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::RenameProfile {
            old_name: old_name.to_string(),
            new_name: new_name.to_string(),
        },
    );
}

pub fn remove_profile(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::RemoveProfile {
            name: name.to_string(),
        },
    );
}

pub fn remove_profile_override(cmd: &CommandTx, target: OverrideTarget) {
    ipc::send(cmd, DaemonCommand::RemoveProfileOverride { target });
}

pub fn list_running_apps(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::ListRunningApps);
}

pub fn remove_app_rule(cmd: &CommandTx, index: usize) {
    ipc::send(cmd, DaemonCommand::RemoveAppRule { index });
}

pub fn update_app_rule(
    cmd: &CommandTx,
    index: usize,
    process_names: Vec<String>,
    profile: &str,
    enabled: bool,
) {
    ipc::send(
        cmd,
        DaemonCommand::UpdateAppRule {
            index,
            process_names,
            profile: profile.to_string(),
            enabled,
        },
    );
}

pub fn add_app_rule(cmd: &CommandTx, process_names: Vec<String>, profile: &str, enabled: bool) {
    ipc::send(
        cmd,
        DaemonCommand::AddAppRule {
            process_names,
            profile: profile.to_string(),
            enabled,
        },
    );
}

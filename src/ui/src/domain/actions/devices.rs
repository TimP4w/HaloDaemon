// SPDX-License-Identifier: GPL-3.0-or-later
//! Device identity/visibility actions: rename, hide/disable a device, hide a
//! sensor.

use halod_shared::commands::DaemonCommand;
use halod_shared::types::VisibilityState;

use crate::runtime::ipc::{self, CommandTx};

/// Rename a device (or chain-linked accessory — same command either way).
pub fn rename_device(cmd: &CommandTx, device_id: &str, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::SetDeviceName {
            device_id: device_id.to_string(),
            name: name.to_string(),
        },
    );
}

/// Show/hide/disable a device from the Home view.
pub fn set_device_visibility(cmd: &CommandTx, device_id: &str, state: VisibilityState) {
    ipc::send(
        cmd,
        DaemonCommand::SetDeviceVisibility {
            device_id: device_id.to_string(),
            state,
        },
    );
}

/// Show/hide a sensor from the Home dashboard.
pub fn set_sensor_visibility(cmd: &CommandTx, sensor_id: &str, state: VisibilityState) {
    ipc::send(
        cmd,
        DaemonCommand::SetSensorVisibility {
            sensor_id: sensor_id.to_string(),
            state,
        },
    );
}

// ── Generic capability controls (Choice/Range/Boolean/Action) ──────────────

pub fn set_choice(cmd: &CommandTx, id: &str, key: &str, selected: usize) {
    ipc::send(
        cmd,
        DaemonCommand::SetChoice {
            id: id.to_string(),
            key: key.to_string(),
            selected,
        },
    );
}

pub fn set_boolean(cmd: &CommandTx, id: &str, key: &str, value: bool) {
    ipc::send(
        cmd,
        DaemonCommand::SetBoolean {
            id: id.to_string(),
            key: key.to_string(),
            value,
        },
    );
}

pub fn trigger_action(cmd: &CommandTx, id: &str, key: &str) {
    ipc::send(
        cmd,
        DaemonCommand::TriggerAction {
            id: id.to_string(),
            key: key.to_string(),
        },
    );
}

pub fn set_eq_preset(cmd: &CommandTx, id: &str, preset_index: usize) {
    ipc::send(
        cmd,
        DaemonCommand::SetEqPreset {
            id: id.to_string(),
            preset_index,
        },
    );
}

// ── Wireless receiver pairing ────────────────────────────────────────────────

pub fn receiver_stop_pairing(cmd: &CommandTx, id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::ReceiverStopPairing { id: id.to_string() },
    );
}

pub fn receiver_start_pairing(cmd: &CommandTx, id: &str, timeout_secs: u8) {
    ipc::send(
        cmd,
        DaemonCommand::ReceiverStartPairing {
            id: id.to_string(),
            timeout_secs,
        },
    );
}

pub fn receiver_unpair(cmd: &CommandTx, id: &str, slot: u8) {
    ipc::send(
        cmd,
        DaemonCommand::ReceiverUnpair {
            id: id.to_string(),
            slot,
        },
    );
}

// ── Onboard (device-memory) profile slots ───────────────────────────────────

pub fn onboard_profile_restore(cmd: &CommandTx, id: &str, slot: u8) {
    ipc::send(
        cmd,
        DaemonCommand::OnboardProfileRestore {
            id: id.to_string(),
            slot,
        },
    );
}

pub fn onboard_profile_switch(cmd: &CommandTx, id: &str, slot: u8) {
    ipc::send(
        cmd,
        DaemonCommand::OnboardProfileSwitch {
            id: id.to_string(),
            slot,
        },
    );
}

pub fn onboard_profile_set_enabled(cmd: &CommandTx, id: &str, slot: u8, enabled: bool) {
    ipc::send(
        cmd,
        DaemonCommand::OnboardProfileSetEnabled {
            id: id.to_string(),
            slot,
            enabled,
        },
    );
}

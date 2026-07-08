// SPDX-License-Identifier: GPL-3.0-or-later
//! Effects Canvas actions: zone placement, per-instance effect assignment,
//! and canvas engine control.
//!
//! Canvas already names each specific intent via local builder functions
//! (`upsert_instance_cmd`, `assign_zone_cmd`, …) that return a fully-built
//! `DaemonCommand`; [`send`] is the single seam where that output — and the
//! debounced `PendingCommands` flush — reaches the daemon.

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

/// Send an already-built canvas command (`CanvasUpsertEffect`,
/// `CanvasMoveZone`, `CanvasPlaceZone`, …).
pub fn send(cmd: &CommandTx, canvas_cmd: DaemonCommand) {
    ipc::send(cmd, canvas_cmd);
}

pub fn remove_zone(cmd: &CommandTx, device_id: &str, zone_id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::CanvasRemoveZone {
            device_id: device_id.to_string(),
            zone_id: zone_id.to_string(),
        },
    );
}

pub fn remove_effect(cmd: &CommandTx, instance_id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::CanvasRemoveEffect {
            instance_id: instance_id.to_string(),
        },
    );
}

pub fn set_default_effect(cmd: &CommandTx, instance_id: Option<String>) {
    ipc::send(cmd, DaemonCommand::CanvasSetDefaultEffect { instance_id });
}

pub fn stop(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::CanvasStop);
}

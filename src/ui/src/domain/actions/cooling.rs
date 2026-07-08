// SPDX-License-Identifier: GPL-3.0-or-later
//! Fan/pump curve actions.

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

pub fn set_fan_curve_points(
    cmd: &CommandTx,
    fan_id: &str,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
) {
    ipc::send(
        cmd,
        DaemonCommand::SetFanCurvePoints {
            fan_id: fan_id.to_string(),
            points,
            sensor_id,
        },
    );
}

pub fn set_fan_curve_preset(
    cmd: &CommandTx,
    fan_id: &str,
    preset: &str,
    sensor_id: Option<String>,
) {
    ipc::send(
        cmd,
        DaemonCommand::SetFanCurvePreset {
            fan_id: fan_id.to_string(),
            preset: preset.to_string(),
            sensor_id,
        },
    );
}

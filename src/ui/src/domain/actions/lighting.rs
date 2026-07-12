// SPDX-License-Identifier: GPL-3.0-or-later
//! RGB lighting actions: apply a zone/device RGB state, target selection,
//! custom-effect lifecycle, and the Effects Canvas instance upsert.
//!
//! The device Lighting tab and the global Lighting page already name each
//! specific intent via local builder functions (`solid_cmd`, `effect_cmd`,
//! `paint_cmd`, `tx_cmd`, `place_on_canvas_cmd`, …) that return a fully-built
//! `DaemonCommand`; [`send`] is the single seam where that output reaches the
//! daemon, so this module doesn't re-invent per-variant wrapper names for
//! commands that are already well-named at the call site.

use std::collections::HashMap;

use halod_shared::commands::DaemonCommand;
use halod_shared::types::{EffectDef, EffectParamValue};

use crate::runtime::ipc::{self, CommandTx};

/// Send an already-built lighting/zone command (`RgbApply`,
/// `RgbSetZoneTransform`, `CanvasPlaceZone`, …).
pub fn send(cmd: &CommandTx, lighting_cmd: DaemonCommand) {
    ipc::send(cmd, lighting_cmd);
}

/// Persist which devices/zones the global Lighting page targets.
pub fn set_lighting_targets(
    cmd: &CommandTx,
    device_ids: Vec<String>,
    zones: HashMap<String, Vec<String>>,
) {
    ipc::send(cmd, DaemonCommand::SetLightingTargets { device_ids, zones });
}

pub fn save_custom_effect(cmd: &CommandTx, name: &str, params: HashMap<String, EffectParamValue>) {
    ipc::send(
        cmd,
        DaemonCommand::SaveCustomEffect {
            name: name.to_string(),
            params,
        },
    );
}

pub fn delete_custom_effect(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::DeleteCustomEffect {
            name: name.to_string(),
        },
    );
}

pub fn canvas_upsert_effect(cmd: &CommandTx, instance_id: &str, def: EffectDef) {
    ipc::send(
        cmd,
        DaemonCommand::CanvasUpsertEffect {
            instance_id: instance_id.to_string(),
            def,
        },
    );
}

// ── RGB chains (ARGB hubs / LED-strip controllers) ──────────────────────────

pub fn rgb_chain_detect_channel(cmd: &CommandTx, id: &str, channel_id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::RgbChainDetectChannel {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
        },
    );
}

pub fn rgb_chain_remove_link(cmd: &CommandTx, id: &str, channel_id: &str, child_device_id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::RgbChainRemoveLink {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            child_device_id: child_device_id.to_string(),
        },
    );
}

pub fn rgb_chain_add_link(
    cmd: &CommandTx,
    id: &str,
    channel_id: &str,
    name: &str,
    led_count: u32,
    topology: halod_shared::types::ZoneTopology,
) {
    ipc::send(
        cmd,
        DaemonCommand::RgbChainAddLink {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            name: name.to_string(),
            led_count,
            topology,
        },
    );
}

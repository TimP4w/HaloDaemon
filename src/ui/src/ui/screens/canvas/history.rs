// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeMap;

use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::types::{CanvasState, EffectDef, PlacedZone};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CanvasSnapshot {
    zones: Vec<PlacedZone>,
    effects: BTreeMap<String, EffectDef>,
    default_effect: Option<String>,
}

impl CanvasSnapshot {
    pub(crate) fn of(canvas: &CanvasState) -> Self {
        let mut zones = canvas.placed_zones.clone();
        zones.sort_by(|a, b| (&a.device_id, &a.channel_id).cmp(&(&b.device_id, &b.channel_id)));
        Self {
            zones,
            effects: canvas
                .effects
                .iter()
                .map(|(id, def)| (id.clone(), def.clone()))
                .collect(),
            default_effect: canvas.default_effect.clone(),
        }
    }

    fn zone(&self, like: &PlacedZone) -> Option<&PlacedZone> {
        self.zones
            .iter()
            .find(|zone| zone.device_id == like.device_id && zone.channel_id == like.channel_id)
    }
}

fn place(zone: &PlacedZone) -> DaemonCommand {
    DaemonCommand::Device {
        device_id: zone.device_id.clone(),
        op: DeviceOp::CanvasPlaceZone {
            channel_id: zone.channel_id.clone(),
            x: Some(zone.x.into()),
            y: Some(zone.y.into()),
            w: Some(zone.w.into()),
            h: Some(zone.h.into()),
            rotation: Some(zone.rotation.into()),
            effect: zone.effect.clone(),
            sampling_mode: Some(zone.sampling_mode),
        },
    }
}

pub(crate) fn restore_commands(
    current: &CanvasSnapshot,
    target: &CanvasSnapshot,
) -> Vec<DaemonCommand> {
    let mut commands: Vec<DaemonCommand> = target
        .effects
        .iter()
        .filter(|(id, def)| current.effects.get(*id) != Some(*def))
        .map(|(id, def)| DaemonCommand::CanvasUpsertEffect {
            instance_id: id.clone(),
            def: def.clone(),
        })
        .collect();
    commands.extend(
        current
            .zones
            .iter()
            .filter(|zone| target.zone(zone).is_none())
            .map(|zone| DaemonCommand::Device {
                device_id: zone.device_id.clone(),
                op: DeviceOp::CanvasRemoveZone {
                    channel_id: zone.channel_id.clone(),
                },
            }),
    );
    commands.extend(
        target
            .zones
            .iter()
            .filter(|zone| current.zone(zone) != Some(*zone))
            .map(place),
    );
    commands.extend(
        current
            .effects
            .keys()
            .filter(|id| !target.effects.contains_key(*id))
            .map(|id| DaemonCommand::CanvasRemoveEffect {
                instance_id: id.clone(),
            }),
    );
    if current.default_effect != target.default_effect {
        commands.push(DaemonCommand::CanvasSetDefaultEffect {
            instance_id: target.default_effect.clone(),
        });
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::zone_with;

    fn snapshot(zones: Vec<PlacedZone>) -> CanvasSnapshot {
        CanvasSnapshot::of(&CanvasState {
            placed_zones: zones,
            ..Default::default()
        })
    }

    #[test]
    fn restoring_the_same_snapshot_sends_nothing() {
        let state = snapshot(vec![zone_with("a", None), zone_with("b", Some("e"))]);
        assert!(restore_commands(&state, &state).is_empty());
    }

    #[test]
    fn restoring_removes_added_zones_and_replaces_moved_ones() {
        let mut moved = zone_with("a", None);
        moved.x = 0.5;
        let before = snapshot(vec![zone_with("a", None)]);
        let after = snapshot(vec![moved, zone_with("b", None)]);
        let commands = restore_commands(&after, &before);
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[0],
            DaemonCommand::Device { op: DeviceOp::CanvasRemoveZone { channel_id }, .. } if channel_id == "b"
        ));
        assert!(matches!(
            &commands[1],
            DaemonCommand::Device { op: DeviceOp::CanvasPlaceZone { channel_id, x: Some(x), .. }, .. }
                if channel_id == "a" && *x == 0.0
        ));
    }

    fn apply(state: &mut CanvasState, command: &DaemonCommand) {
        match command {
            DaemonCommand::Device { device_id, op } => match op {
                DeviceOp::CanvasPlaceZone {
                    channel_id,
                    x,
                    y,
                    w,
                    h,
                    rotation,
                    effect,
                    sampling_mode,
                } => {
                    let index = state
                        .placed_zones
                        .iter()
                        .position(|z| z.device_id == *device_id && z.channel_id == *channel_id);
                    let zone = match index {
                        Some(index) => &mut state.placed_zones[index],
                        None => {
                            state.placed_zones.push(zone_with(channel_id, None));
                            let zone = state.placed_zones.last_mut().unwrap();
                            zone.device_id = device_id.clone();
                            zone
                        }
                    };
                    zone.x = x.unwrap() as f32;
                    zone.y = y.unwrap() as f32;
                    zone.w = w.unwrap() as f32;
                    zone.h = h.unwrap() as f32;
                    zone.rotation = rotation.unwrap() as f32;
                    zone.effect = effect.clone();
                    zone.sampling_mode = sampling_mode.unwrap();
                }
                DeviceOp::CanvasRemoveZone { channel_id } => state
                    .placed_zones
                    .retain(|z| !(z.device_id == *device_id && z.channel_id == *channel_id)),
                _ => panic!("unexpected device op"),
            },
            DaemonCommand::CanvasUpsertEffect { instance_id, def } => {
                state.effects.insert(instance_id.clone(), def.clone());
            }
            DaemonCommand::CanvasRemoveEffect { instance_id } => {
                state.effects.remove(instance_id);
            }
            DaemonCommand::CanvasSetDefaultEffect { instance_id } => {
                state.default_effect = instance_id.clone();
            }
            _ => panic!("unexpected command"),
        }
    }

    fn arb_state() -> impl proptest::strategy::Strategy<Value = CanvasState> {
        use proptest::prelude::*;
        let zones =
            proptest::collection::vec(("[a-c]", 0.0f32..1.0, proptest::option::of("[e-f]")), 0..4);
        let effects = proptest::collection::btree_map("[e-f]", 0u8..3, 0..3);
        (zones, effects, proptest::option::of("[e-f]")).prop_map(|(zones, effects, default)| {
            let mut state = CanvasState::default();
            for (channel, x, effect) in zones {
                if state.placed_zones.iter().any(|z| z.channel_id == channel) {
                    continue;
                }
                let mut zone = zone_with(&channel, effect.as_deref());
                zone.x = x;
                state.placed_zones.push(zone);
            }
            state.effects = effects
                .into_iter()
                .map(|(id, variant)| {
                    (
                        id,
                        EffectDef {
                            effect_id: format!("effect{variant}"),
                            name: None,
                            params: Default::default(),
                        },
                    )
                })
                .collect();
            state.default_effect = default;
            state
        })
    }

    proptest::proptest! {
        #[test]
        fn applying_the_restore_commands_reaches_the_target(current in arb_state(), target in arb_state()) {
            let mut reached = current.clone();
            let from = CanvasSnapshot::of(&current);
            let to = CanvasSnapshot::of(&target);
            for command in restore_commands(&from, &to) {
                apply(&mut reached, &command);
            }
            proptest::prop_assert_eq!(CanvasSnapshot::of(&reached), to);
        }
    }

    #[test]
    fn zone_order_does_not_make_snapshots_differ() {
        let ab = snapshot(vec![zone_with("a", None), zone_with("b", None)]);
        let ba = snapshot(vec![zone_with("b", None), zone_with("a", None)]);
        assert_eq!(ab, ba);
    }
}

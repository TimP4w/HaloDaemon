// SPDX-License-Identifier: GPL-3.0-or-later
//! Vendor-agnostic ARGB chain link.

use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::Arc;

use super::{
    chain::LightingDivisionHub, CapabilityRef, Device, LightingCapability, LightingStateSlot,
    VisibilitySlot,
};
use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{
    DeviceType, LedPosition, LightingChannel, LightingDescriptor, LightingState, RgbColor,
    ZoneTopology,
};
use halod_shared::zone_transform::transform_colors;

pub fn transformed_zone_frame(
    zone: &LightingChannel,
    slot: &LightingStateSlot,
    led_map: &HashMap<String, RgbColor>,
) -> Vec<RgbColor> {
    let colors = (0..zone.leds.len())
        .map(|i| {
            led_map
                .get(&i.to_string())
                .copied()
                .unwrap_or(RgbColor { r: 0, g: 0, b: 0 })
        })
        .collect::<Vec<_>>();
    transform_colors(&colors, zone, &slot.transform_for(&zone.id))
}

pub fn ring_led_positions(topology: &ZoneTopology, count: u32) -> Vec<LedPosition> {
    match topology {
        ZoneTopology::Ring => (0..count)
            .map(|i| {
                let angle = 2.0 * PI * i as f32 / count as f32 - PI / 2.0;
                LedPosition {
                    id: i,
                    x: 0.5 + 0.42 * angle.cos(),
                    y: 0.5 + 0.42 * angle.sin(),
                }
            })
            .collect(),
        ZoneTopology::Rings { count: rings } => {
            let rings = (*rings).max(1) as u32;
            let per_ring = (count / rings).max(1);
            let ring_r_x = 0.42 / rings as f32;
            (0..count)
                .map(|i| {
                    let ring_idx = (i / per_ring).min(rings - 1);
                    let in_ring = i % per_ring;
                    let cx = (ring_idx as f32 + 0.5) / rings as f32;
                    let angle = 2.0 * PI * in_ring as f32 / per_ring as f32 - PI / 2.0;
                    LedPosition {
                        id: i,
                        x: cx + ring_r_x * angle.cos(),
                        y: 0.5 + 0.42 * angle.sin(),
                    }
                })
                .collect()
        }
        _ => vec![],
    }
}

fn topology_to_positions(topology: &ZoneTopology, count: u32) -> Vec<LedPosition> {
    if count == 0 {
        return Vec::new();
    }
    match topology {
        ZoneTopology::Linear => (0..count)
            .map(|i| LedPosition {
                id: i,
                x: if count > 1 {
                    i as f32 / (count - 1) as f32
                } else {
                    0.5
                },
                y: 0.5,
            })
            .collect(),
        ZoneTopology::Ring | ZoneTopology::Rings { .. } => ring_led_positions(topology, count),
        ZoneTopology::Grid => {
            let cols = (count as f32).sqrt().ceil().max(1.0) as u32;
            let rows = count.div_ceil(cols).max(1);
            (0..count)
                .map(|i| {
                    let row = i / cols;
                    let col = i % cols;
                    let x = (col as f32 + 0.5) / cols as f32;
                    let y = if rows > 1 {
                        (row as f32 + 0.5) / rows as f32
                    } else {
                        0.5
                    };
                    LedPosition { id: i, x, y }
                })
                .collect()
        }
        ZoneTopology::Keyboard { .. } => topology_to_positions(&ZoneTopology::Linear, count),
    }
}

/// Shared state for the unified `LightingSegmentDevice`. The authoritative display name
/// lives in the parent's `LightingDivisionHost` state, read via `LightingDivisionHub::link_name`;
/// `fallback_name` is only used when the host has lost the slot.
pub struct LightingSegmentCore {
    pub id: String,
    pub channel_id: String,
    pub fallback_name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    pub leds: Vec<LedPosition>,
    pub rgb: LightingStateSlot,
    pub visibility: VisibilitySlot,
}

impl LightingSegmentCore {
    pub fn new(
        id: String,
        channel_id: String,
        fallback_name: String,
        topology: ZoneTopology,
        led_count: u32,
    ) -> Self {
        let leds = topology_to_positions(&topology, led_count);
        Self {
            id,
            channel_id,
            fallback_name,
            topology,
            led_count,
            leds,
            rgb: LightingStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }
}

pub struct LightingSegmentDevice {
    core: LightingSegmentCore,
    hub: Arc<dyn LightingDivisionHub>,
    rgb_descriptor: LightingDescriptor,
}

impl LightingSegmentDevice {
    pub fn new(
        id: String,
        channel_id: String,
        name: String,
        topology: ZoneTopology,
        led_count: u32,
        hub: Arc<dyn LightingDivisionHub>,
    ) -> Self {
        let core = LightingSegmentCore::new(id, channel_id, name, topology, led_count);
        let rgb_descriptor = LightingDescriptor {
            channels: vec![LightingChannel {
                id: "strip".to_string(),
                name: "Strip".to_string(),
                topology: core.topology.clone(),
                leds: core.leds.clone(),
                color_order: Default::default(),
                division: Default::default(),
            }],
            native_effects: vec![],
        };
        Self {
            core,
            hub,
            rgb_descriptor,
        }
    }

    async fn apply_state(&self, state: LightingState) -> Result<()> {
        let led_count = self.core.led_count as usize;
        match &state {
            LightingState::Static { color } => {
                let colors = vec![*color; led_count];
                self.hub
                    .write_chain_slice(&self.core.channel_id, &self.core.id, &colors)
                    .await?;
            }
            LightingState::PerLed { channels } => {
                if let Some(led_map) = channels.get("strip") {
                    let zone = &self.rgb_descriptor.channels[0];
                    let colors = transformed_zone_frame(zone, &self.core.rgb, led_map);
                    self.hub
                        .write_chain_slice(&self.core.channel_id, &self.core.id, &colors)
                        .await?;
                }
            }
            LightingState::NativeEffect { .. }
            | LightingState::Engine
            | LightingState::DirectEffect { .. } => {}
        }
        self.core.rgb.set_state(Some(state));
        Ok(())
    }
}

#[async_trait]
impl Device for LightingSegmentDevice {
    fn id(&self) -> &str {
        &self.core.id
    }

    fn name(&self) -> &str {
        "ARGB Strip"
    }

    fn has_external_name(&self) -> bool {
        true
    }

    fn vendor(&self) -> &str {
        "Generic"
    }

    fn model(&self) -> &str {
        "ARGB Chain Link"
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::LedStrip
    }

    async fn initialize(&self) -> Result<bool> {
        Ok(true)
    }

    async fn close(&self) {}

    async fn wire_device_name(&self) -> String {
        self.hub
            .link_name(&self.core.channel_id, &self.core.id)
            .unwrap_or_else(|| self.core.fallback_name.clone())
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Lighting(self)]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.core.visibility)
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("child")
    }
}

#[async_trait]
impl LightingCapability for LightingSegmentDevice {
    fn descriptor(&self) -> &LightingDescriptor {
        &self.rgb_descriptor
    }

    async fn apply(&self, state: LightingState) -> Result<()> {
        self.apply_state(state).await
    }

    fn lighting_state(&self) -> &LightingStateSlot {
        &self.core.rgb
    }

    async fn write_frame(&self, channel_id: &str, bytes: &[u8]) -> Result<()> {
        if channel_id != "strip" {
            anyhow::bail!("unknown zone: {channel_id}");
        }
        anyhow::ensure!(
            bytes.len() == self.core.led_count as usize * 3,
            "invalid lighting frame length"
        );
        let colors: Vec<_> = bytes
            .chunks_exact(3)
            .map(|chunk| RgbColor {
                r: chunk[0],
                g: chunk[1],
                b: chunk[2],
            })
            .collect();
        self.hub
            .write_chain_slice(&self.core.channel_id, &self.core.id, &colors)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_positions_span_zero_to_one() {
        let leds = topology_to_positions(&ZoneTopology::Linear, 4);
        assert_eq!(leds.len(), 4);
        assert!((leds[0].x - 0.0).abs() < f32::EPSILON);
        assert!((leds[3].x - 1.0).abs() < f32::EPSILON);
        assert!(leds.iter().all(|l| (l.y - 0.5).abs() < f32::EPSILON));
    }

    #[test]
    fn ring_positions_are_within_unit_square() {
        let leds = topology_to_positions(&ZoneTopology::Ring, 24);
        assert_eq!(leds.len(), 24);
        assert!(leds.iter().all(|l| l.x >= 0.0 && l.x <= 1.0));
        assert!(leds.iter().all(|l| l.y >= 0.0 && l.y <= 1.0));
    }

    #[test]
    fn rings_topology_splits_leds_into_rings() {
        let leds = topology_to_positions(&ZoneTopology::Rings { count: 3 }, 24);
        assert_eq!(leds.len(), 24);
        let first_ring_avg_x: f32 = leds[..8].iter().map(|l| l.x).sum::<f32>() / 8.0;
        let last_ring_avg_x: f32 = leds[16..].iter().map(|l| l.x).sum::<f32>() / 8.0;
        assert!(
            first_ring_avg_x < 0.4,
            "first ring should be on the left: {first_ring_avg_x}"
        );
        assert!(
            last_ring_avg_x > 0.6,
            "last ring should be on the right: {last_ring_avg_x}"
        );
    }

    #[test]
    fn grid_positions_stay_in_unit_square() {
        let leds = topology_to_positions(&ZoneTopology::Grid, 16);
        assert_eq!(leds.len(), 16);
        assert!(leds.iter().all(|l| l.x >= 0.0 && l.x <= 1.0));
        assert!(leds.iter().all(|l| l.y >= 0.0 && l.y <= 1.0));
    }

    #[test]
    fn zero_count_returns_empty() {
        assert!(topology_to_positions(&ZoneTopology::Linear, 0).is_empty());
        assert!(topology_to_positions(&ZoneTopology::Ring, 0).is_empty());
    }

    #[test]
    fn single_led_linear_centres_at_half() {
        let leds = topology_to_positions(&ZoneTopology::Linear, 1);
        assert_eq!(leds.len(), 1);
        assert!((leds[0].x - 0.5).abs() < f32::EPSILON);
    }
}

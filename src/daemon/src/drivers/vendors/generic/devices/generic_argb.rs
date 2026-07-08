//! Vendor-agnostic ARGB chain link.

use std::sync::Arc;

use crate::drivers::{
    chain::ChainHub,
    vendors::generic::devices::common::{per_led_frame, ring_led_positions},
    CapabilityRef, Device, RgbCapability, RgbStateSlot, VisibilitySlot,
};
use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{
    DeviceType, LedPosition, RgbColor, RgbDescriptor, RgbState, RgbZone, ZoneTopology,
};
use halod_shared::zone_transform::transform_colors;

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

/// Shared state for the unified `GenericArgb`. The authoritative display name
/// lives in the parent's `ChainHost` state, read via `ChainHub::link_name`;
/// `fallback_name` is only used when the host has lost the slot.
pub struct GenericArgbCore {
    pub id: String,
    pub channel_id: String,
    pub fallback_name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    pub leds: Vec<LedPosition>,
    pub rgb: RgbStateSlot,
    pub visibility: VisibilitySlot,
}

impl GenericArgbCore {
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
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }
}

pub struct GenericArgb {
    core: GenericArgbCore,
    hub: Arc<dyn ChainHub>,
    rgb_descriptor: RgbDescriptor,
}

impl GenericArgb {
    pub fn new(
        id: String,
        channel_id: String,
        name: String,
        topology: ZoneTopology,
        led_count: u32,
        hub: Arc<dyn ChainHub>,
    ) -> Self {
        let core = GenericArgbCore::new(id, channel_id, name, topology, led_count);
        let rgb_descriptor = RgbDescriptor {
            zones: vec![RgbZone {
                id: "strip".to_string(),
                name: "Strip".to_string(),
                topology: core.topology.clone(),
                leds: core.leds.clone(),
            }],
            native_effects: vec![],
        };
        Self {
            core,
            hub,
            rgb_descriptor,
        }
    }

    async fn apply_state(&self, state: RgbState) -> Result<()> {
        let led_count = self.core.led_count as usize;
        match &state {
            RgbState::Static { color } => {
                let colors = vec![*color; led_count];
                self.hub
                    .write_chain_slice(&self.core.channel_id, &self.core.id, &colors)
                    .await?;
            }
            RgbState::PerLed { zones } => {
                if let Some(led_map) = zones.get("strip") {
                    let colors = per_led_frame(led_map, led_count);
                    let zone = &self.rgb_descriptor.zones[0];
                    let transform = self.core.rgb.transform_for(&zone.id);
                    let colors = transform_colors(&colors, zone, &transform);
                    self.hub
                        .write_chain_slice(&self.core.channel_id, &self.core.id, &colors)
                        .await?;
                }
            }
            RgbState::NativeEffect { .. } | RgbState::Engine | RgbState::DirectEffect { .. } => {}
        }
        self.core.rgb.set_state(Some(state));
        Ok(())
    }
}

#[async_trait]
impl Device for GenericArgb {
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
        vec![CapabilityRef::Rgb(self)]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.core.visibility)
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("child")
    }
}

#[async_trait]
impl RgbCapability for GenericArgb {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.rgb_descriptor
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(state).await
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.core.rgb
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if zone_id != "strip" {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        self.hub
            .write_chain_slice(&self.core.channel_id, &self.core.id, colors)
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

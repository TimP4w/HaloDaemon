//! Generic Razer mouse driver — one `Device` impl over the shared [`Razer`]
//! handle, parameterised by a per-model [`MouseSpec`]. Adding another Razer
//! mouse is a new table row, not a new file.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use crate::{
    drivers::{
        transports::Transport,
        vendors::generic::devices::common::{build_device_id, stable_serial},
        vendors::razer::protocols::razer::{Razer, RAZER_VID},
        CapabilityRef, ChoiceCapability, ChoiceStateCache, Device, DpiCapability, RgbCapability,
        RgbStateSlot, VisibilitySlot,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
};
use halod_shared::types::{
    Choice, ChoiceDisplay, ChoiceOption, ConnectionType, DeviceType, DpiMode, DpiStatus,
    LedPosition, RgbColor, RgbDescriptor, RgbState, RgbZone, ZoneTopology,
};

struct PollRate {
    code: u8,
    label: &'static str,
}

const POLL_RATES: &[PollRate] = &[
    PollRate {
        code: 0x01,
        label: "1000 Hz",
    },
    PollRate {
        code: 0x02,
        label: "500 Hz",
    },
    PollRate {
        code: 0x08,
        label: "125 Hz",
    },
];

struct MouseSpec {
    pid: u16,
    name: &'static str,
    model: &'static str,
    txid: u8,
    led_count: usize,
    dpi_min: u16,
    dpi_max: u16,
    dpi_steps: &'static [u16],
}

const BASILISK_V3_PID: u16 = 0x0099;

const MODELS: &[MouseSpec] = &[MouseSpec {
    pid: BASILISK_V3_PID,
    name: "Basilisk V3",
    model: "Basilisk V3",
    txid: 0x1F,
    led_count: 11,
    dpi_min: 100,
    dpi_max: 26000,
    dpi_steps: &[800, 1600, 3200],
}];

fn spec_for(pid: u16) -> Option<&'static MouseSpec> {
    MODELS.iter().find(|m| m.pid == pid)
}

inventory::submit! {
    DeviceDescriptor {
        // The vendor control lives on interface 3 (OpenRazer wIndex = 0x03).
        // Windows exposes it as the consumer collection (usage_page 0x0C,
        // usage 0x01); Linux hidraw reports usage_page 0, so accept that too.
        matches: |h| matches!(h,
            DiscoveryHandle::Hid { vid: RAZER_VID, pid, interface_number: Some(3), usage_page, usage, .. }
                if spec_for(*pid).is_some()
                    && (*usage_page == 0 || (*usage_page == 0x000C && *usage == 0x0001))
        ),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, pid, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            let spec = spec_for(pid)
                .ok_or_else(|| anyhow::anyhow!("no Razer mouse spec for pid {pid:#06x}"))?;
            Ok(Arc::new(RazerMouse::new(path, serial, idx, spec)?))
        },
    }
}

/// Evenly space `led_count` LEDs along a horizontal line.
fn led_positions(led_count: usize) -> Vec<LedPosition> {
    (0..led_count)
        .map(|i| LedPosition {
            id: i as u32,
            x: if led_count > 1 {
                i as f32 / (led_count - 1) as f32
            } else {
                0.5
            },
            y: 0.5,
        })
        .collect()
}

fn build_descriptor(spec: &MouseSpec) -> RgbDescriptor {
    RgbDescriptor {
        zones: vec![RgbZone {
            id: "mouse".to_string(),
            name: "Lighting".to_string(),
            topology: ZoneTopology::Linear,
            leds: led_positions(spec.led_count),
        }],
        native_effects: vec![],
    }
}

struct DpiState {
    steps: Vec<u16>,
    index: usize,
    current: u16,
}

pub struct RazerMouse {
    id: String,
    serial_number: Option<String>,
    proto: Razer<crate::drivers::transports::hid::HidTransport>,
    spec: &'static MouseSpec,
    descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
    dpi: Mutex<DpiState>,
    poll_rate: AtomicU8,
    choice_cache: ChoiceStateCache,
}

impl RazerMouse {
    fn new(path: &str, serial: Option<&str>, idx: usize, spec: &'static MouseSpec) -> Result<Self> {
        let index = spec.dpi_steps.len() / 2;
        let current = spec.dpi_steps.get(index).copied().unwrap_or(spec.dpi_min);
        Ok(Self {
            id: build_device_id("razer", serial, idx),
            serial_number: stable_serial(serial),
            proto: Razer::open(path, spec.txid)?,
            spec,
            descriptor: build_descriptor(spec),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            dpi: Mutex::new(DpiState {
                steps: spec.dpi_steps.to_vec(),
                index,
                current,
            }),
            poll_rate: AtomicU8::new(0),
            choice_cache: ChoiceStateCache::default(),
        })
    }

    fn clamp_dpi(&self, dpi: u16) -> u16 {
        dpi.clamp(self.spec.dpi_min, self.spec.dpi_max)
    }

    async fn write_leds(&self, colors: &[RgbColor]) -> Result<()> {
        self.proto.set_custom_frame(0, 0, colors).await
    }

    async fn apply_state(&self, state: &RgbState) -> Result<()> {
        let n = self.spec.led_count;
        match state {
            RgbState::Static { color } => self.write_leds(&vec![*color; n]).await?,
            RgbState::PerLed { zones } => {
                if let Some(map) = zones.get("mouse") {
                    let black = RgbColor { r: 0, g: 0, b: 0 };
                    let colors: Vec<RgbColor> = (0..n)
                        .map(|i| map.get(&i.to_string()).copied().unwrap_or(black))
                        .collect();
                    self.write_leds(&colors).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl Device for RazerMouse {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        self.spec.name
    }
    fn vendor(&self) -> &str {
        "Razer"
    }
    fn model(&self) -> &str {
        self.spec.model
    }

    async fn initialize(&self) -> Result<bool> {
        self.proto.enable_custom_frame().await?;
        log::info!("[RazerMouse] Initialized (id={})", self.id);
        Ok(true)
    }

    async fn close(&self) {}

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Mouse
    }

    async fn wire_connection_type(&self) -> Option<ConnectionType> {
        Some(ConnectionType::Wired)
    }

    fn wire_serial_number(&self) -> Option<String> {
        self.serial_number.clone()
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![
            CapabilityRef::Rgb(self),
            CapabilityRef::Dpi(self),
            CapabilityRef::Choice(self),
        ]
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.proto.transport.rate_status())
    }
}

#[async_trait]
impl RgbCapability for RazerMouse {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.descriptor
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(&state).await?;
        self.rgb.set_state(Some(state));
        Ok(())
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if zone_id != "mouse" {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        self.write_leds(colors).await
    }
}

#[async_trait]
impl DpiCapability for RazerMouse {
    async fn dpi_status(&self) -> DpiStatus {
        let dpi = self.dpi.lock().unwrap();
        DpiStatus {
            steps: dpi.steps.clone(),
            current_index: dpi.index,
            current_dpi: dpi.current,
            available_dpis: (self.spec.dpi_min..=self.spec.dpi_max)
                .step_by(100)
                .collect(),
            mode: DpiMode::Host,
        }
    }

    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        let apply = {
            let mut dpi = self.dpi.lock().unwrap();
            dpi.steps = steps.iter().map(|&s| self.clamp_dpi(s)).collect();
            if dpi.index >= dpi.steps.len() {
                dpi.index = dpi.steps.len().saturating_sub(1);
            }
            dpi.steps.get(dpi.index).copied()
        };
        if let Some(v) = apply {
            self.dpi.lock().unwrap().current = v;
            self.proto.set_dpi(v, v).await?;
        }
        Ok(())
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let value = {
            let mut dpi = self.dpi.lock().unwrap();
            let &v = dpi
                .steps
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("dpi index {index} out of range"))?;
            dpi.index = index;
            dpi.current = v;
            v
        };
        self.proto.set_dpi(value, value).await
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        let value = self.clamp_dpi(dpi);
        self.dpi.lock().unwrap().current = value;
        self.proto.set_dpi(value, value).await
    }
}

#[async_trait]
impl ChoiceCapability for RazerMouse {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn to_wire(&self) -> Option<halod_shared::types::DeviceCapability> {
        Some(halod_shared::types::DeviceCapability::Choice(vec![
            Choice {
                key: "poll_rate".into(),
                label: "Polling Rate".into(),
                options: POLL_RATES
                    .iter()
                    .map(|p| ChoiceOption {
                        id: p.label.to_string(),
                        label: p.label.to_string(),
                    })
                    .collect(),
                selected: self.poll_rate.load(Ordering::Relaxed) as usize,
                category: "Mouse".into(),
                display: ChoiceDisplay::List,
                visible_when: None,
            },
        ]))
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        if key != "poll_rate" {
            anyhow::bail!("unknown choice key: {key}");
        }
        let rate = POLL_RATES
            .get(selected)
            .ok_or_else(|| anyhow::anyhow!("poll rate index {selected} out of range"))?;
        self.choice_cache.record(key, selected);
        self.poll_rate.store(selected as u8, Ordering::Relaxed);
        self.proto.set_polling_rate(rate.code).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basilisk() -> &'static MouseSpec {
        spec_for(BASILISK_V3_PID).expect("Basilisk V3 registered")
    }

    #[test]
    fn basilisk_v3_pid_resolves() {
        let spec = basilisk();
        assert_eq!(spec.name, "Basilisk V3");
        assert_eq!(spec.txid, 0x1F);
        assert_eq!(spec.led_count, 11);
        assert!(spec_for(0x0000).is_none());
    }

    #[test]
    fn led_positions_span_unit_line() {
        let leds = led_positions(basilisk().led_count);
        assert_eq!(leds.len(), 11);
        assert!(leds.iter().all(|l| (0.0..=1.0).contains(&l.x)));
        assert!(leds.iter().all(|l| l.y == 0.5));
        assert!(leds.iter().enumerate().all(|(i, l)| l.id == i as u32));
        assert_eq!(leds.first().unwrap().x, 0.0);
        assert_eq!(leds.last().unwrap().x, 1.0);
    }

    #[test]
    fn default_dpi_index_is_mid_step() {
        let spec = basilisk();
        let index = spec.dpi_steps.len() / 2;
        assert_eq!(spec.dpi_steps[index], 1600);
    }

    #[test]
    fn poll_rates_carry_expected_wire_codes() {
        assert_eq!(
            POLL_RATES.iter().map(|p| p.code).collect::<Vec<_>>(),
            vec![0x01, 0x02, 0x08]
        );
    }
}

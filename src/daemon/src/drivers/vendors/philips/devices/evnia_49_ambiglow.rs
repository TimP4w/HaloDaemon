/// Philips Evnia 49M2C8900 — Ambiglow rear LEDs.
///
/// The monitor exposes its DDC/CI tunnel through the VIA Labs Billboard hub
/// (handled by `philips_evnia_49.rs`). The Ambiglow LEDs are driven by a
/// *separate* USB device on the same monitor: an ENE Technology RGB
/// controller. The two devices share no protocol and no transport — this
/// driver targets only the ENE node.
///
/// USB: VID 0x0CF2, PID 0xB201 (bcdDevice 0x0101 in the capture; other
/// firmware revisions of the same SKU may report different bcdDevice values
/// — match VID/PID only).
///
/// Wire format: every operation is a single USB vendor control transfer.
///
///   bmRequestType = 0x40, bRequest = 0x80, wValue = 0,
///   wIndex = target register address, data = payload bytes (1 or 3).
///
/// See `rev_eng/evnia_rgb/PHILIPS_EVNIA_RGB_PROTOCOL.md` for the full memory
/// map. Zone count: 4 zones × 1 LED (one colour each). The panel has more
/// physical LEDs than 4, but the official software only exercises the four
/// RGB slots at 0xE980/E983/E986/E989 — per-LED addressing past 0xE989 is
/// unverified and not exposed here.
mod inner {
    use anyhow::Result;
    use async_trait::async_trait;
    use halod_protocol::types::{
        DeviceType, LedPosition, RgbColor, RgbDescriptor, RgbState, RgbZone,
        ZoneTopology,
    };
    use std::sync::Arc;

    use crate::{
        discovery::{DeviceDescriptor, DiscoveryHandle},
        drivers::{
            vendors::philips::protocols::philips_evnia::PhilipsAmbiglowProtocol,
            CapabilityRef, Device, RgbCapability, RgbStateSlot,
        },
    };

    const VID: u16 = 0x0CF2;
    const PID: u16 = 0xB201;

    // Master enable. 0x04 arms the LED engine; 0x00 disables it.
    const TRIGGER_ADDR: u16 = 0x0023;
    const TRIGGER_ON: u8 = 0x04;
    const TRIGGER_OFF: u8 = 0x00;

    // Four 16-byte zone configuration banks. Six offsets per bank are written.
    const ZONE_BANKS: [u16; 4] = [0xE020, 0xE030, 0xE040, 0xE050];
    const OFF_RESERVED_0: u16 = 0x00;
    const OFF_MODE: u16 = 0x01;
    const OFF_RESERVED_2: u16 = 0x02;
    const OFF_RESERVED_3: u16 = 0x03;
    const OFF_BRIGHTNESS: u16 = 0x09;
    const OFF_COMMIT: u16 = 0x0F;

    const MODE_OFF: u8 = 0x00;
    const MODE_USER_COLOR: u8 = 0x01;

    // OSD brightness levels — Bright / Brighter / Brightest map to 0x00 / 0x02 / 0x04.
    const BRIGHTNESS_BRIGHTEST: u8 = 0x04;

    const COMMIT_APPLY: u8 = 0x01;

    // Per-zone RGB triples, 4 × 3 bytes.
    const COLOR_BASES: [u16; 4] = [0xE980, 0xE983, 0xE986, 0xE989];

    /// Build the single-byte write list for an apply: master + mode/reserved/
    /// brightness in every zone. Order matches the official software.
    fn build_singles(trigger: u8, mode: u8, brightness: u8) -> [(u16, u8); 21] {
        let mut out = [(0u16, 0u8); 21];
        out[0] = (TRIGGER_ADDR, trigger);
        for (i, &base) in ZONE_BANKS.iter().enumerate() {
            out[1 + i] = (base + OFF_MODE, mode);
            out[5 + i] = (base + OFF_RESERVED_0, 0);
            out[9 + i] = (base + OFF_RESERVED_2, 0);
            out[13 + i] = (base + OFF_RESERVED_3, 0);
            out[17 + i] = (base + OFF_BRIGHTNESS, brightness);
        }
        out
    }

    fn build_color_writes(colors: [RgbColor; 4]) -> [(u16, [u8; 3]); 4] {
        let mut out = [(0u16, [0u8; 3]); 4];
        for (i, &base) in COLOR_BASES.iter().enumerate() {
            out[i] = (base, [colors[i].r, colors[i].g, colors[i].b]);
        }
        out
    }

    fn build_commit_writes() -> [(u16, u8); 4] {
        let mut out = [(0u16, 0u8); 4];
        for (i, &base) in ZONE_BANKS.iter().enumerate() {
            out[i] = (base + OFF_COMMIT, COMMIT_APPLY);
        }
        out
    }

    fn build_descriptor() -> RgbDescriptor {
        RgbDescriptor {
            zones: ZONE_BANKS
                .iter()
                .enumerate()
                .map(|(i, _)| RgbZone {
                    id: format!("zone_{}", i),
                    name: format!("Ambiglow Zone {}", i + 1),
                    topology: ZoneTopology::Linear,
                    leds: vec![LedPosition { id: i as u32, x: 0.5, y: 0.5 }],
                })
                .collect(),
            native_effects: vec![],
        }
    }

    /// Fan a state out to 4 per-zone colours. Static replicates; PerLed picks
    /// the first colour of each zone's LED map.
    fn colors_from_state(state: &RgbState) -> Option<[RgbColor; 4]> {
        match state {
            RgbState::Static { color } => Some([*color; 4]),
            RgbState::PerLed { zones } => {
                let mut out = [RgbColor { r: 0, g: 0, b: 0 }; 4];
                for i in 0..4 {
                    let key = format!("zone_{}", i);
                    if let Some(leds) = zones.get(&key) {
                        if let Some(c) = leds.values().next() {
                            out[i] = *c;
                        }
                    }
                }
                Some(out)
            }
            RgbState::NativeEffect { .. } | RgbState::Engine => None,
        }
    }

    pub struct PhilipsEvniaAmbiglow {
        id: String,
        protocol: PhilipsAmbiglowProtocol,
        descriptor: RgbDescriptor,
        rgb: RgbStateSlot,
    }

    impl PhilipsEvniaAmbiglow {
        pub fn new() -> Self {
            Self {
                id: format!("philips_evnia_ambiglow_{:04x}_{:04x}", VID, PID),
                protocol: PhilipsAmbiglowProtocol::new(),
                descriptor: build_descriptor(),
                rgb: RgbStateSlot::default(),
            }
        }

        /// Full 29-transfer apply: master + 4 zones × (mode/zeros/bri) + 4 RGB triples + 4 commits.
        fn apply_sequence(
            &self,
            trigger: u8,
            mode: u8,
            brightness: u8,
            colors: [RgbColor; 4],
        ) -> Result<()> {
            for (addr, byte) in build_singles(trigger, mode, brightness).iter() {
                self.protocol.write(*addr, &[*byte])?;
            }
            for (addr, rgb) in build_color_writes(colors).iter() {
                self.protocol.write(*addr, rgb)?;
            }
            for (addr, byte) in build_commit_writes().iter() {
                self.protocol.write(*addr, &[*byte])?;
            }
            Ok(())
        }

        async fn set_state(&self, state: RgbState) -> Result<()> {
            if let Some(colors) = colors_from_state(&state) {
                self.apply_sequence(TRIGGER_ON, MODE_USER_COLOR, BRIGHTNESS_BRIGHTEST, colors)?;
            }
            self.rgb.set_state(Some(state));
            Ok(())
        }
    }

    #[async_trait]
    impl Device for PhilipsEvniaAmbiglow {
        fn id(&self) -> String { self.id.clone() }
        fn name(&self) -> &str { "Philips Evnia 49 Ambiglow" }
        fn vendor(&self) -> &str { "Philips" }
        fn model(&self) -> &str { "49M2C8900 (Ambiglow)" }

        async fn initialize(&self) -> Result<bool> {
            match self.protocol.open(VID, PID, 0) {
                Ok(()) => {
                    log::info!("PhilipsEvniaAmbiglow: ENE transport opened");
                    Ok(true)
                }
                Err(e) => anyhow::bail!(
                    "Ambiglow transport (USB {VID:04x}:{PID:04x}) open failed: {e}"
                ),
            }
        }

        async fn close(&self) {
            self.protocol.close();
        }

        fn wire_device_type(&self) -> DeviceType {
            DeviceType::Monitor
        }

        async fn wire_device_connected(&self) -> bool {
            self.protocol.is_connected()
        }

        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Rgb(self)]
        }

        fn debug_transport(&self) -> Option<&'static str> {
            Some("usb_control")
        }


    }

    #[async_trait]
    impl RgbCapability for PhilipsEvniaAmbiglow {
        fn descriptor(&self) -> &RgbDescriptor {
            &self.descriptor
        }

        fn rgb_state(&self) -> &RgbStateSlot {
            &self.rgb
        }

        async fn apply(&self, state: RgbState) -> Result<()> {
            self.set_state(state).await
        }

        async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
            let idx = match self.descriptor.zones.iter().position(|z| z.id == zone_id) {
                Some(i) => i,
                None => return Ok(()),
            };
            let color = match colors.first() {
                Some(c) => *c,
                None => return Ok(()),
            };
            self.protocol.write(COLOR_BASES[idx], &[color.r, color.g, color.b])?;
            self.protocol.write(ZONE_BANKS[idx] + OFF_COMMIT, &[COMMIT_APPLY])?;
            Ok(())
        }
    }

    inventory::submit!(DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::UsbNonHid { vid: VID, pid: PID }),
        make: |_h| Ok(Arc::new(PhilipsEvniaAmbiglow::new()) as Arc<dyn crate::drivers::Device>),
    });

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn descriptor_has_four_zones() {
            let d = build_descriptor();
            assert_eq!(d.zones.len(), 4);
            assert_eq!(d.zones[0].id, "zone_0");
            assert_eq!(d.zones[3].id, "zone_3");
            assert!(d.native_effects.is_empty());
        }

        #[test]
        fn singles_layout_matches_capture_blue() {
            let singles = build_singles(TRIGGER_ON, MODE_USER_COLOR, 0x00);
            assert_eq!(singles[0], (0x0023, TRIGGER_ON));
            assert_eq!(singles[1], (0xE021, MODE_USER_COLOR));
            assert_eq!(singles[2], (0xE031, MODE_USER_COLOR));
            assert_eq!(singles[3], (0xE041, MODE_USER_COLOR));
            assert_eq!(singles[4], (0xE051, MODE_USER_COLOR));
            assert_eq!(singles[5], (0xE020, 0x00));
            assert_eq!(singles[17], (0xE029, 0x00));
            assert_eq!(singles[20], (0xE059, 0x00));
        }

        #[test]
        fn color_writes_use_rgb_byte_order() {
            // Capture: FF 00 00 = Red (R=FF, G=00, B=00).
            let red = RgbColor { r: 0xff, g: 0, b: 0 };
            let writes = build_color_writes([red; 4]);
            assert_eq!(writes[0], (0xE980, [0xff, 0, 0]));
            assert_eq!(writes[1], (0xE983, [0xff, 0, 0]));
            assert_eq!(writes[2], (0xE986, [0xff, 0, 0]));
            assert_eq!(writes[3], (0xE989, [0xff, 0, 0]));
        }

        #[test]
        fn commits_target_each_zone() {
            let c = build_commit_writes();
            assert_eq!(c[0], (0xE02F, 0x01));
            assert_eq!(c[1], (0xE03F, 0x01));
            assert_eq!(c[2], (0xE04F, 0x01));
            assert_eq!(c[3], (0xE05F, 0x01));
        }

        #[test]
        fn disable_sequence_zeroes_state() {
            let singles = build_singles(TRIGGER_OFF, MODE_OFF, 0x00);
            assert_eq!(singles[0], (0x0023, 0x00));
            assert_eq!(singles[1], (0xE021, 0x00));
        }

        #[test]
        fn colors_from_static_fans_to_all_zones() {
            let pink = RgbColor { r: 0xff, g: 0x40, b: 0x80 };
            let cs = colors_from_state(&RgbState::Static { color: pink }).unwrap();
            assert_eq!(cs, [pink; 4]);
        }

        #[test]
        fn colors_from_per_led_picks_first_per_zone() {
            use std::collections::HashMap;
            let red = RgbColor { r: 0xff, g: 0, b: 0 };
            let blue = RgbColor { r: 0, g: 0, b: 0xff };
            let mut zones: HashMap<String, HashMap<String, RgbColor>> = HashMap::new();
            let mut z0 = HashMap::new();
            z0.insert("0".into(), red);
            zones.insert("zone_0".into(), z0);
            let mut z2 = HashMap::new();
            z2.insert("2".into(), blue);
            zones.insert("zone_2".into(), z2);
            let cs = colors_from_state(&RgbState::PerLed { zones }).unwrap();
            assert_eq!(cs[0], red);
            assert_eq!(cs[1], RgbColor { r: 0, g: 0, b: 0 });
            assert_eq!(cs[2], blue);
            assert_eq!(cs[3], RgbColor { r: 0, g: 0, b: 0 });
        }

        #[test]
        fn engine_state_skips_apply() {
            assert!(colors_from_state(&RgbState::Engine).is_none());
        }
    }
}


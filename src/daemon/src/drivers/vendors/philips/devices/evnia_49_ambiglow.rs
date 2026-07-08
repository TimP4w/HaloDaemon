// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: tomasf <https://github.com/tomasf/evnia>
//
// The 44-LED full-frame control path (capture-block enable, 0xE100 frame
// buffer, baseline-region restore) is adapted from tomasf/evnia, an
// independent macOS reverse-engineering of the same controller (MIT).

/// Philips Evnia 49M2C8900 — Ambiglow rear LEDs (ENE KB7730 controller).
///
/// Protocol details (control transfers, capture/release, frame buffer) are in
/// `docs/protocols/philips-ambiglow.md`.
mod inner {
    use anyhow::Result;
    use async_trait::async_trait;
    use halod_shared::types::{
        DeviceType, LedPosition, NativeEffect, RgbColor, RgbDescriptor, RgbState, RgbZone,
        ZoneTopology,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::time::{Duration, Instant};

    use crate::{
        drivers::{
            vendors::generic::devices::common::per_led_frame,
            vendors::philips::protocols::philips_evnia::PhilipsAmbiglowProtocol, CapabilityRef,
            Device, RgbCapability, RgbStateSlot,
        },
        registry::discovery::{DeviceDescriptor, DiscoveryHandle},
    };

    const VID: u16 = 0x0CF2;
    const PID: u16 = 0xB201;

    /// Number of addressable LEDs on the rear strip.
    const LED_COUNT: usize = 44;
    const ZONE_ID: &str = "ambiglow";

    /// Native-effect id that hands the LEDs back to the monitor's own Ambiglow
    /// firmware (the inverse of arming host control). Selecting it in the UI
    /// triggers the baseline restore.
    const MONITOR_EFFECT: &str = "monitor";

    /// Minimum gap between the last frame write and a baseline restore. The
    /// controller mis-applies the release if a frame write is still settling.
    const FRAME_SETTLE: Duration = Duration::from_millis(10);

    /// Frame buffer: a single contiguous write of `LED_COUNT * 3` RGB bytes.
    const FRAME_ADDR: u16 = 0xE100;

    /// Control regions. Capture is armed by writing `CAPTURE_BLOCK` to each;
    /// release restores `BASELINE_REGION` at the first one.
    const CONTROL_BLOCKS: [u16; 2] = [0xE020, 0xE030];
    const BASELINE_ADDR: u16 = 0xE020;

    /// 16-byte block that hands direct frame control to the host.
    const CAPTURE_BLOCK: [u8; 16] = [
        0x01, 0x00, 0x02, 0x04, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ];

    /// 64-byte region written to `BASELINE_ADDR` to return control to the
    /// monitor's own Ambiglow firmware.
    const BASELINE_REGION: [u8; 64] = [
        0x00, 0x01, 0x02, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    /// Physical LED layout of the rear Ambiglow strip (front-facing view,
    /// `x` left→right, `y` top→bottom in `[0, 1]`). See
    /// `docs/protocols/philips-ambiglow.md` for the full geometry.
    fn ambiglow_positions() -> Vec<LedPosition> {
        let mut leds = Vec::with_capacity(LED_COUNT);
        let mut push = |id: usize, x: f32, y: f32| {
            leds.push(LedPosition {
                id: id as u32,
                x,
                y,
            })
        };
        // Right vertical edge: 0 bottom → 3 top-right corner.
        for i in 0..4 {
            push(i, 0.98, 1.0 - i as f32 / 3.0);
        }
        // Top row, right of center: corner → just right of center.
        for j in 0..8 {
            push(4 + j, 0.90 - 0.38 * j as f32 / 7.0, 0.0);
        }
        // Top row, left of center: just left of center → left corner.
        for j in 0..8 {
            push(12 + j, 0.48 - 0.38 * j as f32 / 7.0, 0.0);
        }
        // Left vertical edge: top-left corner → bottom.
        for i in 0..4 {
            push(20 + i, 0.02, i as f32 / 3.0);
        }
        // Upper center column (above the mount).
        for k in 0..11 {
            push(24 + k, 0.5, 0.05 + 0.40 * k as f32 / 10.0);
        }
        // Lower center column (below the mount).
        for k in 0..9 {
            push(35 + k, 0.5, 0.55 + 0.40 * k as f32 / 8.0);
        }
        leds
    }

    fn build_descriptor() -> RgbDescriptor {
        RgbDescriptor {
            zones: vec![RgbZone {
                id: ZONE_ID.to_string(),
                name: "Ambiglow".to_string(),
                topology: ZoneTopology::Grid,
                leds: ambiglow_positions(),
            }],
            native_effects: vec![NativeEffect {
                id: MONITOR_EFFECT.to_string(),
                name: "Monitor (firmware control)".to_string(),
                params: vec![],
            }],
        }
    }

    /// Whether a state asks to return the strip to monitor-firmware control.
    fn is_monitor_release(state: &RgbState) -> bool {
        matches!(state, RgbState::NativeEffect { id, .. } if id == MONITOR_EFFECT)
    }

    /// Pack a colour list into the fixed `LED_COUNT * 3` RGB frame buffer.
    /// Extra colours are dropped; a short list leaves the tail black.
    fn build_frame(colors: &[RgbColor]) -> [u8; LED_COUNT * 3] {
        let mut buf = [0u8; LED_COUNT * 3];
        for (i, c) in colors.iter().take(LED_COUNT).enumerate() {
            buf[i * 3] = c.r;
            buf[i * 3 + 1] = c.g;
            buf[i * 3 + 2] = c.b;
        }
        buf
    }

    /// Turn a state into a full `LED_COUNT` colour frame. `Static` fills every
    /// LED; `PerLed` reads the zone's `index -> colour` map. Effect/engine
    /// states drive no frame here.
    fn colors_from_state(state: &RgbState) -> Option<Vec<RgbColor>> {
        match state {
            RgbState::Static { color } => Some(vec![*color; LED_COUNT]),
            RgbState::PerLed { zones } => {
                let map = zones.get(ZONE_ID)?;
                Some(per_led_frame(map, LED_COUNT))
            }
            RgbState::NativeEffect { .. } | RgbState::Engine | RgbState::DirectEffect { .. } => {
                None
            }
        }
    }

    pub struct PhilipsEvniaAmbiglow {
        id: String,
        protocol: PhilipsAmbiglowProtocol,
        descriptor: RgbDescriptor,
        rgb: RgbStateSlot,
        captured: AtomicBool,
        /// When the last frame write landed, so a release can wait for the
        /// frame to settle (see `FRAME_SETTLE`).
        last_frame: Mutex<Option<Instant>>,
    }

    impl PhilipsEvniaAmbiglow {
        pub fn new() -> Self {
            Self {
                id: format!("philips_evnia_ambiglow_{:04x}_{:04x}", VID, PID),
                protocol: PhilipsAmbiglowProtocol::new(),
                descriptor: build_descriptor(),
                rgb: RgbStateSlot::default(),
                captured: AtomicBool::new(false),
                last_frame: Mutex::new(None),
            }
        }

        /// Arm direct frame control once. Idempotent: the capture block is sent
        /// only on the first call (or after a release), so high-frequency
        /// frame writes don't re-arm every time.
        fn ensure_capture(&self) -> Result<()> {
            if self.captured.load(Ordering::Relaxed) {
                return Ok(());
            }
            for addr in CONTROL_BLOCKS {
                self.protocol.write(addr, &CAPTURE_BLOCK)?;
            }
            self.captured.store(true, Ordering::Relaxed);
            Ok(())
        }

        /// Hand control back to the monitor firmware.
        fn release(&self) -> Result<()> {
            self.protocol.write(BASELINE_ADDR, &BASELINE_REGION)?;
            self.captured.store(false, Ordering::Relaxed);
            Ok(())
        }

        /// Wait out any in-flight frame, then restore the baseline. Restoring
        /// while a frame is still settling makes the controller mis-apply the
        /// release (see `FRAME_SETTLE`).
        async fn release_after_settle(&self) -> Result<()> {
            if let Some(d) = self.settle_remaining() {
                tokio::time::sleep(d).await;
            }
            self.release()
        }

        fn write_colors(&self, colors: &[RgbColor]) -> Result<()> {
            self.ensure_capture()?;
            self.protocol.write(FRAME_ADDR, &build_frame(colors))?;
            *self.last_frame.lock().unwrap() = Some(Instant::now());
            Ok(())
        }

        /// Time still to wait before a baseline restore is safe, given the last
        /// frame write. `None` once `FRAME_SETTLE` has already elapsed.
        fn settle_remaining(&self) -> Option<Duration> {
            self.last_frame
                .lock()
                .unwrap()
                .and_then(|t| FRAME_SETTLE.checked_sub(t.elapsed()))
        }

        async fn set_state(&self, state: RgbState) -> Result<()> {
            if is_monitor_release(&state) {
                self.release_after_settle().await?;
            } else if let Some(colors) = colors_from_state(&state) {
                self.write_colors(&colors)?;
            }
            self.rgb.set_state(Some(state));
            Ok(())
        }
    }

    #[async_trait]
    impl Device for PhilipsEvniaAmbiglow {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            "Philips Evnia 49 Ambiglow"
        }
        fn vendor(&self) -> &str {
            "Philips"
        }
        fn model(&self) -> &str {
            "49M2C8900 (Ambiglow)"
        }

        async fn initialize(&self) -> Result<bool> {
            match self.protocol.open(VID, PID, 0) {
                Ok(()) => {
                    log::info!("PhilipsEvniaAmbiglow: ENE transport opened");
                    Ok(true)
                }
                Err(e) => {
                    anyhow::bail!("Ambiglow transport (USB {VID:04x}:{PID:04x}) open failed: {e}")
                }
            }
        }

        async fn close(&self) {
            if self.captured.load(Ordering::Relaxed) {
                if let Err(e) = self.release_after_settle().await {
                    log::warn!("PhilipsEvniaAmbiglow: release failed: {e}");
                }
            }
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

        fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
            self.protocol.rate_status()
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
            if zone_id != ZONE_ID {
                return Ok(());
            }
            self.write_colors(colors)
        }
    }

    inventory::submit!(DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::UsbNonHid { vid: VID, pid: PID }),
        make: |_h| Ok(Arc::new(PhilipsEvniaAmbiglow::new()) as Arc<dyn crate::drivers::Device>),
    });

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        #[test]
        fn descriptor_has_one_zone_of_44_leds() {
            let d = build_descriptor();
            assert_eq!(d.zones.len(), 1);
            assert_eq!(d.zones[0].id, ZONE_ID);
            assert_eq!(d.zones[0].leds.len(), LED_COUNT);
            assert!(matches!(d.zones[0].topology, ZoneTopology::Grid));
        }

        #[test]
        fn descriptor_exposes_monitor_release_effect() {
            let d = build_descriptor();
            let e = d
                .native_effects
                .iter()
                .find(|e| e.id == MONITOR_EFFECT)
                .expect("monitor release effect must be exposed");
            assert!(e.params.is_empty(), "monitor release takes no parameters");
        }

        #[test]
        fn monitor_native_effect_is_a_release() {
            assert!(is_monitor_release(&RgbState::NativeEffect {
                id: MONITOR_EFFECT.into(),
                params: HashMap::new(),
            }));
            // Other native effects and colour states are not releases.
            assert!(!is_monitor_release(&RgbState::NativeEffect {
                id: "something_else".into(),
                params: HashMap::new(),
            }));
            assert!(!is_monitor_release(&RgbState::Static {
                color: RgbColor { r: 1, g: 2, b: 3 }
            }));
        }

        #[test]
        fn positions_cover_every_led_in_order_within_bounds() {
            let leds = ambiglow_positions();
            assert_eq!(leds.len(), LED_COUNT);
            for (i, led) in leds.iter().enumerate() {
                assert_eq!(led.id, i as u32, "LED ids must be contiguous and ordered");
                assert!((0.0..=1.0).contains(&led.x), "x out of range: {}", led.x);
                assert!((0.0..=1.0).contains(&led.y), "y out of range: {}", led.y);
            }
        }

        #[test]
        fn positions_match_documented_geometry() {
            let leds = ambiglow_positions();
            // Right edge runs bottom (LED 0) to top (LED 3); left edge mirrors it.
            assert!(leds[0].x > 0.9 && (leds[0].y - 1.0).abs() < f32::EPSILON);
            assert!(leds[3].x > 0.9 && leds[3].y.abs() < f32::EPSILON);
            assert!(leds[20].x < 0.1 && leds[20].y.abs() < f32::EPSILON);
            assert!(leds[23].x < 0.1 && (leds[23].y - 1.0).abs() < f32::EPSILON);
            // Top row sits at y = 0; center columns sit at x = 0.5.
            assert!(leds[4..20].iter().all(|l| l.y.abs() < f32::EPSILON));
            assert!(leds[24..44]
                .iter()
                .all(|l| (l.x - 0.5).abs() < f32::EPSILON));
        }

        #[test]
        fn settle_remaining_none_without_a_frame() {
            let dev = PhilipsEvniaAmbiglow::new();
            assert!(dev.settle_remaining().is_none());
        }

        #[test]
        fn settle_remaining_some_right_after_a_frame() {
            let dev = PhilipsEvniaAmbiglow::new();
            *dev.last_frame.lock().unwrap() = Some(Instant::now());
            let remaining = dev
                .settle_remaining()
                .expect("just-written frame must settle");
            assert!(remaining <= FRAME_SETTLE);
        }

        #[test]
        fn frame_is_rgb_order_and_132_bytes() {
            // Capture: FF 00 00 = red (R first).
            let red = RgbColor {
                r: 0xff,
                g: 0,
                b: 0,
            };
            let buf = build_frame(&[red; LED_COUNT]);
            assert_eq!(buf.len(), LED_COUNT * 3);
            assert_eq!(&buf[..3], &[0xff, 0x00, 0x00]);
            assert_eq!(&buf[129..], &[0xff, 0x00, 0x00]);
        }

        #[test]
        fn frame_pads_short_list_with_black() {
            let green = RgbColor {
                r: 0,
                g: 0xff,
                b: 0,
            };
            let buf = build_frame(&[green]);
            assert_eq!(&buf[..3], &[0x00, 0xff, 0x00]);
            assert!(buf[3..].iter().all(|&b| b == 0));
        }

        #[test]
        fn frame_truncates_overlong_list() {
            let buf = build_frame(&[RgbColor { r: 1, g: 2, b: 3 }; LED_COUNT + 10]);
            assert_eq!(buf.len(), LED_COUNT * 3);
        }

        #[test]
        fn capture_and_baseline_sizes_match_reference() {
            assert_eq!(CAPTURE_BLOCK.len(), 16);
            assert_eq!(BASELINE_REGION.len(), 64);
            assert_eq!(CONTROL_BLOCKS, [0xE020, 0xE030]);
        }

        #[test]
        fn static_state_fills_every_led() {
            let pink = RgbColor {
                r: 0xff,
                g: 0x40,
                b: 0x80,
            };
            let cs = colors_from_state(&RgbState::Static { color: pink }).unwrap();
            assert_eq!(cs.len(), LED_COUNT);
            assert!(cs.iter().all(|&c| c == pink));
        }

        #[test]
        fn per_led_state_maps_indices() {
            let red = RgbColor {
                r: 0xff,
                g: 0,
                b: 0,
            };
            let blue = RgbColor {
                r: 0,
                g: 0,
                b: 0xff,
            };
            let mut inner = HashMap::new();
            inner.insert("0".to_string(), red);
            inner.insert("43".to_string(), blue);
            let mut zones = HashMap::new();
            zones.insert(ZONE_ID.to_string(), inner);
            let cs = colors_from_state(&RgbState::PerLed { zones }).unwrap();
            assert_eq!(cs[0], red);
            assert_eq!(cs[43], blue);
            assert_eq!(cs[1], RgbColor { r: 0, g: 0, b: 0 });
        }

        #[test]
        fn engine_and_effect_states_drive_no_frame() {
            assert!(colors_from_state(&RgbState::Engine).is_none());
            assert!(colors_from_state(&RgbState::NativeEffect {
                id: "x".into(),
                params: HashMap::new()
            })
            .is_none());
        }
    }
}

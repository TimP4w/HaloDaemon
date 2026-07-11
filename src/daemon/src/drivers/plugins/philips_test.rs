// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in Philips Evnia plugin. These replace the
//! unit tests that lived in the deleted native `evnia_49`/`evnia_49_ambiglow`
//! drivers: they drive the *actual* Lua plugin through the real worker over a
//! recording USB-control transport, and assert both that the DDC/CI settings
//! reach the primary (monitor) endpoint and the Ambiglow frames reach the
//! bundled secondary endpoint — i.e. one device correctly driving two chips.

use std::collections::HashMap;
use std::sync::Arc;

use std::path::Path;

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::{ControlEndpoints, PluginIo, RecordingControl};
use super::worker::DevMatch;
use crate::drivers::transports::ControlTransport;
use crate::drivers::{
    ActionCapability, BooleanCapability, ChoiceCapability, RangeCapability, RgbCapability,
};
use halod_shared::types::{RgbColor, RgbState};

const PHILIPS_SRC: &str = include_str!("builtins/philips_evnia.lua");

/// DDC/CI XOR-fold checksum (mirrors the plugin), for building expected packets.
fn ddcci_xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, &b| a ^ b)
}

/// The expected wire bytes for a standard MCCS VCP write, `6e 51 84 03 vcp 00 value <xor>`.
fn ddc_write_packet(vcp: u8, value: u8) -> Vec<u8> {
    let mut p = vec![0x6e, 0x51, 0x84, 0x03, vcp, 0x00, value];
    p.push(ddcci_xor(&p));
    p
}

/// The expected wire bytes for a Philips extended (E2A0xx) VCP write.
fn ddc_ext_packet(sub: u8, value: u8) -> Vec<u8> {
    let mut p = vec![0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, sub, 0x00, value];
    p.push(ddcci_xor(&p));
    p
}

struct Rig {
    dev: LuaDevice,
    monitor: Arc<RecordingControl>,
    ambiglow: Arc<RecordingControl>,
}

/// Build a merged Philips device over two recording control endpoints (the DDC
/// monitor as primary, the Ambiglow controller as the bundled secondary).
fn rig() -> Rig {
    let manifest = parse_manifest(PHILIPS_SRC, Path::new("philips_evnia.lua")).unwrap();
    let spec = manifest.match_specs[0].clone();

    let monitor = RecordingControl::new();
    let ambiglow = RecordingControl::new();
    let mut endpoints: HashMap<String, Arc<dyn ControlTransport>> = HashMap::new();
    endpoints.insert(ControlEndpoints::PRIMARY.to_owned(), monitor.clone());
    endpoints.insert("ambiglow".to_owned(), ambiglow.clone());

    let dev = LuaDevice::with_transport(
        "philips_evnia-2109_8884".into(),
        &manifest,
        &spec,
        DevMatch {
            transport: "usb_control".into(),
            bus: None,
            addr: None,
            pid: Some(0x8884),
        },
        PluginIo::Control(ControlEndpoints::new(endpoints)),
        tokio::runtime::Handle::current(),
    );
    Rig {
        dev,
        monitor,
        ambiglow,
    }
}

/// Only the payload bytes of each captured write, in order.
fn write_payloads(rec: &RecordingControl) -> Vec<Vec<u8>> {
    rec.writes().into_iter().map(|t| t.data).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn brightness_range_emits_ddcci_write_on_the_monitor_endpoint() {
    let r = rig();
    RangeCapability::set_range(&r.dev, "brightness", 75)
        .await
        .unwrap();

    let writes = r.monitor.writes();
    assert_eq!(writes.len(), 1, "one DDC write for one range change");
    let w = &writes[0];
    assert_eq!(w.bm_request_type, 0x40);
    assert_eq!(w.b_request, 0xB2);
    assert_eq!(w.data, ddc_write_packet(0x10, 75));
    // Setting the monitor must never touch the Ambiglow controller.
    assert!(
        r.ambiglow.writes().is_empty(),
        "monitor set leaves RGB alone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn light_enhancement_range_uses_extended_vcp() {
    let r = rig();
    RangeCapability::set_range(&r.dev, "light_enhancement", 2)
        .await
        .unwrap();
    assert_eq!(write_payloads(&r.monitor), vec![ddc_ext_packet(0x3D, 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_image_choice_maps_index_to_vcp_code() {
    let r = rig();
    // Index 11 = "HDR Game" → VCP 0xDC value 0x21.
    r.dev.set_choice("smart_image", 11).await.unwrap();
    assert_eq!(
        write_payloads(&r.monitor),
        vec![ddc_write_packet(0xDC, 0x21)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_mute_boolean_uses_mccs_mute_codes() {
    let r = rig();
    // MCCS 0x8D: 0x01 = mute, 0x02 = unmute.
    r.dev.set_boolean("audio_mute", true).await.unwrap();
    r.dev.set_boolean("audio_mute", false).await.unwrap();
    assert_eq!(
        write_payloads(&r.monitor),
        vec![ddc_write_packet(0x8D, 0x01), ddc_write_packet(0x8D, 0x02)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pixel_refresh_action_triggers_extended_write() {
    let r = rig();
    r.dev.trigger_action("pixel_refresh").await.unwrap();
    assert_eq!(write_payloads(&r.monitor), vec![ddc_ext_packet(0x36, 0x01)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_apply_arms_capture_then_streams_a_frame_on_ambiglow() {
    let r = rig();
    r.dev
        .apply(RgbState::Static {
            color: RgbColor {
                r: 0xff,
                g: 0,
                b: 0,
            },
        })
        .await
        .unwrap();

    let writes = r.ambiglow.writes();
    // Two capture-block arms (0xE020, 0xE030) then the 132-byte frame (0xE100).
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[0].w_index, 0xE020);
    assert_eq!(writes[0].data.len(), 16);
    assert_eq!(writes[1].w_index, 0xE030);
    let frame = &writes[2];
    assert_eq!(frame.w_index, 0xE100);
    assert_eq!(frame.data.len(), 44 * 3);
    assert_eq!(
        &frame.data[..3],
        &[0xff, 0x00, 0x00],
        "LED 0 is red (R first)"
    );
    assert_eq!(&frame.data[129..], &[0xff, 0x00, 0x00], "LED 43 is red too");
    // Driving the LEDs must never touch the monitor's DDC endpoint.
    assert!(
        r.monitor.writes().is_empty(),
        "RGB apply leaves monitor alone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subsequent_frames_do_not_re_arm_capture() {
    let r = rig();
    r.dev
        .write_frame("ambiglow", &[RgbColor { r: 1, g: 2, b: 3 }; 44])
        .await
        .unwrap();
    r.dev
        .write_frame("ambiglow", &[RgbColor { r: 4, g: 5, b: 6 }; 44])
        .await
        .unwrap();

    // First frame arms capture (2) + frame (1); the second is just a frame.
    let indices: Vec<u16> = r.ambiglow.writes().iter().map(|w| w.w_index).collect();
    assert_eq!(indices, vec![0xE020, 0xE030, 0xE100, 0xE100]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_native_effect_restores_the_baseline_region() {
    let r = rig();
    // Take control first, then hand it back to firmware via the "monitor" effect.
    r.dev
        .apply(RgbState::Static {
            color: RgbColor { r: 1, g: 1, b: 1 },
        })
        .await
        .unwrap();
    r.dev
        .apply(RgbState::NativeEffect {
            id: "monitor".into(),
            params: HashMap::new(),
        })
        .await
        .unwrap();

    let writes = r.ambiglow.writes();
    let last = writes.last().expect("a release write");
    // The release writes the 64-byte baseline region back to 0xE020.
    assert_eq!(last.w_index, 0xE020);
    assert_eq!(last.data.len(), 64);
}

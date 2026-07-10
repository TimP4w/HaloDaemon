// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in Razer Basilisk plugin. They drive the real
//! worker + capabilities against a recording HID transport and assert the emitted
//! 90-byte `razer_report`s (class/id/data_size, args, XOR CRC) match the native
//! Razer protocol exactly — RGB custom-frame, DPI, and polling-rate.

use std::path::Path;
use std::sync::Arc;

use halod_shared::types::{RgbColor, RgbState};

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::PluginIo;
use super::worker::DevMatch;
use crate::drivers::transports::mock::test_transport::MockTransport;
use crate::drivers::{Device, DpiCapability, RgbCapability};

const RAZER_SRC: &str = include_str!("builtins/razer_basilisk.lua");
const TXID: u8 = 0x1F;

/// Reference `razer_report` builder (mirrors the native `build_report`): 91-byte
/// buffer, XOR CRC over bytes 3..=88 at byte 89.
fn razer_report(class: u8, id: u8, data_size: u8, args: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 91];
    buf[2] = TXID;
    buf[6] = data_size;
    buf[7] = class;
    buf[8] = id;
    let n = args.len().min(80);
    buf[9..9 + n].copy_from_slice(&args[..n]);
    buf[89] = buf[3..89].iter().fold(0u8, |crc, &b| crc ^ b);
    buf
}

fn device(mock: Arc<MockTransport>) -> LuaDevice {
    let manifest = parse_manifest(RAZER_SRC, Path::new("razer_basilisk.lua")).unwrap();
    let spec = manifest.match_specs[0].clone();
    let io = PluginIo::Stream {
        transport: mock,
        bulk: None,
    };
    let dev_match = DevMatch {
        transport: "hid".into(),
        pid: Some(0x0099),
        ..Default::default()
    };
    LuaDevice::with_transport(
        "razer".into(),
        &manifest,
        &spec,
        dev_match,
        io,
        tokio::runtime::Handle::current(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_enables_custom_frame_and_reports_zone() {
    let mock = Arc::new(MockTransport::empty());
    let dev = device(mock.clone());
    assert!(dev.initialize().await.unwrap());
    // enable custom-frame: class 0x0F id 0x02 data_size 0x0C, args [0,0,0x08].
    let written = mock.written.lock().await;
    assert_eq!(
        written[0],
        razer_report(0x0F, 0x02, 0x0C, &[0x00, 0x00, 0x08])
    );
    // 11-LED linear zone reported dynamically.
    assert_eq!(dev.descriptor().zones[0].leds.len(), 11);
    assert_eq!(dev.descriptor().zones[0].id, "mouse");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_apply_streams_custom_frame_row() {
    let mock = Arc::new(MockTransport::empty());
    let dev = device(mock.clone());
    dev.initialize().await.unwrap();
    mock.written.lock().await.clear();

    dev.apply(RgbState::Static {
        color: RgbColor { r: 1, g: 2, b: 3 },
    })
    .await
    .unwrap();

    // custom frame: class 0x0F id 0x03 data_size 0x47; args [0,0,row=0,start=0,
    // stop=10, then RGB×11].
    let mut args = vec![0x00, 0x00, 0x00, 0x00, 10];
    for _ in 0..11 {
        args.extend_from_slice(&[1, 2, 3]);
    }
    assert_eq!(
        mock.written.lock().await[0],
        razer_report(0x0F, 0x03, 0x47, &args)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_dpi_encodes_big_endian_x_y() {
    let mock = Arc::new(MockTransport::empty());
    let dev = device(mock.clone());
    dev.initialize().await.unwrap();
    mock.written.lock().await.clear();

    dev.set_dpi_direct(1600).await.unwrap();

    // DPI: class 0x04 id 0x05 data_size 0x07; args [VARSTORE, x_hi,x_lo, y_hi,y_lo, 0,0].
    assert_eq!(
        mock.written.lock().await[0],
        razer_report(
            0x04,
            0x05,
            0x07,
            &[0x01, 0x06, 0x40, 0x06, 0x40, 0x00, 0x00]
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poll_rate_choice_writes_expected_code() {
    use crate::drivers::ChoiceCapability;
    let mock = Arc::new(MockTransport::empty());
    let dev = device(mock.clone());
    dev.initialize().await.unwrap();
    mock.written.lock().await.clear();

    // Selecting index 1 (500 Hz) → wire code 0x02 on class 0x00 id 0x05.
    ChoiceCapability::set_choice(&dev, "poll_rate", 1)
        .await
        .unwrap();
    assert_eq!(
        mock.written.lock().await[0],
        razer_report(0x00, 0x05, 0x01, &[0x02])
    );

    // Out-of-range selection is rejected before any write.
    assert!(ChoiceCapability::set_choice(&dev, "poll_rate", 9)
        .await
        .is_err());
}

// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
//! End-to-end test for the built-in OpenRGB integration plugin: drives the
//! *actual* Lua plugin through the real worker + TCP transport against an
//! in-process fake OpenRGB server, and asserts both directions of the wire
//! protocol (docs/protocols/openrgb.md) — enumeration parses a hand-built
//! `RequestControllerData` reply, and a frame write produces the exact bytes
//! a real OpenRGB server expects.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use halod_shared::types::RgbColor;

use super::super::device::{ChildWorkerFactory, LuaDevice};
use super::super::parse_manifest;
use super::super::transport::PluginIo;
use super::super::worker::{DevMatch, PluginHandle};
use crate::drivers::transports::tcp::TcpTransport;
use crate::drivers::Device;

const OPENRGB_SRC: &str = include_str!("../builtins/openrgb.lua");

/// One packet the fake server received, for post-hoc assertions.
#[derive(Debug, Clone)]
struct Received {
    device_idx: u32,
    packet_id: u32,
    payload: Vec<u8>,
}

fn header_bytes(device_idx: u32, packet_id: u32, size: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(b"ORGB");
    b.extend_from_slice(&device_idx.to_le_bytes());
    b.extend_from_slice(&packet_id.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b
}

async fn read_packet(stream: &mut TcpStream) -> Received {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(&header[0..4], b"ORGB");
    let device_idx = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let packet_id = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let size = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let mut payload = vec![0u8; size as usize];
    if size > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    Received {
        device_idx,
        packet_id,
        payload,
    }
}

/// Build one `RequestControllerData` reply payload matching real OpenRGB's
/// wire format at protocol version 3 (`RGBController::GetDeviceDescriptionData`
/// et al. in the upstream OpenRGB source): a controller named "Test
/// Controller", a `vendor` field (present at version >= 1 — the field this
/// test exists to guard, since it was the actual bug), one mode with all 12
/// `ModeDescription` uint32 fields (`brightness_min`/`max`/`brightness`
/// present at version >= 3, `value` present since version < 6 — both true at
/// version 3), and a single 4-LED "Main" linear zone.
fn controller_data_payload() -> Vec<u8> {
    fn write_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    let mut desc = Vec::new();
    desc.extend_from_slice(&0u32.to_le_bytes()); // type
    write_str(&mut desc, "Test Controller"); // name
    write_str(&mut desc, "Test Vendor"); // vendor (version >= 1)
    write_str(&mut desc, ""); // description
    write_str(&mut desc, ""); // version
    write_str(&mut desc, ""); // serial
    write_str(&mut desc, ""); // location
    desc.extend_from_slice(&1u16.to_le_bytes()); // num_modes
    desc.extend_from_slice(&0u32.to_le_bytes()); // active_mode

    // One ModeDescription ("Direct"), all 12 uint32 fields present at v3.
    write_str(&mut desc, "Direct"); // mode name
    desc.extend_from_slice(&0u32.to_le_bytes()); // value (< v6)
    desc.extend_from_slice(&0u32.to_le_bytes()); // flags
    desc.extend_from_slice(&0u32.to_le_bytes()); // speed_min
    desc.extend_from_slice(&0u32.to_le_bytes()); // speed_max
    desc.extend_from_slice(&0u32.to_le_bytes()); // brightness_min (>= v3)
    desc.extend_from_slice(&100u32.to_le_bytes()); // brightness_max (>= v3)
    desc.extend_from_slice(&0u32.to_le_bytes()); // colors_min
    desc.extend_from_slice(&0u32.to_le_bytes()); // colors_max
    desc.extend_from_slice(&0u32.to_le_bytes()); // speed
    desc.extend_from_slice(&100u32.to_le_bytes()); // brightness (>= v3)
    desc.extend_from_slice(&0u32.to_le_bytes()); // direction
    desc.extend_from_slice(&1u32.to_le_bytes()); // color_mode (PerLed)
    desc.extend_from_slice(&0u16.to_le_bytes()); // mode num_colors (none)

    desc.extend_from_slice(&1u16.to_le_bytes()); // num_zones
    write_str(&mut desc, "Main"); // zone name
    desc.extend_from_slice(&1u32.to_le_bytes()); // zone type (Linear)
    desc.extend_from_slice(&0u32.to_le_bytes()); // leds_min
    desc.extend_from_slice(&0u32.to_le_bytes()); // leds_max
    desc.extend_from_slice(&4u32.to_le_bytes()); // leds_count
                                                 // Matrix-map block: OpenRGB always writes height+width (8 bytes), even
                                                 // for a non-matrix zone, so `matrix_length` is never actually 0.
    desc.extend_from_slice(&8u16.to_le_bytes()); // matrix_length
    desc.extend_from_slice(&0u32.to_le_bytes()); // matrix_height
    desc.extend_from_slice(&0u32.to_le_bytes()); // matrix_width

    desc.extend_from_slice(&0u16.to_le_bytes()); // num_leds
    desc.extend_from_slice(&0u16.to_le_bytes()); // num_colors

    // The message repeats its own total length as a leading u32 ("data_size").
    let mut payload = Vec::with_capacity(4 + desc.len());
    payload.extend_from_slice(&(4 + desc.len() as u32).to_le_bytes());
    payload.extend_from_slice(&desc);
    payload
}

/// Root connection: the enumeration handshake only. Each controller now gets
/// its own connection (see `run_child_server`), so no frame writes arrive here.
/// Drives SetClientName, RequestProtocolVersion, RequestControllerCount,
/// RequestControllerData(0), then returns (caller drops the stream).
async fn run_root_server(mut stream: TcpStream) {
    // SetClientName — no reply.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 50);
    assert_eq!(pkt.payload, b"HaloDaemon\0");

    // RequestProtocolVersion — reply with version 3.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 40);
    stream.write_all(&header_bytes(0, 40, 4)).await.unwrap();
    stream.write_all(&3u32.to_le_bytes()).await.unwrap();

    // RequestControllerCount — reply with count 1.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 0);
    stream.write_all(&header_bytes(0, 0, 4)).await.unwrap();
    stream.write_all(&1u32.to_le_bytes()).await.unwrap();

    // RequestControllerData(0) — reply with our one hand-built controller.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 1);
    assert_eq!(pkt.device_idx, 0);
    let body = controller_data_payload();
    stream
        .write_all(&header_bytes(0, 1, body.len() as u32))
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
}

/// Per-controller connection: its own SetClientName + RequestProtocolVersion
/// handshake, then record whatever comes after (SetCustomMode + UpdateZoneLEDs
/// from the frame writes).
async fn run_child_server(mut stream: TcpStream, recorder: Arc<Mutex<Vec<Received>>>) {
    // SetClientName — no reply.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 50);

    // RequestProtocolVersion — reply with version 3.
    let pkt = read_packet(&mut stream).await;
    assert_eq!(pkt.packet_id, 40);
    stream.write_all(&header_bytes(0, 40, 4)).await.unwrap();
    stream.write_all(&3u32.to_le_bytes()).await.unwrap();

    // Whatever comes next (SetCustomMode, UpdateZoneLEDs) — just record it.
    loop {
        let mut header = [0u8; 16];
        if stream.read_exact(&mut header).await.is_err() {
            break;
        }
        let device_idx = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let packet_id = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let size = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let mut payload = vec![0u8; size as usize];
        if size > 0 && stream.read_exact(&mut payload).await.is_err() {
            break;
        }
        recorder.lock().await.push(Received {
            device_idx,
            packet_id,
            payload,
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openrgb_plugin_enumerates_and_writes_a_frame_end_to_end() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let server_recorder = recorder.clone();
    // The root connects first (enumeration), then the one controller opens its
    // own connection for the frame writes.
    let server_task = tokio::spawn(async move {
        let (root, _) = listener.accept().await.unwrap();
        run_root_server(root).await;
        let (child, _) = listener.accept().await.unwrap();
        run_child_server(child, server_recorder).await;
    });

    let manifest = parse_manifest(OPENRGB_SRC, std::path::Path::new("openrgb.lua")).unwrap();
    let client = TcpTransport::connect(&addr.ip().to_string(), addr.port(), 2000)
        .await
        .unwrap();

    // Each controller child opens its own connection to the same fake server.
    // The child worker needs the plugin's declared permissions (`os` backs the
    // throttle's wall clock), so grant them exactly as the real host would.
    let child_ip = addr.ip().to_string();
    let child_port = addr.port();
    let child_source = manifest.script_source.clone();
    let child_granted = super::super::granted_for(&manifest.plugin_id);
    let child_handle = tokio::runtime::Handle::current();
    let child_worker: ChildWorkerFactory = Arc::new(move || {
        let transport = TcpTransport::connect_blocking(&child_ip, child_port, 2000)?;
        Ok(PluginHandle::spawn(
            child_source.clone(),
            PluginIo::Stream {
                transport: Arc::new(transport),
                bulk: None,
            },
            DevMatch {
                transport: "tcp".to_owned(),
                ..Default::default()
            },
            child_granted.clone(),
            std::collections::HashMap::new(),
            child_handle.clone(),
        ))
    });

    let dev = Arc::new_cyclic(|weak| {
        let mut d = LuaDevice::integration_root(
            "openrgb-0".into(),
            &manifest,
            PluginIo::Stream {
                transport: Arc::new(client),
                bulk: None,
            },
            child_worker,
            tokio::runtime::Handle::current(),
        );
        d.set_self_ref(weak.clone());
        d
    });

    // initialize() drives SetClientName + the protocol-version handshake.
    assert!(dev.initialize().await.unwrap());

    // Go through `as_controller()`, exactly like the real registration path
    // (`register_device_and_children`) does — not a direct `Controller`
    // trait call, which would bypass the `capabilities()` advertisement this
    // depends on. `discover_children()` drives RequestControllerCount +
    // RequestControllerData and must yield one IntegrationLeaf matching our
    // fake controller.
    let children = dev.as_controller().unwrap().discover_children().await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id(), "openrgb-0_ctrl_0");
    assert_eq!(children[0].name(), "Test Controller");
    let rgb = children[0].as_rgb().expect("has rgb");
    assert_eq!(rgb.descriptor().zones.len(), 1);
    assert_eq!(rgb.descriptor().zones[0].id, "0");
    assert_eq!(rgb.descriptor().zones[0].leds.len(), 4);

    // A frame write must produce SetCustomMode (once) then UpdateZoneLEDs
    // with the exact expected wire bytes.
    let colors = [RgbColor { r: 1, g: 2, b: 3 }; 4];
    rgb.write_frame("0", &colors).await.unwrap();
    // The plugin throttles sends to the same zone to at most 60/s so it
    // doesn't outrun OpenRGB's own internal frame queue; space these out so
    // both actually hit the wire.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    // Second write must NOT repeat SetCustomMode.
    rgb.write_frame("0", &colors).await.unwrap();

    drop(dev);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;

    let received = recorder.lock().await.clone();
    let custom_mode_count = received.iter().filter(|p| p.packet_id == 1100).count();
    assert_eq!(custom_mode_count, 1, "SetCustomMode must be sent only once");

    let updates: Vec<&Received> = received.iter().filter(|p| p.packet_id == 1051).collect();
    assert_eq!(updates.len(), 2);
    for update in &updates {
        assert_eq!(update.device_idx, 0);
        // data_size(u32) + zone_idx(u32) + num_colors(u16) + 4x Color(4 bytes).
        let mut expected = Vec::new();
        expected.extend_from_slice(&(4 + 4 + 2 + 4 * 4u32).to_le_bytes());
        expected.extend_from_slice(&0u32.to_le_bytes()); // zone_idx
        expected.extend_from_slice(&4u16.to_le_bytes()); // num_colors
        for _ in 0..4 {
            expected.extend_from_slice(&[1, 2, 3, 0]);
        }
        assert_eq!(update.payload, expected);
    }
}

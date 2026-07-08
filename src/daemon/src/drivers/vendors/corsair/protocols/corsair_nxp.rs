// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project

//! Corsair "NXP" peripheral wire protocol.
//!
//! Device-agnostic: the caller supplies the device class byte and the color
//! buffer. See `docs/protocols/corsair-nxp.md`.

use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};
use halod_shared::types::RgbColor;

/// Report buffer: byte 0 is the `0x00` report ID, bytes 1..65 the 64-byte payload.
pub const REPORT_SIZE: usize = 65;
const TIMEOUT_MS: i32 = 1000;

const CMD_WRITE: u8 = 0x07;
const CMD_READ: u8 = 0x0E;
const CMD_STREAM: u8 = 0x7F;

const PROP_FIRMWARE: u8 = 0x01;
const PROP_SPECIAL_FUNCTION: u8 = 0x04;
const PROP_LIGHTING_CONTROL: u8 = 0x05;
const PROP_LAYOUT_SETUP: u8 = 0x40;
const PROP_SUBMIT_KEYBOARD_24: u8 = 0x28;

const LAYOUT_SETUP_SUB: u8 = 0x1E;
const LAYOUT_PRIMER_MODE: u8 = 0x08;
const LAYOUT_KEY_TRAILER: u8 = 0xC0;
const LAYOUT_KEYS_PER_PACKET: usize = 30;
const LAYOUT_PACKET_COUNT: usize = 4;

const HARDWARE: u8 = 0x01;
const SOFTWARE: u8 = 0x02;

pub const CLASS_KEYBOARD: u8 = 0x03;

const CH_RED: u8 = 0x01;
const CH_GREEN: u8 = 0x02;
const CH_BLUE: u8 = 0x03;

const STREAM_CHUNK: usize = 60;

const CHANNELS: [(u8, fn(&RgbColor) -> u8); 3] =
    [(CH_RED, |c| c.r), (CH_GREEN, |c| c.g), (CH_BLUE, |c| c.b)];

fn frame(payload: &[u8]) -> [u8; REPORT_SIZE] {
    let mut buf = [0u8; REPORT_SIZE];
    buf[1..1 + payload.len()].copy_from_slice(payload);
    buf
}

pub fn special_function(software: bool) -> [u8; REPORT_SIZE] {
    let mode = if software { SOFTWARE } else { HARDWARE };
    frame(&[CMD_WRITE, PROP_SPECIAL_FUNCTION, mode])
}

pub fn lighting_control(software: bool, class: u8) -> [u8; REPORT_SIZE] {
    let mode = if software { SOFTWARE } else { HARDWARE };
    frame(&[CMD_WRITE, PROP_LIGHTING_CONTROL, mode, 0x00, class])
}

pub fn firmware_query() -> [u8; REPORT_SIZE] {
    frame(&[CMD_READ, PROP_FIRMWARE])
}

fn stream_packet(nonce: u8, data: &[u8]) -> [u8; REPORT_SIZE] {
    debug_assert!(data.len() <= STREAM_CHUNK);
    let mut payload = vec![CMD_STREAM, nonce, data.len() as u8, 0x00];
    payload.extend_from_slice(data);
    frame(&payload)
}

fn submit_keyboard_24(channel: u8, packet_count: u8, finish: u8) -> [u8; REPORT_SIZE] {
    frame(&[
        CMD_WRITE,
        PROP_SUBMIT_KEYBOARD_24,
        channel,
        packet_count,
        finish,
    ])
}

pub fn color_frame_packets(colors: &[RgbColor]) -> Vec<[u8; REPORT_SIZE]> {
    let mut out = Vec::new();
    for (channel, extract) in CHANNELS {
        let plane: Vec<u8> = colors.iter().map(extract).collect();
        let chunks: Vec<&[u8]> = plane.chunks(STREAM_CHUNK).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            out.push(stream_packet((i + 1) as u8, chunk));
        }
        let finish = if channel == CH_BLUE { 2 } else { 1 };
        out.push(submit_keyboard_24(channel, chunks.len() as u8, finish));
    }
    out
}

/// Key identifiers omitted from the layout-setup burst to select physical layout.
pub const SKIP_ANSI: &[u8] = &[
    0x31, 0x3f, 0x41, 0x42, 0x51, 0x53, 0x55, 0x6f, 0x7e, 0x7f, 0x80, 0x81,
];
pub const SKIP_ISO_K70_MK2: &[u8] = &[
    0x3f, 0x41, 0x42, 0x50, 0x53, 0x55, 0x6f, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80,
    0x81,
];

pub fn layout_setup_packets(skip: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
    let mut out = Vec::with_capacity(LAYOUT_PACKET_COUNT + 1);
    out.push(frame(&[
        CMD_WRITE,
        PROP_LIGHTING_CONTROL,
        LAYOUT_PRIMER_MODE,
        0x00,
        0x01,
    ]));

    let mut id: u8 = 0;
    for _ in 0..LAYOUT_PACKET_COUNT {
        let mut payload = vec![CMD_WRITE, PROP_LAYOUT_SETUP, LAYOUT_SETUP_SUB, 0x00];
        for _ in 0..LAYOUT_KEYS_PER_PACKET {
            // Advance past skipped identifiers. `skip` is sorted ascending.
            for &s in skip {
                if id == s {
                    id += 1;
                }
            }
            payload.push(id);
            payload.push(LAYOUT_KEY_TRAILER);
            id += 1;
        }
        out.push(frame(&payload));
    }
    out
}

pub struct CorsairNxp<T: Transport> {
    pub(crate) transport: T,
}

impl CorsairNxp<HidTransport> {
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            transport: HidTransport::open(path, None, TIMEOUT_MS, false, None)?,
        })
    }
}

impl<T: Transport> CorsairNxp<T> {
    pub async fn enter_software_mode(&self, class: u8) -> Result<()> {
        let _ = self.transport.write(&firmware_query()).await;
        self.transport.write(&special_function(true)).await?;
        self.transport.write(&lighting_control(true, class)).await?;
        Ok(())
    }

    pub async fn send_layout_setup(&self, skip: &[u8]) -> Result<()> {
        let packets: Vec<Vec<u8>> = layout_setup_packets(skip)
            .into_iter()
            .map(|p| p.to_vec())
            .collect();
        self.transport.write_many(&packets).await
    }

    pub async fn leave_software_mode(&self) -> Result<()> {
        self.transport.write(&special_function(false)).await
    }

    pub async fn write_colors(&self, colors: &[RgbColor]) -> Result<()> {
        let packets: Vec<Vec<u8>> = color_frame_packets(colors)
            .into_iter()
            .map(|p| p.to_vec())
            .collect();
        self.transport.write_many(&packets).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn c(r: u8, g: u8, b: u8) -> RgbColor {
        RgbColor { r, g, b }
    }

    #[test]
    fn report_buffer_has_leading_report_id() {
        let pkt = special_function(true);
        assert_eq!(pkt.len(), REPORT_SIZE);
        assert_eq!(pkt[0], 0x00, "byte 0 is the report ID");
        assert_eq!(pkt[1], CMD_WRITE);
    }

    #[test]
    fn control_packets_match_wire_spec() {
        assert_eq!(&special_function(true)[1..4], &[0x07, 0x04, 0x02]);
        assert_eq!(&special_function(false)[1..4], &[0x07, 0x04, 0x01]);
        assert_eq!(
            &lighting_control(true, CLASS_KEYBOARD)[1..6],
            &[0x07, 0x05, 0x02, 0x00, 0x03]
        );
        assert_eq!(
            &lighting_control(false, CLASS_KEYBOARD)[1..6],
            &[0x07, 0x05, 0x01, 0x00, 0x03]
        );
        assert_eq!(&firmware_query()[1..3], &[0x0E, 0x01]);
    }

    #[test]
    fn stream_packet_carries_length_and_data() {
        let data = [0x11, 0x22, 0x33];
        let pkt = stream_packet(2, &data);
        assert_eq!(&pkt[1..5], &[0x7F, 0x02, 0x03, 0x00]);
        assert_eq!(&pkt[5..8], &data);
    }

    #[test]
    fn three_channels_committed_finish_two_on_blue() {
        // 1 key → 1 stream packet + 1 submit per channel = 6 packets.
        let pkts = color_frame_packets(&[c(10, 20, 30)]);
        assert_eq!(pkts.len(), 6);
        // Submits are the 2nd, 4th, 6th packets; finish=1 for R/G, 2 for blue.
        assert_eq!(&pkts[1][1..6], &[0x07, 0x28, CH_RED, 1, 1]);
        assert_eq!(&pkts[3][1..6], &[0x07, 0x28, CH_GREEN, 1, 1]);
        assert_eq!(&pkts[5][1..6], &[0x07, 0x28, CH_BLUE, 1, 2]);
    }

    #[test]
    fn layout_setup_has_primer_plus_four_packets() {
        let pkts = layout_setup_packets(SKIP_ISO_K70_MK2);
        assert_eq!(pkts.len(), 5);
        // Primer is a lighting-control sub-0x08 packet.
        assert_eq!(&pkts[0][1..6], &[0x07, 0x05, 0x08, 0x00, 0x01]);
        // Each of the 4 layout packets is `07 40 1E 00` then 30 `<id> C0` pairs.
        for p in &pkts[1..] {
            assert_eq!(&p[1..5], &[0x07, 0x40, 0x1E, 0x00]);
            for j in 0..LAYOUT_KEYS_PER_PACKET {
                assert_eq!(
                    p[1 + 5 + 2 * j],
                    LAYOUT_KEY_TRAILER,
                    "trailer after key {j}"
                );
            }
        }
    }

    #[test]
    fn layout_setup_skips_listed_identifiers() {
        // The running key sequence must never emit an identifier from the skip
        // list, including consecutive runs (0x79..0x7d in the ISO list).
        // Ids sit at buf[5], buf[7], …; trailers (0xC0) at buf[6], buf[8], ….
        let ids: Vec<u8> = layout_setup_packets(SKIP_ISO_K70_MK2)[1..]
            .iter()
            .flat_map(|p| (0..LAYOUT_KEYS_PER_PACKET).map(move |j| p[5 + 2 * j]))
            .collect();
        assert_eq!(ids.len(), LAYOUT_PACKET_COUNT * LAYOUT_KEYS_PER_PACKET);
        for &s in SKIP_ISO_K70_MK2 {
            assert!(
                !ids.contains(&s),
                "skip id {s:#04x} leaked into the sequence"
            );
        }
        // Emitted identifiers are strictly increasing (a monotone sequence with
        // the skipped ids removed).
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn large_frame_splits_into_60_byte_stream_packets() {
        // 61 keys → 61 bytes per plane → 2 stream packets (60 + 1) + 1 submit.
        let colors = vec![c(1, 2, 3); 61];
        let pkts = color_frame_packets(&colors);
        // Red plane: packets 0,1 = stream (nonce 1,2), packet 2 = submit(count=2).
        assert_eq!(&pkts[0][1..4], &[0x7F, 1, 60]);
        assert_eq!(&pkts[1][1..4], &[0x7F, 2, 1]);
        assert_eq!(&pkts[2][1..5], &[0x07, 0x28, CH_RED, 2]);
    }

    proptest! {
        // The streamed red plane, reassembled from its stream packets, equals the
        // input red bytes in order — the encode is a faithful channel serializer.
        #[test]
        fn red_plane_round_trips(reds in prop::collection::vec(any::<u8>(), 0..200)) {
            let colors: Vec<RgbColor> = reds.iter().map(|&r| c(r, 0, 0)).collect();
            let pkts = color_frame_packets(&colors);
            // Red stream packets precede the first submit (PROP_SUBMIT at [2]).
            let mut recovered = Vec::new();
            for p in &pkts {
                if p[1] == CMD_STREAM {
                    let len = p[3] as usize;
                    recovered.extend_from_slice(&p[5..5 + len]);
                } else if p[1] == CMD_WRITE && p[2] == PROP_SUBMIT_KEYBOARD_24 {
                    break; // stop at the red submit
                }
            }
            prop_assert_eq!(recovered, reds);
        }

        // Every emitted buffer is well-formed: report ID 0, known command class.
        #[test]
        fn all_packets_well_formed(n in 0usize..300) {
            let colors = vec![c(7, 7, 7); n];
            for p in color_frame_packets(&colors) {
                prop_assert_eq!(p.len(), REPORT_SIZE);
                prop_assert_eq!(p[0], 0x00);
                prop_assert!(p[1] == CMD_STREAM || p[1] == CMD_WRITE);
            }
        }
    }
}

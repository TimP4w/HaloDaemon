// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2021 Aleksandr Mezin <mezin.alexander@gmail.com>
/// Shared NZXT HID protocol primitives reused by Control Hub and Kraken.
///
/// Both devices speak the same wire format for firmware version queries,
/// accessory detection, and raw transport I/O. Device-specific logic
/// (fan caches, status caches, RGB framing, pump profiles) stays in each
/// driver file.
use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};

/// Wire command/reply prefixes shared by Control Hub and Kraken. Each request
/// is the two leading bytes written to the device; the matching reply carries
/// the paired prefix.
mod cmd {
    pub const FW_REQUEST: [u8; 2] = [0x10, 0x02];
    pub const FW_REPLY: [u8; 2] = [0x11, 0x02];
    pub const ACCESSORY_REQUEST: [u8; 2] = [0x20, 0x03];
    pub const ACCESSORY_REPLY: [u8; 2] = [0x21, 0x03];
}

pub struct AccessoryInfo {
    pub channel_id: u8,
    pub accessory_id: u8,
}

#[derive(Clone)]
pub struct NzxtBaseProtocol<T: Transport> {
    /// Exposed so sibling protocol structs can clone it for their polling loop.
    pub(crate) transport: T,
}

impl NzxtBaseProtocol<HidTransport> {
    pub fn open(path: &str, report_size: usize, timeout_ms: i32) -> Result<Self> {
        Ok(Self {
            transport: HidTransport::open(path, Some(report_size), timeout_ms, false, None)?,
        })
    }
}

impl<T: Transport> NzxtBaseProtocol<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub async fn write(&self, pkt: &[u8]) -> Result<()> {
        self.transport.write(pkt).await
    }

    pub async fn read(&self, size: usize) -> Result<Vec<u8>> {
        self.transport.read(size).await
    }

    pub async fn get_firmware_version(&self) -> Result<String> {
        self.write(&cmd::FW_REQUEST).await?;
        let msg = self
            .transport
            .read_matching(64, |pkt| pkt.len() >= 2 && pkt[..2] == cmd::FW_REPLY, 8)
            .await;
        match msg {
            Some(pkt) if pkt.len() > 0x13 => {
                Ok(format!("{}.{}.{}", pkt[0x11], pkt[0x12], pkt[0x13]))
            }
            _ => Ok(String::new()),
        }
    }

    /// Queries accessories connected to any channel.
    /// Layout: count at byte 14; per-channel slots start at byte 15,
    /// each channel has up to 6 accessory IDs (first non-zero is taken).
    pub async fn detect_accessories(&self) -> Result<Vec<AccessoryInfo>> {
        self.write(&cmd::ACCESSORY_REQUEST).await?;
        let msg = self
            .transport
            .read_matching(
                64,
                |pkt| pkt.len() >= 2 && pkt[..2] == cmd::ACCESSORY_REPLY,
                16,
            )
            .await;
        let Some(msg) = msg else {
            return Ok(Vec::new());
        };
        const MAX_ACCESSORIES_PER_CHANNEL: usize = 6;
        let count = if msg.len() > 14 {
            (msg[14] as usize).min(8)
        } else {
            0
        };
        let mut accessories = Vec::new();
        for channel_id in 0..count {
            // Only the first accessory slot of each channel is read.
            let offset = 15 + channel_id * MAX_ACCESSORIES_PER_CHANNEL;
            let accessory_id = if offset < msg.len() { msg[offset] } else { 0 };
            if accessory_id != 0 {
                accessories.push(AccessoryInfo {
                    channel_id: channel_id as u8,
                    accessory_id,
                });
            }
        }
        Ok(accessories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtBaseProtocol<MockTransport> {
        NzxtBaseProtocol::new(MockTransport::new(responses))
    }

    #[tokio::test]
    async fn firmware_returns_version_from_matching_packet() {
        let mut pkt = vec![0u8; 0x14 + 1];
        pkt[0] = 0x11;
        pkt[1] = 0x02;
        pkt[0x11] = 3;
        pkt[0x12] = 1;
        pkt[0x13] = 0;
        assert_eq!(
            protocol(vec![pkt]).get_firmware_version().await.unwrap(),
            "3.1.0"
        );
    }

    #[tokio::test]
    async fn firmware_skips_unrelated_packets_before_match() {
        let garbage = vec![0xAAu8; 64];
        let mut pkt = vec![0u8; 0x14 + 1];
        pkt[0] = 0x11;
        pkt[1] = 0x02;
        pkt[0x11] = 1;
        pkt[0x12] = 2;
        pkt[0x13] = 3;
        let p = protocol(vec![garbage.clone(), garbage.clone(), garbage, pkt]);
        assert_eq!(p.get_firmware_version().await.unwrap(), "1.2.3");
    }

    #[tokio::test]
    async fn firmware_returns_empty_when_no_matching_packet() {
        assert_eq!(
            protocol(vec![vec![0xAAu8; 64]; 8])
                .get_firmware_version()
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn accessories_parsed_from_matching_packet() {
        let mut pkt = vec![0u8; 30];
        pkt[0] = 0x21;
        pkt[1] = 0x03;
        pkt[14] = 2;
        pkt[15] = 0x13; // channel 0
        pkt[21] = 0x17; // channel 1 (offset = 15 + 1*6)
        let acc = protocol(vec![pkt]).detect_accessories().await.unwrap();
        assert_eq!(acc.len(), 2);
        assert_eq!(acc[0].channel_id, 0);
        assert_eq!(acc[0].accessory_id, 0x13);
        assert_eq!(acc[1].channel_id, 1);
        assert_eq!(acc[1].accessory_id, 0x17);
    }

    #[tokio::test]
    async fn accessories_skips_unrelated_packets_before_match() {
        let garbage = vec![0xAAu8; 64];
        let mut pkt = vec![0u8; 20];
        pkt[0] = 0x21;
        pkt[1] = 0x03;
        pkt[14] = 0;
        assert!(protocol(vec![garbage, pkt])
            .detect_accessories()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn accessories_empty_when_no_matching_packet() {
        assert!(protocol(vec![vec![0xAAu8; 64]; 16])
            .detect_accessories()
            .await
            .unwrap()
            .is_empty());
    }
}

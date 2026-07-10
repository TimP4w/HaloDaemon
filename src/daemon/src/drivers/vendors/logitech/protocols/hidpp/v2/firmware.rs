// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! FIRMWARE_VERSION (`0x0003`) — per-entity firmware type and version.
//!
//! func `0x00` getCount returns the number of firmware entities; func `0x10`
//! getFwInfo(entity) returns one entity's type, ASCII prefix, and version. We
//! surface only the MAIN (application) entity's version string.
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use super::{feature, Hidpp20};

const GET_COUNT: u8 = 0x00;
const GET_FW_INFO: u8 = 0x10;
/// fw type stored in the low nibble of byte 0; `0` is the main/application firmware.
const FW_TYPE_MAIN: u8 = 0x00;

/// One decoded firmware entity from a getFwInfo (`0x10`) reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareEntity {
    pub kind: u8,
    pub name: String,
    pub version: String,
}

/// Decode a getFwInfo (`0x10`) reply: byte 0 low nibble = fw type, bytes 1–3 =
/// ASCII prefix, byte 4 = major, byte 5 = minor, bytes 6–7 = big-endian build.
/// Version format mirrors Solaar: `"{prefix}{major:02X}.{minor:02X}.B{build:04X}"`.
/// `None` if the reply is shorter than 8 bytes.
pub fn parse_fw_entity(reply: &[u8]) -> Option<FirmwareEntity> {
    if reply.len() < 8 {
        return None;
    }
    let kind = reply[0] & 0x0F;
    let name = String::from_utf8_lossy(&reply[1..4])
        .trim_end_matches(['\0', ' '])
        .to_string();
    let major = reply[4];
    let minor = reply[5];
    let build = u16::from_be_bytes([reply[6], reply[7]]);
    let version = format!("{name}{major:02X}.{minor:02X}.B{build:04X}");
    Some(FirmwareEntity {
        kind,
        name,
        version,
    })
}

impl Hidpp20 {
    /// Read the MAIN firmware entity's version string. `None` when the device
    /// lacks FIRMWARE_VERSION, reports no entities, or every read fails.
    pub async fn read_firmware_version(&self) -> Option<String> {
        let idx = self.idx(feature::FIRMWARE_VERSION)?;
        let count = self
            .call(idx, GET_COUNT, &[])
            .await
            .ok()?
            .first()
            .copied()?;
        for entity in 0..count {
            let Ok(reply) = self.call(idx, GET_FW_INFO, &[entity]).await else {
                continue;
            };
            if let Some(e) = parse_fw_entity(&reply) {
                if e.kind == FW_TYPE_MAIN {
                    return Some(e.version);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::test_util::MockHidppChannel;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    #[test]
    fn parse_fw_entity_decodes_main_firmware() {
        // type=0 (main), prefix "MPM", major=0x19, minor=0x02, build=0x0016.
        let reply = [0x00, b'M', b'P', b'M', 0x19, 0x02, 0x00, 0x16, 0x00];
        let e = parse_fw_entity(&reply).unwrap();
        assert_eq!(e.kind, 0);
        assert_eq!(e.name, "MPM");
        assert_eq!(e.version, "MPM19.02.B0016");
    }

    #[test]
    fn parse_fw_entity_masks_type_nibble_and_trims_prefix() {
        // High nibble of byte 0 is flags, ignored; prefix padded with a space.
        let reply = [0x51, b'B', b'O', b'T', 0x01, 0x00, 0x00, 0x02, 0x00];
        let e = parse_fw_entity(&reply).unwrap();
        assert_eq!(e.kind, 1);
        assert_eq!(e.name, "BOT");
        assert_eq!(e.version, "BOT01.00.B0002");

        let padded = [0x00, b'R', b'B', b' ', 0x03, 0x0A, 0x01, 0x00, 0x00];
        assert_eq!(parse_fw_entity(&padded).unwrap().name, "RB");
    }

    #[test]
    fn parse_fw_entity_short_reply_is_none() {
        assert_eq!(
            parse_fw_entity(&[0x00, b'M', b'P', b'M', 0x19, 0x02, 0x00]),
            None
        );
    }

    fn hidpp_with(funcs: HashMap<u8, VecDeque<Result<Vec<u8>, String>>>) -> Hidpp20 {
        let ch = Arc::new(MockHidppChannel::new(funcs));
        Hidpp20::new(ch, 0xff, HashMap::from([(feature::FIRMWARE_VERSION, 0x01)]))
    }

    #[tokio::test]
    async fn read_firmware_version_skips_bootloader_returns_main() {
        // getCount → 2 entities. First entity is bootloader (type 1), second is main.
        let funcs = HashMap::from([
            (GET_COUNT, VecDeque::from([Ok(vec![0x02])])),
            (
                GET_FW_INFO,
                VecDeque::from([
                    Ok(vec![0x01, b'B', b'O', b'T', 0x01, 0x00, 0x00, 0x02, 0x00]),
                    Ok(vec![0x00, b'M', b'P', b'M', 0x19, 0x02, 0x00, 0x16, 0x00]),
                ]),
            ),
        ]);
        assert_eq!(
            hidpp_with(funcs).read_firmware_version().await,
            Some("MPM19.02.B0016".to_string())
        );
    }

    #[tokio::test]
    async fn read_firmware_version_none_when_feature_absent() {
        let ch = Arc::new(MockHidppChannel::new(HashMap::new()));
        let h = Hidpp20::new(ch, 0xff, HashMap::new());
        assert_eq!(h.read_firmware_version().await, None);
    }
}

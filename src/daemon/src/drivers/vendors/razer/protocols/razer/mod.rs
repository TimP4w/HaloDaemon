// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Terry Cain and OpenRazer contributors <https://github.com/openrazer/openrazer>

//! Razer USB HID vendor protocol: the fixed 90-byte `razer_report`, its XOR
//! checksum, and the [`Razer`] handle that owns the HID transport and stamps
//! the device's transaction id onto each report.
//!
//! Per-domain operations live in [`matrix`] and [`dpi`] as `impl Razer` blocks.

use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};

pub mod dpi;
pub mod matrix;

pub const RAZER_VID: u16 = 0x1532;

const TIMEOUT_MS: i32 = 1000;

pub const REPORT_LEN: usize = 90;
pub const BUF_LEN: usize = REPORT_LEN + 1;
const ARGS_LEN: usize = 80;

// Storage / LED-zone constants (razercommon.h).
pub const NOSTORE: u8 = 0x00;
pub const VARSTORE: u8 = 0x01;
pub const LED_ZERO: u8 = 0x00;

/// Build the 91-byte wire buffer.  The XOR CRC over report
/// bytes `2..=87` is stamped into report byte 88.
pub fn build_report(txid: u8, class: u8, id: u8, data_size: u8, args: &[u8]) -> [u8; BUF_LEN] {
    let mut buf = [0u8; BUF_LEN];
    buf[2] = txid;
    buf[6] = data_size;
    buf[7] = class;
    buf[8] = id;
    let n = args.len().min(ARGS_LEN);
    buf[9..9 + n].copy_from_slice(&args[..n]);
    buf[89] = buf[3..89].iter().fold(0u8, |crc, &b| crc ^ b);
    buf
}

pub struct Razer<T: Transport> {
    pub(crate) transport: T,
    txid: u8,
}

impl Razer<HidTransport> {
    pub fn open(path: &str, txid: u8) -> Result<Self> {
        let transport = HidTransport::open(path, None, TIMEOUT_MS, true, None)?;
        Ok(Self { transport, txid })
    }
}

impl<T: Transport> Razer<T> {
    pub fn with_transport(transport: T, txid: u8) -> Self {
        Self { transport, txid }
    }

    fn report(&self, class: u8, id: u8, data_size: u8, args: &[u8]) -> [u8; BUF_LEN] {
        build_report(self.txid, class, id, data_size, args)
    }

    pub(crate) async fn send(&self, class: u8, id: u8, data_size: u8, args: &[u8]) -> Result<()> {
        self.transport
            .write(&self.report(class, id, data_size, args))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_report_id_and_field_offsets() {
        let buf = build_report(0x1F, 0x0F, 0x02, 0x06, &[0xAA, 0xBB]);
        assert_eq!(buf.len(), BUF_LEN);
        assert_eq!(buf[0], 0x00, "leading report id");
        assert_eq!(buf[1], 0x00, "status host→device");
        assert_eq!(buf[2], 0x1F, "transaction id");
        assert_eq!(buf[6], 0x06, "data_size");
        assert_eq!(buf[7], 0x0F, "command class");
        assert_eq!(buf[8], 0x02, "command id");
        assert_eq!(&buf[9..11], &[0xAA, 0xBB], "arguments");
    }

    #[test]
    fn crc_is_xor_of_report_bytes_2_through_87() {
        let buf = build_report(0x3F, 0x04, 0x05, 0x07, &[0x01, 0x1B, 0x00]);
        let expected = buf[3..89].iter().fold(0u8, |c, &b| c ^ b);
        assert_eq!(buf[89], expected);
        let other = build_report(0x00, 0x04, 0x05, 0x07, &[0x01, 0x1B, 0x00]);
        assert_eq!(buf[89], other[89]);
    }

    #[test]
    fn oversized_args_are_truncated_to_80() {
        let buf = build_report(0x1F, 0x0F, 0x03, 0x47, &[0x7Fu8; 200]);
        assert_eq!(buf.len(), BUF_LEN);
        assert_eq!(buf[89 - 1], 0x7F);
        assert_eq!(buf[90], 0x00);
    }
}

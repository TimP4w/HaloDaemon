// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! HID++ 1.0 receiver operations — pairing info, device count, and the
//! device-connection (`0x41`) notification decoder.
//!
//! These wrap the receiver's register access ([`Hidpp10`]) behind typed results
//! so the receiver device never touches register addresses or reply bytes.
//!
//! Reference: Solaar (GPL-2.0-or-later) — receiver.py, hidpp10.py
use super::{
    Hidpp10, INFO_EXTENDED_PAIRING, INFO_PAIRING, PAIRING_CLOSE_LOCK, PAIRING_OPEN_LOCK,
    PAIRING_UNPAIR, REG_DEVICE_COUNT, REG_RECEIVER_INFO, REG_RECEIVER_PAIRING,
};

/// A device paired to a receiver slot, decoded from its pairing registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub devnum: u8,
    pub wpid: u16,
    pub serial: Option<String>,
}

impl Hidpp10 {
    /// Ask the receiver to (re)broadcast connection status for all paired
    /// devices (write `0x02` to `REG_DEVICE_COUNT`).
    pub async fn notify_devices(&self) {
        if let Err(e) = self.write(REG_DEVICE_COUNT, &[0x02]).await {
            log::warn!("[HID++1.0] notify_devices failed: {e}");
        }
    }

    /// Number of currently-paired devices. The count is in reply byte 1.
    pub async fn device_count(&self) -> u8 {
        match self.read(REG_DEVICE_COUNT, &[]).await {
            Ok(data) => data.get(1).copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[HID++1.0] read device count failed: {e}");
                0
            }
        }
    }

    /// Read a pairing slot's device info (`slot` is 1-based). Returns `None`
    /// when the slot is empty or the WPID is a sentinel.
    pub async fn paired_info(&self, slot: u8) -> Option<PairedDevice> {
        let pair = self
            .read(REG_RECEIVER_INFO, &[INFO_PAIRING + slot - 1])
            .await
            .ok()?;
        if pair.len() < 8 {
            return None;
        }
        // WPID is bytes[3:5] big-endian (Solaar: extract_wpid reverses pair[3:5]).
        let wpid = ((pair[3] as u16) << 8) | (pair[4] as u16);
        if wpid == 0 || wpid == 0xFFFF {
            return None;
        }
        let serial = self
            .read(REG_RECEIVER_INFO, &[INFO_EXTENDED_PAIRING + slot - 1])
            .await
            .ok()
            .and_then(|ext| parse_extended_serial(&ext));
        Some(PairedDevice {
            devnum: slot,
            wpid,
            serial,
        })
    }

    /// Open the receiver's pairing lock for `timeout_secs` so a new device can
    /// be paired (`0xB2` action `0x01`).
    pub async fn open_pairing_lock(&self, timeout_secs: u8) -> anyhow::Result<()> {
        self.write(
            REG_RECEIVER_PAIRING,
            &[PAIRING_OPEN_LOCK, 0x00, timeout_secs],
        )
        .await
    }

    /// Close the receiver's pairing lock (`0xB2` action `0x02`).
    pub async fn close_pairing_lock(&self) -> anyhow::Result<()> {
        self.write(REG_RECEIVER_PAIRING, &[PAIRING_CLOSE_LOCK, 0x00, 0x00])
            .await
    }

    /// Unpair the device in `slot` (1-based) from the receiver (`0xB2` action `0x03`).
    pub async fn unpair(&self, slot: u8) -> anyhow::Result<()> {
        self.write(REG_RECEIVER_PAIRING, &[PAIRING_UNPAIR, slot])
            .await
    }
}

/// A pairing error reported by the receiver in a `0x4A` lock-status notification
/// after the lock closes without a device having paired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingError {
    DeviceTimeout,
    NotSupported,
    TooManyDevices,
    SequenceTimeout,
    Unknown(u8),
}

impl PairingError {
    pub fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::DeviceTimeout,
            0x02 => Self::NotSupported,
            0x03 => Self::TooManyDevices,
            0x06 => Self::SequenceTimeout,
            other => Self::Unknown(other),
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::DeviceTimeout => "no device found before the pairing window closed",
            Self::NotSupported => "pairing is not supported by this receiver",
            Self::TooManyDevices => "no free pairing slot on the receiver",
            Self::SequenceTimeout => "the pairing sequence timed out",
            Self::Unknown(_) => "pairing failed",
        }
    }
}

/// Decoded `0x4A` pairing-lock-status notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingLockStatus {
    pub open: bool,
    pub error: Option<PairingError>,
}

/// Decode a receiver `0x4A` pairing-lock-status notification. Bit `0x01` of the
/// `address` byte is "lock open"; the first `data` byte is a [`PairingError`]
/// code when the lock has closed (Solaar reads these from two distinct bytes,
/// so an error code such as `0x03` is never mistaken for the open flag).
pub fn decode_pairing_lock(address: u8, data: &[u8]) -> PairingLockStatus {
    let open = address & 0x01 != 0;
    let code = data.first().copied().unwrap_or(0);
    let error = (!open && code != 0).then(|| PairingError::from_code(code));
    PairingLockStatus { open, error }
}

/// Decode the link state from an HID++ 1.0 receiver `0x41` device-connection
/// notification. The receiver sends `0x41` for both connect and disconnect;
/// bit `0x40` of the first data byte is "link not established" — set on
/// power-off, clear on power-on. (`link_established = !(data[0] & 0x40)`)
pub fn decode_link_established(data: &[u8]) -> bool {
    data.first().is_some_and(|&b| b & 0x40 == 0)
}

/// Parse the 4-byte serial from an extended-pairing reply.
/// Returns `None` for all-zero or all-`0xFF` payloads (unset slot sentinels).
fn parse_extended_serial(ext: &[u8]) -> Option<String> {
    if ext.len() < 5 {
        return None;
    }
    let b = &ext[1..5];
    if b == [0xFF, 0xFF, 0xFF, 0xFF] || b == [0, 0, 0, 0] {
        return None;
    }
    Some(format!("{:02X}{:02X}{:02X}{:02X}", b[0], b[1], b[2], b[3]))
}

#[cfg(test)]
mod tests {
    use super::{
        decode_link_established, decode_pairing_lock, parse_extended_serial, PairingError,
    };

    // Live captures from a Lightspeed receiver: the `0x41` notification is sent
    // for both power-off and power-on; bit 0x40 of data[0] is "link not
    // established". The trailing bytes vary per device and are irrelevant.
    #[test]
    fn decode_power_off_is_disconnected() {
        assert!(!decode_link_established(&[0x71, 0xb0, 0x40])); // device 1 off
        assert!(!decode_link_established(&[0x72, 0x99, 0x40])); // device 2 off
    }

    #[test]
    fn decode_power_on_is_connected() {
        assert!(decode_link_established(&[0xb1, 0xb0, 0x40])); // device 1 on
        assert!(decode_link_established(&[0xb2, 0x99, 0x40])); // device 2 on
    }

    #[test]
    fn decode_empty_payload_is_disconnected() {
        assert!(!decode_link_established(&[]));
    }

    #[test]
    fn parse_extended_serial_returns_hex_string() {
        // Byte 0 is ignored; bytes 1–4 are the serial.
        let ext = [0x00u8, 0xAB, 0xCD, 0xEF, 0x12, 0x00];
        assert_eq!(parse_extended_serial(&ext), Some("ABCDEF12".to_string()));
    }

    #[test]
    fn parse_extended_serial_rejects_all_ff_sentinel() {
        let ext = [0x00u8, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(parse_extended_serial(&ext), None);
    }

    #[test]
    fn parse_extended_serial_rejects_all_zero_sentinel() {
        let ext = [0x00u8, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_extended_serial(&ext), None);
    }

    #[test]
    fn parse_extended_serial_rejects_short_payload() {
        assert_eq!(parse_extended_serial(&[0x00, 0xAB, 0xCD]), None);
    }

    // Bit 0x01 of the address byte means the pairing lock is open; the data byte
    // is irrelevant while open.
    #[test]
    fn decode_pairing_lock_open() {
        let s = decode_pairing_lock(0x01, &[0x00]);
        assert!(s.open);
        assert_eq!(s.error, None);
    }

    // Lock closed (address bit clear) with data byte 0 — a device paired, no error.
    #[test]
    fn decode_pairing_lock_closed_clean() {
        let s = decode_pairing_lock(0x00, &[0x00]);
        assert!(!s.open);
        assert_eq!(s.error, None);
    }

    // Lock closed with a nonzero data byte — that byte is the error code. An error
    // code whose low bit is set (0x03) is still an error, not "lock open", because
    // open/error live in different bytes.
    #[test]
    fn decode_pairing_lock_closed_with_error() {
        assert_eq!(
            decode_pairing_lock(0x00, &[0x02]).error,
            Some(PairingError::NotSupported)
        );
        assert_eq!(
            decode_pairing_lock(0x00, &[0x06]).error,
            Some(PairingError::SequenceTimeout)
        );
        assert_eq!(
            decode_pairing_lock(0x00, &[0x03]).error,
            Some(PairingError::TooManyDevices)
        );
    }

    // While the lock is open, a stray data byte is not treated as an error.
    #[test]
    fn decode_pairing_lock_open_ignores_data_byte() {
        assert_eq!(decode_pairing_lock(0x01, &[0x03]).error, None);
    }

    #[test]
    fn decode_pairing_lock_empty_payload_is_closed() {
        let s = decode_pairing_lock(0x00, &[]);
        assert!(!s.open);
        assert_eq!(s.error, None);
    }

    #[test]
    fn pairing_error_maps_unknown_code() {
        assert_eq!(PairingError::from_code(0x7F), PairingError::Unknown(0x7F));
    }
}

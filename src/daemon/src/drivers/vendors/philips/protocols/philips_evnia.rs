use anyhow::{anyhow, Context, Result};
use std::time::Duration;

use crate::drivers::transports::usb_control::UsbControlTransport;

// ── DDC/CI tunnel constants ───────────────────────────────────────────────────

// USB control transfer parameters for the DDC/CI tunnel.
pub(crate) const BMREQ_OUT: u8 = 0x40; // vendor | host-to-device | device recipient
pub(crate) const BMREQ_IN: u8 = 0xC0;  // vendor | device-to-host  | device recipient
pub(crate) const BREQ_DDCCI_WRITE: u8 = 0xB2;
pub(crate) const BREQ_DDCCI_READ: u8 = 0xA3;
pub(crate) const READ_W_INDEX: u16 = 0x006F;

// Time the monitor needs to assemble a reply after we write a request.
// Captures showed ~130 ms; 150 ms is a comfortable margin.
pub(crate) const READ_DELAY: Duration = Duration::from_millis(150);

// Minimum gap between consecutive DDC/CI write commands (MCCS spec §4.5).
// Without this, rapid back-to-back writes (e.g. during profile restore) cause
// the monitor firmware to drop or mis-apply some commands.
pub(crate) const WRITE_DELAY: Duration = Duration::from_millis(50);

// ── DDC/CI frame helpers ──────────────────────────────────────────────────────

pub(crate) fn ddcci_xor(buf: &[u8]) -> u8 {
    buf.iter().fold(0u8, |acc, &b| acc ^ b)
}

pub(crate) fn build_write(vcp: u8, value: u8) -> [u8; 8] {
    let mut p = [0x6e, 0x51, 0x84, 0x03, vcp, 0x00, value, 0x00];
    p[7] = ddcci_xor(&p[..7]);
    p
}

pub(crate) fn build_extended_set(sub: u8, value: u8) -> [u8; 10] {
    let mut p = [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, sub, 0x00, value, 0x00];
    p[9] = ddcci_xor(&p[..9]);
    p
}

pub(crate) fn build_get_standard(vcp: u8) -> [u8; 6] {
    let mut p = [0x6e, 0x51, 0x82, 0x01, vcp, 0x00];
    p[5] = ddcci_xor(&p[..5]);
    p
}

pub(crate) fn build_get_extended(sub: u8) -> [u8; 8] {
    let mut p = [0x6e, 0x51, 0x84, 0x01, 0xe2, 0xa0, sub, 0x00];
    p[7] = ddcci_xor(&p[..7]);
    p
}

/// Build a "read info string" request. Address is the 4-byte selector
/// (the first byte gates the page; later bytes are field-specific).
pub(crate) fn build_get_info(addr: [u8; 4]) -> [u8; 10] {
    let mut p = [
        0x6e, 0x51, 0x86, 0x01, 0xfe, addr[0], addr[1], addr[2], addr[3], 0x00,
    ];
    p[9] = ddcci_xor(&p[..9]);
    p
}

/// Parse an info-string reply. Two envelopes are observed on this monitor:
///   * Standard `6e <len> 02 fe <addr_echo> <ascii…> <xor>` — used by the
///     `e1`, `e9` info pages.
///   * Raw `6e <len> <ascii…> <xor>` — used by `ef 13 00 20`, which reads
///     the asset EEPROM directly and returns just the bytes.
/// Both shapes share the leading source byte, the length byte, and the
/// trailing XOR seeded from `0x50`. Returns the ASCII payload trimmed at
/// the first NUL.
pub(crate) fn parse_info_reply(buf: &[u8]) -> Result<String> {
    if buf.len() < 4 {
        return Err(anyhow!("short info reply ({} bytes)", buf.len()));
    }
    if buf[0] != 0x6e {
        return Err(anyhow!("unexpected source byte {:02x}", buf[0]));
    }
    let n = (buf[1] & 0x7f) as usize;
    if buf.len() < 2 + n + 1 {
        return Err(anyhow!("info reply truncated (need {}, got {})", n + 3, buf.len()));
    }
    let calc = 0x50u8 ^ ddcci_xor(&buf[..2 + n]);
    if buf[2 + n] != calc {
        return Err(anyhow!(
            "info reply checksum mismatch (got {:02x}, expected {:02x})",
            buf[2 + n],
            calc
        ));
    }
    let body = &buf[2..2 + n];
    // Standard envelope starts with `02 fe <addr_echo>`; asset-EEPROM reply
    // is raw ASCII with no prefix.
    let payload = if body.len() >= 3 && body[0] == 0x02 && body[1] == 0xfe {
        &body[3..]
    } else {
        body
    };
    let end = payload.iter().position(|&b| b == 0).unwrap_or(payload.len());
    Ok(String::from_utf8_lossy(&payload[..end]).trim().to_string())
}

/// Parse a standard MCCS get-VCP reply (`6e 88 02 00 vcp type maxH maxL curH curL xor`).
/// The same layout applies to extended-VCP replies — the device reports `vcp=0xe2`
/// and omits the e2a0 sub byte, since the caller already knows which sub it asked for.
pub(crate) fn parse_get_reply(buf: &[u8]) -> Result<u16> {
    if buf.len() < 12 {
        return Err(anyhow!("short DDC/CI reply ({} bytes)", buf.len()));
    }
    if buf[0] != 0x6e {
        return Err(anyhow!("unexpected source byte {:02x}", buf[0]));
    }
    if buf[2] != 0x02 {
        return Err(anyhow!("not a get-VCP reply (opcode {:02x})", buf[2]));
    }
    if buf[3] != 0x00 {
        return Err(anyhow!("monitor reported error code {:02x}", buf[3]));
    }
    let calc = 0x50u8 ^ ddcci_xor(&buf[..10]);
    if buf[10] != calc {
        return Err(anyhow!(
            "checksum mismatch: got {:02x}, expected {:02x}",
            buf[10],
            calc
        ));
    }
    Ok(u16::from_be_bytes([buf[8], buf[9]]))
}

// ── PhilipsEvnia49Protocol ────────────────────────────────────────────────────

/// DDC/CI MCCS protocol for the Philips Evnia 49 monitor.
pub struct PhilipsEvnia49Protocol {
    pub(crate) transport: std::sync::Mutex<Option<UsbControlTransport>>,
    /// Tracks when the last write was issued so WRITE_DELAY can be enforced.
    last_write: std::sync::Mutex<Option<std::time::Instant>>,
}

impl PhilipsEvnia49Protocol {
    pub fn new() -> Self {
        Self {
            transport: std::sync::Mutex::new(None),
            last_write: std::sync::Mutex::new(None),
        }
    }

    pub fn open(&self, vid: u16, pid: u16, interface: u8) -> Result<()> {
        let t = UsbControlTransport::open(vid, pid, interface)?;
        *self.transport.lock().unwrap() = Some(t);
        Ok(())
    }

    pub fn close(&self) {
        if let Some(t) = self.transport.lock().unwrap().take() {
            t.release();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.transport.lock().unwrap().is_some()
    }

    pub async fn write_packet(&self, payload: &[u8]) -> Result<()> {
        let sleep_for = {
            let last = self.last_write.lock().unwrap();
            last.and_then(|t| WRITE_DELAY.checked_sub(t.elapsed()))
        };
        if let Some(d) = sleep_for {
            tokio::time::sleep(d).await;
        }
        let result = {
            let guard = self.transport.lock().unwrap();
            let t = guard.as_ref().ok_or_else(|| anyhow!("not connected"))?;
            t.write_control(BMREQ_OUT, BREQ_DDCCI_WRITE, 0, 0, payload)
        };
        *self.last_write.lock().unwrap() = Some(std::time::Instant::now());
        result
    }

    pub async fn read_get_reply(&self) -> Result<u16> {
        tokio::time::sleep(READ_DELAY).await;
        let mut buf = [0u8; 32];
        let n = {
            let guard = self.transport.lock().unwrap();
            let t = guard.as_ref().ok_or_else(|| anyhow!("not connected"))?;
            t.read_control(BMREQ_IN, BREQ_DDCCI_READ, 0, READ_W_INDEX, &mut buf)?
        };
        parse_get_reply(&buf[..n])
    }

    pub async fn get_standard(&self, vcp: u8) -> Result<u16> {
        self.write_packet(&build_get_standard(vcp)).await?;
        self.read_get_reply().await
    }

    pub async fn get_extended(&self, sub: u8) -> Result<u16> {
        self.write_packet(&build_get_extended(sub)).await?;
        self.read_get_reply().await
    }

    /// Read an info-string at the given 4-byte address. The reply is up to
    /// 32 bytes of buffered USB data; we only need the leading frame and
    /// the parser stops at the embedded NUL terminator.
    pub async fn get_info(&self, addr: [u8; 4]) -> Result<String> {
        self.write_packet(&build_get_info(addr)).await?;
        tokio::time::sleep(READ_DELAY).await;
        let mut buf = [0u8; 32];
        let n = {
            let guard = self.transport.lock().unwrap();
            let t = guard.as_ref().ok_or_else(|| anyhow!("not connected"))?;
            t.read_control(BMREQ_IN, BREQ_DDCCI_READ, 0, READ_W_INDEX, &mut buf)?
        };
        parse_info_reply(&buf[..n])
    }
}

// ── PhilipsAmbiglowProtocol ───────────────────────────────────────────────────

// USB control transfer parameters for the Ambiglow ENE controller.
const AMBIGLOW_BMREQ_OUT: u8 = 0x40;
const AMBIGLOW_BREQ: u8 = 0x80;

/// ENE Technology RGB controller protocol for the Philips Evnia 49 Ambiglow LEDs.
pub struct PhilipsAmbiglowProtocol {
    pub(crate) transport: std::sync::Mutex<Option<UsbControlTransport>>,
}

impl PhilipsAmbiglowProtocol {
    pub fn new() -> Self {
        Self { transport: std::sync::Mutex::new(None) }
    }

    pub fn open(&self, vid: u16, pid: u16, interface: u8) -> Result<()> {
        let t = UsbControlTransport::open(vid, pid, interface)?;
        *self.transport.lock().unwrap() = Some(t);
        Ok(())
    }

    pub fn close(&self) {
        if let Some(t) = self.transport.lock().unwrap().take() {
            t.release();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.transport.lock().unwrap().is_some()
    }

    pub fn write(&self, address: u16, data: &[u8]) -> Result<()> {
        let guard = self.transport.lock().unwrap();
        let t = guard.as_ref().ok_or_else(|| anyhow!("not connected"))?;
        t.write_control(AMBIGLOW_BMREQ_OUT, AMBIGLOW_BREQ, 0, address, data)
            .with_context(|| format!("write @0x{:04X} ({} bytes)", address, data.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddcci_xor_empty() {
        assert_eq!(ddcci_xor(&[]), 0);
    }

    #[test]
    fn build_write_checksum() {
        let p = build_write(0x10, 50);
        let expected = p[..7].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(p[7], expected);
    }

    #[test]
    fn build_extended_set_checksum() {
        let p = build_extended_set(0x04, 1);
        let expected = p[..9].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(p[9], expected);
    }

    #[test]
    fn build_get_standard_checksum() {
        let p = build_get_standard(0x10);
        let expected = p[..5].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(p[5], expected);
    }

    #[test]
    fn build_get_extended_checksum() {
        let p = build_get_extended(0x04);
        let expected = p[..7].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(p[7], expected);
    }

    #[test]
    fn build_get_info_checksum() {
        let p = build_get_info([0xE9, 0x0D, 0x00, 0x00]);
        let expected = p[..9].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(p[9], expected);
    }

    #[test]
    fn parse_get_reply_rejects_short_buffer() {
        assert!(parse_get_reply(&[0u8; 4]).is_err());
    }

    #[test]
    fn parse_info_reply_rejects_short_buffer() {
        assert!(parse_info_reply(&[0u8; 2]).is_err());
    }
}

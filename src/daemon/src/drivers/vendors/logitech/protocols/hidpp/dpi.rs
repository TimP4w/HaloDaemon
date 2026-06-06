//! ADJUSTABLE_DPI (feature 0x2201) codecs.
//!
//! Pure byte encoders/decoders for the HID++ 2.0 ADJUSTABLE_DPI feature —
//! no `&self`, no messenger, just `&[u8]`/`u16` in, bytes/values out.

/// Parse raw DPI list bytes from ADJUSTABLE_DPI func=0x10 chunks.
/// Handles range encoding: `val >> 13 == 0b111` means the next u16 is the
/// range end and `val & 0x1FFF` is the step.
pub fn parse_dpi_list(raw: &[u8]) -> Vec<u16> {
    let mut list: Vec<u16> = Vec::new();
    let mut i = 0;
    while i + 1 < raw.len() {
        let val = u16::from_be_bytes([raw[i], raw[i + 1]]);
        if val == 0 {
            break;
        }
        if val >> 13 == 0b111 {
            let step = val & 0x1FFF;
            if i + 3 < raw.len() && step > 0 {
                let end = u16::from_be_bytes([raw[i + 2], raw[i + 3]]);
                if let Some(&last) = list.last() {
                    let mut cur = last + step;
                    while cur <= end {
                        list.push(cur);
                        cur = cur.saturating_add(step);
                    }
                }
                i += 4;
                continue;
            } else {
                break;
            }
        } else {
            list.push(val);
            i += 2;
        }
    }
    list
}

/// Parse the current DPI from an ADJUSTABLE_DPI func=0x20 (getDpi) reply.
///
/// Reply layout varies by device: longer replies echo the sensor index in
/// byte 0 and carry the DPI big-endian in bytes 1..3; shorter replies put the
/// DPI big-endian in bytes 0..2. Returns `None` when the reply is too short or
/// decodes to 0 — a value the firmware never genuinely reports, so treating it
/// as "unknown" is safer than caching a bogus 0.
pub fn parse_current_dpi(reply: &[u8]) -> Option<u16> {
    let dpi = if reply.len() >= 3 {
        u16::from_be_bytes([reply[1], reply[2]])
    } else if reply.len() >= 2 {
        u16::from_be_bytes([reply[0], reply[1]])
    } else {
        return None;
    };
    (dpi != 0).then_some(dpi)
}

/// Encode the ADJUSTABLE_DPI (0x2201) setSensorDPI (func 0x30) parameter block.
///
/// Layout: `[sensor=0, dpi_hi, dpi_lo]` — sensor index 0, then the DPI
/// big-endian. This is the standard 3-byte 0x2201 form. An earlier 5-byte
/// variant that repeated the DPI pair was rejected by some firmware
/// (G502 X Plus) with HID++ `INVALID_ARGUMENT`.
pub fn encode_set_dpi(dpi: u16) -> [u8; 3] {
    [0x00, (dpi >> 8) as u8, dpi as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dpi_list_explicit_values() {
        // 400, 800, 1600 in big-endian u16, terminated by 0x0000
        let raw: &[u8] = &[0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0x00, 0x00];
        let list = parse_dpi_list(raw);
        assert_eq!(list, vec![400, 800, 1600]);
    }

    #[test]
    fn test_dpi_list_range_encoding() {
        // 400, then range marker step=400 → entries 800, 1200, 1600
        let raw: &[u8] = &[
            0x01, 0x90, // 400
            0xE1, 0x90, // range marker: 0xE000 | step(400=0x190) = 0xE190
            0x06, 0x40, // end = 1600
            0x00, 0x00,
        ];
        let list = parse_dpi_list(raw);
        assert_eq!(list, vec![400, 800, 1200, 1600]);
    }

    #[test]
    fn test_parse_current_dpi_long_reply_with_sensor_echo() {
        // byte 0 = sensor index echo, bytes 1..3 = DPI big-endian (1600).
        assert_eq!(parse_current_dpi(&[0x00, 0x06, 0x40, 0x00, 0x00]), Some(1600));
    }

    #[test]
    fn test_parse_current_dpi_short_reply() {
        // No sensor echo: DPI big-endian in bytes 0..2 (800).
        assert_eq!(parse_current_dpi(&[0x03, 0x20]), Some(800));
    }

    #[test]
    fn test_parse_current_dpi_rejects_zero() {
        // A 0 DPI is never genuine — must read back as "unknown", not Some(0),
        // so a momentary-DPI restore point is never a bogus 0.
        assert_eq!(parse_current_dpi(&[0x00, 0x00, 0x00]), None);
        assert_eq!(parse_current_dpi(&[0x00, 0x00]), None);
    }

    #[test]
    fn test_parse_current_dpi_too_short() {
        assert_eq!(parse_current_dpi(&[]), None);
        assert_eq!(parse_current_dpi(&[0x42]), None);
    }

    #[test]
    fn test_encode_set_dpi() {
        // Standard 0x2201 setSensorDPI: 3 bytes — sensor 0, DPI big-endian.
        // (A non-standard 5-byte form that repeated the DPI pair was rejected
        // by G502 X Plus firmware with INVALID_ARGUMENT.)
        assert_eq!(encode_set_dpi(1600), [0x00, 0x06, 0x40]);
        assert_eq!(encode_set_dpi(800), [0x00, 0x03, 0x20]);
        assert_eq!(encode_set_dpi(3200), [0x00, 0x0c, 0x80]);
        assert_eq!(encode_set_dpi(0), [0x00, 0x00, 0x00]);
    }
}

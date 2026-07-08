pub const FRAME_JSON: u8 = 1;
pub const FRAME_BINARY: u8 = 2;

pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

pub fn payload_exceeds_max(payload_len: u32) -> bool {
    payload_len > MAX_PAYLOAD
}

/// Build a 5-byte frame header + payload.
fn write_frame(frame_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("frame payload exceeds u32 max");
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(frame_type);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn encode_json_frame(msg: &serde_json::Value) -> Option<Vec<u8>> {
    let payload = serde_json::to_vec(msg).ok()?;
    let len = u32::try_from(payload.len()).ok()?;
    if payload_exceeds_max(len) {
        return None;
    }
    Some(write_frame(FRAME_JSON, &payload))
}

pub fn encode_binary_frame(payload: &[u8]) -> Vec<u8> {
    write_frame(FRAME_BINARY, payload)
}

pub fn decode_header(header: &[u8; 5]) -> (u8, u32) {
    let frame_type = header[0];
    let payload_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    (frame_type, payload_len)
}

/// Encodes a binary payload with a `req_id`/`content_type` header. Each header
/// field's length is stored in a `u16`, so an over-long field is rejected
/// (`None`) rather than truncated.
pub fn encode_binary_payload(req_id: &str, content_type: &str, data: &[u8]) -> Option<Vec<u8>> {
    let rid = req_id.as_bytes();
    let ct = content_type.as_bytes();
    if rid.len() > u16::MAX as usize || ct.len() > u16::MAX as usize {
        return None;
    }
    let mut buf = Vec::with_capacity(4 + rid.len() + ct.len() + data.len());
    buf.extend_from_slice(&(rid.len() as u16).to_be_bytes());
    buf.extend_from_slice(&(ct.len() as u16).to_be_bytes());
    buf.extend_from_slice(rid);
    buf.extend_from_slice(ct);
    buf.extend_from_slice(data);
    Some(buf)
}

pub fn decode_binary_payload(payload: &[u8]) -> Option<(String, String, Vec<u8>)> {
    if payload.len() < 4 {
        return None;
    }
    let req_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let ct_len = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    let header_end = 4 + req_len + ct_len;
    if payload.len() < header_end {
        return None;
    }
    let req_id = std::str::from_utf8(&payload[4..4 + req_len])
        .ok()?
        .to_owned();
    let content_type = std::str::from_utf8(&payload[4 + req_len..header_end])
        .ok()?
        .to_owned();
    let data = payload[header_end..].to_vec();
    Some((req_id, content_type, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_json_frame_header_and_payload() {
        let msg = serde_json::json!({"type": "ping"});
        let frame = encode_json_frame(&msg).unwrap();
        assert_eq!(frame[0], FRAME_JSON);
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len, frame.len() - 5);
        let recovered: serde_json::Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(recovered, msg);
    }

    #[test]
    fn encode_binary_frame_header_and_payload() {
        let data = b"hello binary";
        let frame = encode_binary_frame(data);
        assert_eq!(frame[0], FRAME_BINARY);
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len, data.len());
        assert_eq!(&frame[5..], data);
    }

    #[test]
    fn decode_header_recovers_type_and_length() {
        let header: [u8; 5] = [FRAME_JSON, 0, 0, 0, 42];
        let (frame_type, payload_len) = decode_header(&header);
        assert_eq!(frame_type, FRAME_JSON);
        assert_eq!(payload_len, 42);
    }

    #[test]
    fn binary_payload_round_trip() {
        let req_id = "req-123";
        let content_type = "image/png";
        let data = b"\x89PNG\r\n";
        let encoded = encode_binary_payload(req_id, content_type, data).unwrap();
        let (r, ct, d) = decode_binary_payload(&encoded).unwrap();
        assert_eq!(r, req_id);
        assert_eq!(ct, content_type);
        assert_eq!(d, data);
    }

    #[test]
    fn decode_binary_payload_returns_none_for_short_input() {
        assert!(decode_binary_payload(&[]).is_none());
        assert!(decode_binary_payload(&[0, 0, 0]).is_none());
    }

    #[test]
    fn decode_binary_payload_returns_none_when_header_claims_more_than_available() {
        // req_len=100, ct_len=0 but payload is only 4 bytes
        let payload = [0, 100, 0, 0];
        assert!(decode_binary_payload(&payload).is_none());
    }

    #[test]
    fn decode_binary_payload_accepts_exactly_four_byte_header() {
        // 4-byte input (req_len=0, ct_len=0, no body) must decode to empty fields.
        let (r, c, d) = decode_binary_payload(&[0, 0, 0, 0]).expect("4-byte header decodes");
        assert_eq!(r, "");
        assert_eq!(c, "");
        assert!(d.is_empty());
    }

    #[test]
    fn decode_binary_payload_accepts_payload_of_exactly_header_length() {
        // Total length equals header_end exactly; must still decode (empty data).
        let payload = [0, 2, 0, 1, b'a', b'b', b'c'];
        let (r, c, d) = decode_binary_payload(&payload).expect("exact-length payload decodes");
        assert_eq!(r, "ab");
        assert_eq!(c, "c");
        assert!(d.is_empty());
    }

    #[test]
    fn encode_binary_payload_rejects_oversize_header_fields() {
        let huge = "x".repeat(u16::MAX as usize + 1);
        assert!(encode_binary_payload(&huge, "ct", b"").is_none());
        assert!(encode_binary_payload("rid", &huge, b"").is_none());
        // Exactly u16::MAX bytes still fits.
        let max = "x".repeat(u16::MAX as usize);
        assert!(encode_binary_payload(&max, "ct", b"").is_some());
    }

    #[test]
    fn payload_within_cap_is_accepted() {
        assert!(!payload_exceeds_max(MAX_PAYLOAD));
        assert!(!payload_exceeds_max(0));
    }

    #[test]
    fn payload_over_cap_is_rejected() {
        assert!(payload_exceeds_max(MAX_PAYLOAD + 1));
        assert!(payload_exceeds_max(u32::MAX));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `decode_binary_payload(encode_binary_payload(x)) == x` for any inputs.
        #[test]
        fn binary_payload_round_trips(
            req_id in ".{0,300}",
            content_type in ".{0,300}",
            data in prop::collection::vec(any::<u8>(), 0..4096),
        ) {
            let encoded = encode_binary_payload(&req_id, &content_type, &data)
                .expect("short header fields must encode");
            let (r, c, d) = decode_binary_payload(&encoded)
                .expect("a freshly encoded payload must decode");
            prop_assert_eq!(r, req_id);
            prop_assert_eq!(c, content_type);
            prop_assert_eq!(d, data);
        }

        /// `decode_binary_payload` must never panic on arbitrary bytes — it is
        /// fed untrusted data straight off the socket. It may return None.
        #[test]
        fn decode_binary_payload_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_binary_payload(&bytes);
        }

        /// The 5-byte binary frame header round-trips the type byte and payload
        /// length, and the declared length equals the actual payload length.
        #[test]
        fn binary_frame_header_round_trips(payload in prop::collection::vec(any::<u8>(), 0..1024)) {
            let frame = encode_binary_frame(&payload);
            let header: [u8; 5] = frame[..5].try_into().unwrap();
            let (frame_type, len) = decode_header(&header);
            prop_assert_eq!(frame_type, FRAME_BINARY);
            prop_assert_eq!(len as usize, payload.len());
            prop_assert_eq!(&frame[5..], &payload[..]);
        }

        /// The cap check is a pure threshold at exactly MAX_PAYLOAD.
        #[test]
        fn payload_cap_is_a_clean_threshold(len in any::<u32>()) {
            prop_assert_eq!(payload_exceeds_max(len), len > MAX_PAYLOAD);
        }

        /// Any oversized inbound payload length is rejected before an allocation
        /// would occur — the DoS/OOM guard `encode_json_frame` and callers rely on.
        #[test]
        fn oversized_payload_lengths_are_always_rejected(extra in 1u64..=(u32::MAX as u64 - MAX_PAYLOAD as u64)) {
            let len = MAX_PAYLOAD + extra as u32;
            prop_assert!(payload_exceeds_max(len));
        }
    }
}

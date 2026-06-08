pub const FRAME_JSON: u8 = 1;
pub const FRAME_BINARY: u8 = 2;

/// Maximum accepted inbound frame payload size to prevent OOM/DoS
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

pub fn payload_exceeds_max(payload_len: u32) -> bool {
    payload_len > MAX_PAYLOAD
}

pub fn encode_json_frame(msg: &serde_json::Value) -> Vec<u8> {
    let payload = serde_json::to_vec(msg).unwrap_or_default();
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(FRAME_JSON);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    buf
}

pub fn encode_binary_frame(payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(FRAME_BINARY);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn decode_header(header: &[u8; 5]) -> (u8, u32) {
    let frame_type = header[0];
    let payload_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    (frame_type, payload_len)
}

pub fn encode_binary_payload(req_id: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
    let rid = req_id.as_bytes();
    let ct = content_type.as_bytes();
    let mut buf = Vec::with_capacity(4 + rid.len() + ct.len() + data.len());
    buf.extend_from_slice(&(rid.len() as u16).to_be_bytes());
    buf.extend_from_slice(&(ct.len() as u16).to_be_bytes());
    buf.extend_from_slice(rid);
    buf.extend_from_slice(ct);
    buf.extend_from_slice(data);
    buf
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
    let req_id = String::from_utf8(payload[4..4 + req_len].to_vec()).ok()?;
    let content_type = String::from_utf8(payload[4 + req_len..header_end].to_vec()).ok()?;
    let data = payload[header_end..].to_vec();
    Some((req_id, content_type, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_json_frame_header_and_payload() {
        let msg = serde_json::json!({"type": "ping"});
        let frame = encode_json_frame(&msg);
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
        let encoded = encode_binary_payload(req_id, content_type, data);
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

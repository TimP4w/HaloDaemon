pub const FRAME_JSON: u8 = 1;

pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

pub fn payload_exceeds_max(payload_len: u32) -> bool {
    payload_len > MAX_PAYLOAD
}

pub fn encode_json_frame(msg: &serde_json::Value) -> Option<Vec<u8>> {
    let payload = serde_json::to_vec(msg).ok()?;
    let len = u32::try_from(payload.len()).ok()?;
    if payload_exceeds_max(len) {
        return None;
    }
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(FRAME_JSON);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    Some(buf)
}

pub fn decode_header(header: &[u8; 5]) -> (u8, u32) {
    let frame_type = header[0];
    let payload_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    (frame_type, payload_len)
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn json_frame_round_trips(text in ".{0,512}") {
            let msg = serde_json::json!({ "type": "ping", "text": text });
            let frame = encode_json_frame(&msg).unwrap();
            let header: [u8; 5] = frame[..5].try_into().unwrap();
            let (frame_type, len) = decode_header(&header);
            prop_assert_eq!(frame_type, FRAME_JSON);
            prop_assert_eq!(len as usize, frame.len() - 5);
            let recovered: serde_json::Value = serde_json::from_slice(&frame[5..]).unwrap();
            prop_assert_eq!(recovered, msg);
        }

        #[test]
        fn payload_cap_is_a_clean_threshold(len in any::<u32>()) {
            prop_assert_eq!(payload_exceeds_max(len), len > MAX_PAYLOAD);
        }
    }
}

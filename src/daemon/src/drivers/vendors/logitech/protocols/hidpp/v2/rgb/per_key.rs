//! Pure HID++ frame encoder for PER_KEY_LIGHTING (feature 0x8081).
//!
//! Naive streaming sends one `setIndividualRGBZones` packet per 4 keys, which
//! floods the HID++ bus. [`encode_frame`] cuts that down by diffing against the
//! previous frame, run-length-encoding equal-colour spans into
//! `setRangeRGBZones` (0x50), and using `setConsecutiveRGBZones` (0x20) for
//! consecutive-id varying runs.

use halod_shared::types::RgbColor;

use crate::drivers::vendors::logitech::protocols::hidpp::build_packet;

/// HID++ software id stamped into the low nibble of every function byte, matching
/// `HidppMessenger::feature_request`.
const SWID: u8 = 0x01;

// PER_KEY_LIGHTING_V2 (0x8081) function bytes (high nibble).
const FN_SET_INDIVIDUAL: u8 = 0x10;
const FN_SET_CONSECUTIVE: u8 = 0x20;
const FN_DELTA_5BIT: u8 = 0x30;
const FN_DELTA_4BIT: u8 = 0x40;
const FN_SET_RANGE: u8 = 0x50;
const FN_FRAME_END: u8 = 0x70;

/// `setRangeRGBZones` carries three `[start, end, r, g, b]` entries per report.
const RANGES_PER_PACKET: usize = 3;
/// `setConsecutiveRGBZones` carries `[firstId]` + five `[r, g, b]` per report.
const KEYS_PER_CONSECUTIVE: usize = 5;
/// A varying run shorter than this is cheaper as individual `setRange` entries
/// than as its own `setConsecutive`/delta packet.
const MIN_VARYING_RUN: usize = 3;

/// Delta-compressed packets (`0x30`/`0x40`) are disabled: their wire bit-layout is unverified against hardware.
const ENABLE_DELTA: bool = false;

/// Per-zone record of the colours last written to the hardware, used to diff the
/// next frame. Indexed by position in the zone's LED list.
#[derive(Default)]
pub struct PkFrameCache {
    last: Vec<Option<RgbColor>>,
}

/// Encode one RGB animation frame into HID++ long packets for feature `pk_idx`.
///
/// `keys` is `(firmware zone id, colour)` in the zone's LED order. Only keys
/// whose colour differs from `cache` are emitted; the returned packets end with
/// a `frameEnd` commit. Returns an empty `Vec` when nothing changed — the caller
/// should then skip the bus write entirely.
pub fn encode_frame(
    keys: &[(u8, RgbColor)],
    cache: &mut PkFrameCache,
    devnum: u8,
    pk_idx: u8,
) -> Vec<Vec<u8>> {
    // A zone re-enumeration changes the LED count; drop a stale cache.
    if cache.last.len() != keys.len() {
        cache.last = vec![None; keys.len()];
    }

    // Diff against the cache, then sort by zone id so runs become adjacent.
    let mut sorted: Vec<KeyState> = keys
        .iter()
        .enumerate()
        .map(|(i, &(id, color))| KeyState {
            id,
            color,
            changed: cache.last[i] != Some(color),
        })
        .collect();
    if !sorted.iter().any(|k| k.changed) {
        return Vec::new();
    }
    sorted.sort_by_key(|k| k.id);

    // Group into maximal equal-colour blocks. Every real key inside a block's
    // id span shares the block colour, so a `setRange` over it can never clobber
    // an unrelated key — gaps in the span are phantom ids and no-op.
    let blocks = build_blocks(&sorted);

    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut ranges: Vec<[u8; 5]> = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        let b = &blocks[i];
        if !b.dirty {
            i += 1;
            continue;
        }
        if b.len > 1 {
            // Uniform multi-key span — one range entry is optimal.
            ranges.push([b.first_id, b.last_id, b.color.r, b.color.g, b.color.b]);
            i += 1;
            continue;
        }
        // Single-key dirty block: try to grow a consecutive-id varying run.
        let mut j = i;
        while j < blocks.len()
            && blocks[j].dirty
            && blocks[j].len == 1
            && (j == i || blocks[j].first_id == blocks[j - 1].first_id.saturating_add(1))
        {
            j += 1;
        }
        let run: Vec<(u8, RgbColor)> = blocks[i..j].iter().map(|b| (b.first_id, b.color)).collect();
        if run.len() >= MIN_VARYING_RUN {
            encode_varying_run(&run, devnum, pk_idx, &mut packets);
        } else {
            for &(id, c) in &run {
                ranges.push([id, id, c.r, c.g, c.b]);
            }
        }
        i = j;
    }

    // Pack accumulated range entries three per report.
    for chunk in ranges.chunks(RANGES_PER_PACKET) {
        let mut params = Vec::with_capacity(chunk.len() * 5);
        for e in chunk {
            params.extend_from_slice(e);
        }
        packets.push(build_packet(
            devnum,
            pk_idx,
            FN_SET_RANGE | SWID,
            &params,
            true,
        ));
    }

    packets.push(build_packet(
        devnum,
        pk_idx,
        FN_FRAME_END | SWID,
        &[0x00],
        true,
    ));

    // The cache must reflect the full frame, not just the changed keys.
    for (i, &(_, color)) in keys.iter().enumerate() {
        cache.last[i] = Some(color);
    }
    packets
}

/// Encode up to-four-at-a-time per-key colour pairs as `setIndividual` (func
/// 0x10) batches followed by a `commit` (func 0x70). A short final batch is
/// padded with its last pair so key 0 is never zero-keyed.
pub fn encode_individual_pairs(pairs: &[(u8, u8, u8, u8)], devnum: u8, pk_idx: u8) -> Vec<Vec<u8>> {
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut packets = Vec::with_capacity(pairs.len() / 4 + 2);
    for chunk in pairs.chunks(4) {
        let last = *chunk.last().unwrap();
        let mut batch = chunk.to_vec();
        while batch.len() < 4 {
            batch.push(last);
        }
        let mut buf = [0u8; 16];
        for (i, &(k, r, g, bl)) in batch.iter().take(4).enumerate() {
            buf[i * 4] = k;
            buf[i * 4 + 1] = r;
            buf[i * 4 + 2] = g;
            buf[i * 4 + 3] = bl;
        }
        packets.push(build_packet(
            devnum,
            pk_idx,
            FN_SET_INDIVIDUAL | SWID,
            &buf,
            true,
        ));
    }
    packets.push(encode_commit(devnum, pk_idx));
    packets
}

/// Build the PER_KEY_LIGHTING COMMIT (func 0x70) packet.
pub fn encode_commit(devnum: u8, pk_idx: u8) -> Vec<u8> {
    build_packet(devnum, pk_idx, FN_FRAME_END | SWID, &[0x00], true)
}

struct KeyState {
    id: u8,
    color: RgbColor,
    changed: bool,
}

/// A maximal run of consecutively-sorted keys sharing one colour.
struct Block {
    first_id: u8,
    last_id: u8,
    len: usize,
    color: RgbColor,
    dirty: bool,
}

fn build_blocks(sorted: &[KeyState]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for k in sorted {
        match blocks.last_mut() {
            Some(b) if b.color == k.color => {
                b.last_id = k.id;
                b.len += 1;
                b.dirty |= k.changed;
            }
            _ => blocks.push(Block {
                first_id: k.id,
                last_id: k.id,
                len: 1,
                color: k.color,
                dirty: k.changed,
            }),
        }
    }
    blocks
}

/// Encode a run of consecutive-id, all-changed keys with varying colours. Tries
/// delta compression first (when enabled), else `setConsecutiveRGBZones`.
fn encode_varying_run(run: &[(u8, RgbColor)], devnum: u8, pk_idx: u8, packets: &mut Vec<Vec<u8>>) {
    if ENABLE_DELTA {
        if let Some(delta) = encode_delta(run, devnum, pk_idx) {
            packets.extend(delta);
            return;
        }
    }
    for chunk in run.chunks(KEYS_PER_CONSECUTIVE) {
        let mut params = Vec::with_capacity(1 + chunk.len() * 3);
        params.push(chunk[0].0);
        for &(_, c) in chunk {
            params.extend_from_slice(&[c.r, c.g, c.b]);
        }
        packets.push(build_packet(
            devnum,
            pk_idx,
            FN_SET_CONSECUTIVE | SWID,
            &params,
            true,
        ));
    }
}

/// Encode a consecutive-id run as delta-compressed packets. Returns `None` when
/// the per-channel deltas do not fit the 4-bit or 5-bit range.
fn encode_delta(run: &[(u8, RgbColor)], devnum: u8, pk_idx: u8) -> Option<Vec<Vec<u8>>> {
    let fits = |bits: u32| -> bool {
        let lo = -(1i16 << (bits - 1));
        let hi = (1i16 << (bits - 1)) - 1;
        run.windows(2).all(|w| {
            let (a, b) = (w[0].1, w[1].1);
            [
                b.r as i16 - a.r as i16,
                b.g as i16 - a.g as i16,
                b.b as i16 - a.b as i16,
            ]
            .iter()
            .all(|&d| d >= lo && d <= hi)
        })
    };
    let (bits, func) = if fits(4) {
        (4u32, FN_DELTA_4BIT)
    } else if fits(5) {
        (5u32, FN_DELTA_5BIT)
    } else {
        return None;
    };

    let keys_per_packet = 1 + (12 * 8) / (3 * bits as usize);
    let mut packets = Vec::new();
    for group in run.chunks(keys_per_packet) {
        let head = group[0].1;
        let mut params = vec![group[0].0, head.r, head.g, head.b];
        let mut writer = BitWriter::new();
        for w in group.windows(2) {
            let (a, b) = (w[0].1, w[1].1);
            for &d in &[
                b.r as i16 - a.r as i16,
                b.g as i16 - a.g as i16,
                b.b as i16 - a.b as i16,
            ] {
                writer.push(d as u16 & ((1 << bits) - 1), bits);
            }
        }
        params.extend_from_slice(&writer.finish());
        packets.push(build_packet(devnum, pk_idx, func | SWID, &params, true));
    }
    Some(packets)
}

/// Accumulates variable-width fields into bytes, most-significant bit first.
struct BitWriter {
    bytes: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    fn push(&mut self, value: u16, bits: u32) {
        self.acc = (self.acc << bits) | value as u32;
        self.nbits += bits;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.bytes.push((self.acc >> self.nbits) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.bytes.push((self.acc << (8 - self.nbits)) as u8);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::{HIDPP_LONG, LONG_LEN};

    const DEVNUM: u8 = 0x01;
    const PK_IDX: u8 = 0x09;

    fn rgb(r: u8, g: u8, b: u8) -> RgbColor {
        RgbColor { r, g, b }
    }

    fn assert_well_formed(packets: &[Vec<u8>]) {
        for p in packets {
            assert_eq!(p.len(), LONG_LEN);
            assert_eq!(p[0], HIDPP_LONG);
            assert_eq!(p[1], DEVNUM);
            assert_eq!(p[2], PK_IDX);
        }
    }

    fn func(p: &[u8]) -> u8 {
        p[3] & 0xF0
    }

    #[test]
    fn uniform_frame_is_one_range_plus_frame_end() {
        let keys: Vec<_> = (1u8..=87).map(|id| (id, rgb(0, 0, 255))).collect();
        let mut cache = PkFrameCache::default();
        let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

        assert_eq!(packets.len(), 2, "1 setRange + frameEnd");
        assert_well_formed(&packets);
        assert_eq!(func(&packets[0]), FN_SET_RANGE);
        assert_eq!(&packets[0][4..9], &[1, 87, 0, 0, 255]);
        assert_eq!(func(&packets[1]), FN_FRAME_END);
        assert_eq!(packets[1][4], 0x00);
    }

    #[test]
    fn unchanged_frame_emits_nothing() {
        let keys: Vec<_> = (1u8..=87).map(|id| (id, rgb(10, 20, 30))).collect();
        let mut cache = PkFrameCache::default();
        assert_eq!(encode_frame(&keys, &mut cache, DEVNUM, PK_IDX).len(), 2);
        assert!(encode_frame(&keys, &mut cache, DEVNUM, PK_IDX).is_empty());
    }

    #[test]
    fn single_key_change_sends_only_that_key() {
        let mut keys: Vec<_> = (1u8..=87).map(|id| (id, rgb(0, 0, 255))).collect();
        let mut cache = PkFrameCache::default();
        encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

        keys[40].1 = rgb(255, 0, 0); // id 41
        let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

        assert_eq!(packets.len(), 2, "1 setRange + frameEnd");
        assert_well_formed(&packets);
        assert_eq!(func(&packets[0]), FN_SET_RANGE);
        assert_eq!(&packets[0][4..9], &[41, 41, 255, 0, 0]);
    }

    #[test]
    fn consecutive_gradient_uses_set_consecutive() {
        let keys: Vec<_> = (1u8..=10).map(|id| (id, rgb(id, id * 2, id * 3))).collect();
        let mut cache = PkFrameCache::default();
        let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

        assert_eq!(packets.len(), 3);
        assert_well_formed(&packets);
        assert_eq!(func(&packets[0]), FN_SET_CONSECUTIVE);
        assert_eq!(func(&packets[1]), FN_SET_CONSECUTIVE);
        assert_eq!(func(&packets[2]), FN_FRAME_END);
        assert_eq!(packets[0][4], 1, "first id of first chunk");
        assert_eq!(packets[1][4], 6, "first id of second chunk");
    }

    #[test]
    fn scattered_varying_keys_pack_as_range_singletons() {
        let keys: Vec<_> = [1u8, 5, 9, 13, 17]
            .iter()
            .map(|&id| (id, rgb(id, 0, id)))
            .collect();
        let mut cache = PkFrameCache::default();
        let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

        assert_eq!(packets.len(), 3);
        assert_well_formed(&packets);
        assert_eq!(func(&packets[0]), FN_SET_RANGE);
        assert_eq!(func(&packets[1]), FN_SET_RANGE);
        assert_eq!(func(&packets[2]), FN_FRAME_END);
    }

    #[test]
    fn non_empty_result_always_ends_with_frame_end() {
        let keys: Vec<_> = (1u8..=20).map(|id| (id, rgb(id, id, id))).collect();
        let mut cache = PkFrameCache::default();
        let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);
        let last = packets.last().unwrap();
        assert_eq!(func(last), FN_FRAME_END);
        assert_eq!(last[3] & 0x0F, SWID);
        assert_eq!(last[4], 0x00);
    }

    #[test]
    fn cache_records_full_frame_not_just_changes() {
        let mut keys: Vec<_> = (1u8..=87).map(|id| (id, rgb(0, 0, 255))).collect();
        let mut cache = PkFrameCache::default();
        encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);
        keys[0].1 = rgb(1, 2, 3);
        encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);
        assert!(encode_frame(&keys, &mut cache, DEVNUM, PK_IDX).is_empty());
    }

    #[test]
    fn led_count_change_rebuilds_cache() {
        let keys87: Vec<_> = (1u8..=87).map(|id| (id, rgb(5, 5, 5))).collect();
        let keys10: Vec<_> = (1u8..=10).map(|id| (id, rgb(5, 5, 5))).collect();
        let mut cache = PkFrameCache::default();
        encode_frame(&keys87, &mut cache, DEVNUM, PK_IDX);
        assert!(!encode_frame(&keys10, &mut cache, DEVNUM, PK_IDX).is_empty());
    }

    #[test]
    fn delta_encodes_smooth_run_into_fewer_packets() {
        let run: Vec<_> = (0u8..18).map(|i| (i + 1, rgb(i, i, i))).collect();
        let packets = encode_delta(&run, DEVNUM, PK_IDX).expect("smooth run fits 4-bit");
        assert_well_formed(&packets);
        assert_eq!(packets.len(), 2, "18 keys / 9 per packet");
        assert!(packets.iter().all(|p| func(p) == FN_DELTA_4BIT));
    }

    #[test]
    fn delta_rejects_steep_run() {
        let run = vec![(1u8, rgb(0, 0, 0)), (2, rgb(200, 0, 0)), (3, rgb(0, 0, 0))];
        assert!(encode_delta(&run, DEVNUM, PK_IDX).is_none());
    }

    #[test]
    fn individual_pairs_pad_short_final_batch() {
        // 5 pairs → batch of 4 + padded batch of 4 (last pair repeated) + commit.
        let pairs = [
            (1u8, 1, 1, 1),
            (2, 2, 2, 2),
            (3, 3, 3, 3),
            (4, 4, 4, 4),
            (5, 5, 5, 5),
        ];
        let packets = encode_individual_pairs(&pairs, DEVNUM, PK_IDX);
        assert_eq!(packets.len(), 3, "2 setIndividual + commit");
        assert_well_formed(&packets);
        assert_eq!(packets[0][3] & 0xF0, 0x10);
        assert_eq!(&packets[1][4..8], &[5, 5, 5, 5], "fifth pair");
        assert_eq!(&packets[1][8..12], &[5, 5, 5, 5], "padded with last pair");
        assert_eq!(func(&packets[2]), FN_FRAME_END);
    }

    #[test]
    fn bit_writer_packs_msb_first() {
        let mut w = BitWriter::new();
        w.push(0b1010, 4);
        w.push(0b0011, 4);
        assert_eq!(w.finish(), vec![0b1010_0011]);
    }

    proptest! {
        /// Any frame either encodes to nothing (unchanged) or to well-formed
        /// packets ending in a `frameEnd` commit — regardless of key count or
        /// colour pattern.
        #[test]
        fn encode_frame_always_well_formed_and_commits(
            colors in prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), 1..64),
        ) {
            let keys: Vec<(u8, RgbColor)> = colors
                .iter()
                .enumerate()
                .map(|(i, &(r, g, b))| ((i + 1) as u8, rgb(r, g, b)))
                .collect();
            let mut cache = PkFrameCache::default();
            let packets = encode_frame(&keys, &mut cache, DEVNUM, PK_IDX);

            assert_well_formed(&packets);
            if let Some(last) = packets.last() {
                prop_assert_eq!(func(last), FN_FRAME_END);
            }
        }
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! Audio features: EQUALIZER (`0x8310`) and SIDETONE (`0x8300`).
//!
//! Codecs (pure `&[u8]` in / values out) plus the typed [`Hidpp20`] operations
//! that drive them, device-agnostic.
//!
//! Reference: Solaar (GPL-2.0-or-later) — `settings_templates.py`.
use super::{feature, Hidpp20};

// ── EQUALIZER (0x8310) function codes ────────────────────────────────────────
const EQ_GET_INFO: u8 = 0x00;
const EQ_GET_FREQUENCIES: u8 = 0x10;
const EQ_GET_BANDS: u8 = 0x20;
const EQ_SET_BANDS: u8 = 0x30;
/// Read-prefix byte sent with `get_bands` (band offset 0).
const EQ_READ_PREFIX: u8 = 0x00;
/// Write-prefix byte that precedes the custom band set in `set_bands`.
const EQ_WRITE_PREFIX: u8 = 0x02;

// ── SIDETONE (0x8300) function codes ─────────────────────────────────────────
const SIDETONE_GET: u8 = 0x00;
const SIDETONE_SET: u8 = 0x10;

/// Equalizer capabilities, decoded from `EQ_GET_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EqInfo {
    /// Number of EQ bands.
    pub count: u8,
    /// Minimum band level in dB (signed).
    pub db_min: i8,
    /// Maximum band level in dB (signed).
    pub db_max: i8,
}

/// A full equalizer reading: capabilities, per-band centre frequencies (Hz) and
/// current band levels (signed dB).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EqReading {
    pub info: Option<EqInfo>,
    pub freqs: Vec<u16>,
    pub bands: Vec<i8>,
}

// ── Codecs ──────────────────────────────────────────────────────────────────

/// Decode the `SIDETONE_GET` (0x00) reply: byte 0 is the level (0–100).
pub fn parse_sidetone(reply: &[u8]) -> Option<u8> {
    reply.first().map(|&b| b.min(100))
}

/// Encode the `SIDETONE_SET` (0x10) payload: a single level byte (0–100).
fn encode_sidetone(level: u8) -> [u8; 1] {
    [level.min(100)]
}

/// Parse the `EQ_GET_INFO` (0x00) reply: `[count, dbRange, _, dbMin, dbMax]`.
/// A `dbMin`/`dbMax` of 0 means "use ∓dbRange" (Solaar's fix-up).
pub fn parse_eq_info(reply: &[u8]) -> Option<EqInfo> {
    if reply.len() < 5 {
        return None;
    }
    let count = reply[0];
    if count == 0 {
        return None;
    }
    let db_range = reply[1] as i8;
    let raw_min = reply[3] as i8;
    let raw_max = reply[4] as i8;
    let db_min = if raw_min == 0 { -db_range } else { raw_min };
    let db_max = if raw_max == 0 { db_range } else { raw_max };
    Some(EqInfo {
        count,
        db_min,
        db_max,
    })
}

/// Parse one `EQ_GET_FREQUENCIES` (0x10) group reply into up to 7 centre
/// frequencies (Hz). Byte 0 is a header/echo; each frequency is a big-endian
/// u16 at offset `2*b + 1`. `remaining` caps how many bands this group covers.
pub fn parse_eq_frequencies(reply: &[u8], remaining: usize) -> Vec<u16> {
    let mut out = Vec::new();
    for b in 0..remaining.min(7) {
        let lo = 2 * b + 1;
        if lo + 2 > reply.len() {
            break;
        }
        out.push(u16::from_be_bytes([reply[lo], reply[lo + 1]]));
    }
    out
}

/// Decode the `EQ_GET_BANDS` (0x20) reply into `count` signed-dB band levels.
/// Unlike `getFrequencies`, the reply carries no echoed prefix — band `n` is
/// `reply[n]` as `i8` (matches Solaar's `PackedRangeValidator` with `rsbc=0`).
pub fn parse_eq_bands(reply: &[u8], count: u8) -> Vec<i8> {
    (0..count as usize)
        .map(|n| reply.get(n).map(|&b| b as i8).unwrap_or(0))
        .collect()
}

/// Encode the `EQ_SET_BANDS` (0x30) payload: the write-prefix byte followed by
/// each band level as a two's-complement `i8`, clamped to `[db_min, db_max]`.
pub fn encode_eq_bands(values: &[i8], db_min: i8, db_max: i8) -> Vec<u8> {
    let mut payload = Vec::with_capacity(values.len() + 1);
    payload.push(EQ_WRITE_PREFIX);
    for &v in values {
        payload.push(v.clamp(db_min, db_max) as u8);
    }
    payload
}

// ── Typed operations ──────────────────────────────────────────────────────────

impl Hidpp20 {
    /// Read the current sidetone level. `None` when the feature is absent, the
    /// headset is asleep, or the read fails.
    pub async fn read_sidetone(&self) -> Option<u8> {
        let idx = self.idx(feature::SIDETONE)?;
        match self.call(idx, SIDETONE_GET, &[]).await {
            Ok(reply) => parse_sidetone(&reply),
            Err(e) => {
                log::debug!("[HID++2.0] SIDETONE get failed: {e}");
                None
            }
        }
    }

    /// Set the sidetone level (clamped 0–100).
    pub async fn set_sidetone(&self, level: u8) -> anyhow::Result<()> {
        let idx = self
            .idx(feature::SIDETONE)
            .ok_or_else(|| anyhow::anyhow!("SIDETONE not available"))?;
        self.call(idx, SIDETONE_SET, &encode_sidetone(level))
            .await?;
        Ok(())
    }

    /// Read the full equalizer state: info, per-band frequencies and current
    /// levels. Returns the default (info `None`) when the feature is absent or
    /// the device rejects `getInfo`.
    pub async fn read_equalizer(&self) -> EqReading {
        let Some(idx) = self.idx(feature::EQUALIZER) else {
            return EqReading::default();
        };
        let info = match self.call(idx, EQ_GET_INFO, &[]).await {
            Ok(reply) => parse_eq_info(&reply),
            Err(e) => {
                log::debug!("[HID++2.0] EQUALIZER getInfo unsupported: {e}");
                None
            }
        };
        let Some(info) = info else {
            return EqReading::default();
        };

        let mut freqs = Vec::with_capacity(info.count as usize);
        let groups = info.count.div_ceil(7);
        for g in 0..groups {
            let start = g * 7;
            let remaining = (info.count - start) as usize;
            match self.call(idx, EQ_GET_FREQUENCIES, &[start]).await {
                Ok(reply) => freqs.extend(parse_eq_frequencies(&reply, remaining)),
                Err(e) => log::debug!("[HID++2.0] EQUALIZER getFrequencies failed: {e}"),
            }
        }

        let bands = match self.call(idx, EQ_GET_BANDS, &[EQ_READ_PREFIX]).await {
            Ok(reply) => parse_eq_bands(&reply, info.count),
            Err(e) => {
                log::debug!("[HID++2.0] EQUALIZER getBands failed: {e}");
                vec![0; info.count as usize]
            }
        };

        EqReading {
            info: Some(info),
            freqs,
            bands,
        }
    }

    /// Write custom equalizer band levels (clamped to the device's dB range).
    /// Returns the clamped levels the device now holds.
    pub async fn set_eq_bands(&self, values: &[i8], info: EqInfo) -> anyhow::Result<Vec<i8>> {
        let idx = self
            .idx(feature::EQUALIZER)
            .ok_or_else(|| anyhow::anyhow!("EQUALIZER not available"))?;
        let payload = encode_eq_bands(values, info.db_min, info.db_max);
        self.call(idx, EQ_SET_BANDS, &payload).await?;
        Ok(values
            .iter()
            .map(|&b| b.clamp(info.db_min, info.db_max))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn eq_info_applies_db_range_fixup() {
        // count=4, dbRange=12, min/max=0 → span -12..+12.
        let info = parse_eq_info(&[4, 12, 0, 0, 0]).unwrap();
        assert_eq!(
            info,
            EqInfo {
                count: 4,
                db_min: -12,
                db_max: 12
            }
        );
    }

    #[test]
    fn eq_info_honours_explicit_min_max() {
        let info = parse_eq_info(&[10, 12, 0, 0xF8, 6]).unwrap();
        assert_eq!(info.db_min, -8); // 0xF8 = -8
        assert_eq!(info.db_max, 6);
    }

    #[test]
    fn eq_info_rejects_short_or_zero_count() {
        assert!(parse_eq_info(&[4, 12, 0, 0]).is_none());
        assert!(parse_eq_info(&[0, 12, 0, 0, 0]).is_none());
    }

    #[test]
    fn eq_frequencies_parses_big_endian_pairs() {
        // header byte + 31Hz(0x001F) + 250Hz(0x00FA) + 1kHz(0x03E8)
        let reply = [0x00, 0x00, 0x1F, 0x00, 0xFA, 0x03, 0xE8];
        assert_eq!(parse_eq_frequencies(&reply, 3), vec![31, 250, 1000]);
    }

    #[test]
    fn eq_frequencies_caps_at_remaining_and_length() {
        let reply = [0x00, 0x00, 0x1F, 0x00, 0xFA];
        // remaining=7 but only 2 full pairs present.
        assert_eq!(parse_eq_frequencies(&reply, 7), vec![31, 250]);
        // remaining=1 stops after the first.
        assert_eq!(parse_eq_frequencies(&reply, 1), vec![31]);
    }

    #[test]
    fn eq_bands_read_signed_from_byte_zero() {
        // No echoed prefix: -12 (0xF4), 0, +6 start at byte 0.
        let reply = [0xF4, 0x00, 0x06];
        assert_eq!(parse_eq_bands(&reply, 3), vec![-12, 0, 6]);
    }

    #[test]
    fn eq_bands_pads_missing_with_zero() {
        assert_eq!(parse_eq_bands(&[0x06], 3), vec![6, 0, 0]);
    }

    #[test]
    fn encode_eq_bands_prefixes_and_clamps() {
        let payload = encode_eq_bands(&[-20, 0, 20], -12, 12);
        // prefix 0x02, then clamped -12 (0xF4), 0, +12 (0x0C).
        assert_eq!(payload, vec![0x02, 0xF4, 0x00, 0x0C]);
    }

    #[test]
    fn sidetone_round_trips() {
        assert_eq!(parse_sidetone(&[42]), Some(42));
        assert_eq!(parse_sidetone(&[200]), Some(100));
        assert_eq!(parse_sidetone(&[]), None);
        assert_eq!(encode_sidetone(50), [50]);
        assert_eq!(encode_sidetone(250), [100]);
    }

    proptest! {
        /// Encoding then decoding band levels (within range) is the identity,
        /// once the encoder's write-prefix byte is accounted for.
        #[test]
        fn eq_bands_encode_decode_round_trip(
            bands in proptest::collection::vec(-12i8..=12, 1..=10usize),
        ) {
            let payload = encode_eq_bands(&bands, -12, 12);
            // The set payload is `[prefix, b0, b1, ...]`; the get reply carries
            // no prefix, so strip the write-prefix byte before decoding.
            let decoded = parse_eq_bands(&payload[1..], bands.len() as u8);
            prop_assert_eq!(decoded, bands);
        }

        /// Every encoded band stays within the advertised dB range.
        #[test]
        fn eq_bands_encode_stays_in_range(
            raw in proptest::collection::vec(any::<i8>(), 1..=10usize),
            lo in -24i8..=0,
            hi in 0i8..=24,
        ) {
            let payload = encode_eq_bands(&raw, lo, hi);
            for &b in &payload[1..] {
                let v = b as i8;
                prop_assert!(v >= lo && v <= hi);
            }
        }
    }
}

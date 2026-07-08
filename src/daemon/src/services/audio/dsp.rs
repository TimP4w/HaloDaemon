use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

use super::{SpectrumFrame, BANDS, WAVE_SAMPLES};

const FFT_SIZE: usize = 1024;
const HOP: usize = 512;
const BASS_BANDS: usize = 8;
const BASS_HISTORY: usize = 43;
const BEAT_THRESHOLD: f32 = 0.30;
const BEAT_REFRACTORY_SECS: f64 = 0.150;
const RELEASE_TAU_SECS: f32 = 0.12;

/// Log-spaced band edges over `[f_lo, f_hi)`, computed once per sample rate.
struct BandMap {
    /// For each band, the inclusive bin range `[lo, hi]` to fold (max) over.
    bins: Vec<(usize, usize)>,
}

impl BandMap {
    fn new(sample_rate: u32, fft_size: usize) -> Self {
        let f_lo = 40.0f64;
        let f_hi = sample_rate as f64 / 2.0;
        let r = (f_hi / f_lo).powf(1.0 / BANDS as f64);
        let bin_hz = sample_rate as f64 / fft_size as f64;
        // Usable bins are 1..=fft_size/2 (bin 0 is DC).
        let max_bin = fft_size / 2;

        let mut bins = Vec::with_capacity(BANDS);
        for i in 0..BANDS {
            let lo_hz = f_lo * r.powi(i as i32);
            let hi_hz = f_lo * r.powi(i as i32 + 1);
            let mut lo_bin = (lo_hz / bin_hz).round() as usize;
            let mut hi_bin = (hi_hz / bin_hz).round() as usize;
            lo_bin = lo_bin.clamp(1, max_bin);
            hi_bin = hi_bin.clamp(1, max_bin);
            if hi_bin < lo_bin {
                hi_bin = lo_bin;
            }
            bins.push((lo_bin, hi_bin));
        }
        Self { bins }
    }

    /// Fold FFT magnitudes into `BANDS` values using max-over-range; bands
    /// with an empty covered range reuse the nearest bin (they cover at
    /// least one by construction here, but guard anyway).
    fn fold(&self, magnitudes: &[f32]) -> [f32; BANDS] {
        let mut out = [0.0f32; BANDS];
        for (i, &(lo, hi)) in self.bins.iter().enumerate() {
            let lo = lo.min(magnitudes.len().saturating_sub(1));
            let hi = hi.min(magnitudes.len().saturating_sub(1));
            let mut m = 0.0f32;
            for b in &magnitudes[lo..=hi] {
                if *b > m {
                    m = *b;
                }
            }
            out[i] = m;
        }
        out
    }
}

pub struct SpectrumAnalyzer {
    sample_rate: u32,
    fft: Arc<dyn Fft<f32>>,
    hann: [f32; FFT_SIZE],
    band_map: BandMap,
    /// Ring buffer of pending raw samples; a window is consumed every `HOP`
    /// new samples once at least `FFT_SIZE` are buffered.
    pending: VecDeque<f32>,
    bands_smoothed: [f32; BANDS],
    level_smoothed: f32,
    bass_history: VecDeque<f32>,
    beat_refractory_remaining: f64,
    seq: u64,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        let mut hann = [0.0f32; FFT_SIZE];
        for (n, w) in hann.iter_mut().enumerate() {
            *w =
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (FFT_SIZE as f32 - 1.0)).cos();
        }

        Self {
            sample_rate,
            fft,
            hann,
            band_map: BandMap::new(sample_rate, FFT_SIZE),
            pending: VecDeque::with_capacity(FFT_SIZE * 2),
            bands_smoothed: [0.0; BANDS],
            level_smoothed: 0.0,
            bass_history: VecDeque::with_capacity(BASS_HISTORY),
            beat_refractory_remaining: 0.0,
            seq: 0,
        }
    }

    /// Feed mono samples; returns one frame per full `FFT_SIZE`-sample
    /// window consumed at a 50% hop (`HOP` samples).
    pub fn feed(&mut self, samples: &[f32]) -> Vec<SpectrumFrame> {
        self.pending.extend(samples.iter().copied());

        let mut frames = Vec::new();
        while self.pending.len() >= FFT_SIZE {
            let window: Vec<f32> = self.pending.iter().take(FFT_SIZE).copied().collect();
            frames.push(self.analyze_window(&window));
            for _ in 0..HOP.min(self.pending.len()) {
                self.pending.pop_front();
            }
        }
        frames
    }

    fn analyze_window(&mut self, window: &[f32]) -> SpectrumFrame {
        let dt = HOP as f32 / self.sample_rate as f32;
        let release = (-dt / RELEASE_TAU_SECS).exp();

        // 1. Hann window + FFT + magnitude normalization.
        let mut buf: Vec<Complex32> = window
            .iter()
            .zip(self.hann.iter())
            .map(|(s, w)| Complex32::new(s * w, 0.0))
            .collect();
        self.fft.process(&mut buf);

        // Hann coherent gain is 0.5, so normalize by N/4 (N/2 * 0.5).
        let norm = FFT_SIZE as f32 / 4.0;
        let max_bin = FFT_SIZE / 2;
        let magnitudes: Vec<f32> = buf[1..=max_bin].iter().map(|c| c.norm() / norm).collect();

        // 2. Band folding + perceptual curve.
        let folded = self.band_map.fold(&magnitudes);
        let mut bands_raw = [0.0f32; BANDS];
        for (o, v) in bands_raw.iter_mut().zip(folded.iter()) {
            *o = v.clamp(0.0, 1.0).powf(0.5);
        }

        // 3. Per-band smoothing with fast-attack, exponential release
        for (s, v) in self.bands_smoothed.iter_mut().zip(bands_raw.iter()) {
            *s = v.max(*s * release);
        }

        let rms = (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt();
        let level_raw = rms.clamp(0.0, 1.0).powf(0.5);
        self.level_smoothed = if level_raw > self.level_smoothed {
            level_raw
        } else {
            self.level_smoothed * release
        };

        // 4. Flux / beat detection.
        let bass = bands_raw[..BASS_BANDS].iter().sum::<f32>() / BASS_BANDS as f32;
        let avg = if self.bass_history.is_empty() {
            bass
        } else {
            self.bass_history.iter().sum::<f32>() / self.bass_history.len() as f32
        };
        let flux = ((bass - avg) / (avg + 1e-4)).clamp(0.0, 1.0);

        if self.bass_history.len() >= BASS_HISTORY {
            self.bass_history.pop_front();
        }
        self.bass_history.push_back(bass);

        self.beat_refractory_remaining = (self.beat_refractory_remaining - dt as f64).max(0.0);
        let mut beat = false;
        if flux > BEAT_THRESHOLD && self.beat_refractory_remaining <= 0.0 {
            beat = true;
            self.beat_refractory_remaining = BEAT_REFRACTORY_SECS;
        }

        // 5. Waveform downsample.
        let mut waveform = [0.0f32; WAVE_SAMPLES];
        let stride = window.len() as f32 / WAVE_SAMPLES as f32;
        for (i, w) in waveform.iter_mut().enumerate() {
            let idx = ((i as f32 * stride) as usize).min(window.len() - 1);
            *w = window[idx].clamp(-1.0, 1.0);
        }

        // 6. Sequence.
        self.seq += 1;

        SpectrumFrame {
            bands: self.bands_smoothed,
            level: self.level_smoothed,
            flux,
            beat,
            waveform,
            seq: self.seq,
        }
    }
}

/// Decodes a raw little-endian `f32` byte buffer into samples, truncating any
/// trailing partial sample (the capture-owned buffer may not be 4-byte aligned).
pub(super) fn le_f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Downmixes an interleaved `channels`-channel buffer to mono by averaging each
/// frame. `channels <= 1` returns the input unchanged (no allocation).
pub(super) fn downmix_to_mono(samples: Vec<f32>, channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples;
    }
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn silence(n: usize) -> Vec<f32> {
        vec![0.0; n]
    }

    #[test]
    fn le_f32_samples_decodes_and_truncates_partial() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
        bytes.extend_from_slice(&[0x00, 0x01]); // trailing partial sample
        assert_eq!(le_f32_samples(&bytes), vec![1.0, -0.5]);
    }

    #[test]
    fn downmix_averages_frames_and_passes_mono_through() {
        // 2-channel interleaved: (1,0), (0,1) -> means 0.5, 0.5
        assert_eq!(downmix_to_mono(vec![1.0, 0.0, 0.0, 1.0], 2), vec![0.5, 0.5]);
        // mono and zero-channel inputs are returned unchanged
        assert_eq!(downmix_to_mono(vec![0.3, 0.7], 1), vec![0.3, 0.7]);
        assert_eq!(downmix_to_mono(vec![0.3, 0.7], 0), vec![0.3, 0.7]);
    }

    fn sine(freq: f32, sample_rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin())
            .collect()
    }

    #[test]
    fn silence_produces_all_zero_bands_and_no_beat() {
        let mut analyzer = SpectrumAnalyzer::new(44100);
        let frames = analyzer.feed(&silence(FFT_SIZE * 4));
        assert!(!frames.is_empty());
        for f in &frames {
            assert!(f.bands.iter().all(|&b| b == 0.0), "bands not silent");
            assert_eq!(f.level, 0.0);
            assert!(!f.beat);
        }
    }

    #[test]
    fn bands_decay_to_silence_after_a_loud_burst() {
        let sample_rate = 44100;
        let mut analyzer = SpectrumAnalyzer::new(sample_rate);
        let frames = analyzer.feed(&sine(440.0, sample_rate, FFT_SIZE * 4));
        let peak = frames
            .last()
            .unwrap()
            .bands
            .iter()
            .cloned()
            .fold(0.0f32, f32::max);
        assert!(peak > 0.1, "burst did not register: {peak}");

        // 1 s of silence ≫ the 120 ms release tau: every band must have
        // decayed to near-zero, and each frame's max must be non-increasing.
        let frames = analyzer.feed(&silence(sample_rate as usize));
        let mut prev = f32::INFINITY;
        for f in &frames {
            let m = f.bands.iter().cloned().fold(0.0f32, f32::max);
            assert!(m <= prev + 1e-6, "bands rose during silence");
            prev = m;
        }
        assert!(prev < 0.01, "bands froze instead of decaying: {prev}");
    }

    #[test]
    fn sine_440_peaks_in_the_band_containing_440hz() {
        let sample_rate = 44100;
        let mut analyzer = SpectrumAnalyzer::new(sample_rate);
        let samples = sine(440.0, sample_rate, FFT_SIZE * 6);
        let frames = analyzer.feed(&samples);
        let last = frames.last().expect("frame emitted");

        let band_map = BandMap::new(sample_rate, FFT_SIZE);
        let bin_hz = sample_rate as f64 / FFT_SIZE as f64;
        let target_bin = (440.0 / bin_hz).round() as usize;
        let expected_band = band_map
            .bins
            .iter()
            .position(|&(lo, hi)| target_bin >= lo && target_bin <= hi)
            .expect("440 Hz maps to a band");

        let (argmax, _) = last
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(argmax, expected_band);
    }

    #[test]
    fn bass_burst_after_silence_triggers_exactly_one_beat_in_refractory_window() {
        let sample_rate = 44100;
        let mut analyzer = SpectrumAnalyzer::new(sample_rate);
        // Warm up bass history with silence.
        analyzer.feed(&silence(FFT_SIZE + HOP * BASS_HISTORY));

        // Loud 60 Hz burst.
        let burst = sine(60.0, sample_rate, FFT_SIZE * 8)
            .iter()
            .map(|s| s * 4.0)
            .collect::<Vec<_>>();
        let frames = analyzer.feed(&burst);

        let beats: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.beat)
            .map(|(i, _)| i)
            .collect();
        assert!(!beats.is_empty(), "expected at least one beat");

        // No two beats closer than the refractory period (in hops).
        let dt = HOP as f64 / sample_rate as f64;
        let min_gap_hops = (BEAT_REFRACTORY_SECS / dt).floor() as usize;
        for pair in beats.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= min_gap_hops,
                "beats too close: {gap} hops apart (min {min_gap_hops})"
            );
        }
    }

    proptest! {
        #[test]
        fn frame_outputs_stay_in_declared_ranges(
            samples in proptest::collection::vec(-1.0f32..=1.0, FFT_SIZE..FFT_SIZE * 3)
        ) {
            let mut analyzer = SpectrumAnalyzer::new(44100);
            let frames = analyzer.feed(&samples);
            for f in &frames {
                for b in f.bands.iter() {
                    prop_assert!(b.is_finite() && *b >= 0.0 && *b <= 1.0);
                }
                prop_assert!(f.level.is_finite() && f.level >= 0.0 && f.level <= 1.0);
                prop_assert!(f.flux.is_finite() && f.flux >= 0.0 && f.flux <= 1.0);
                for w in f.waveform.iter() {
                    prop_assert!(w.is_finite() && *w >= -1.0 && *w <= 1.0);
                }
            }
        }

        #[test]
        fn seq_strictly_increases_across_emitted_frames(
            samples in proptest::collection::vec(-1.0f32..=1.0, FFT_SIZE..FFT_SIZE * 4)
        ) {
            let mut analyzer = SpectrumAnalyzer::new(44100);
            let frames = analyzer.feed(&samples);
            for pair in frames.windows(2) {
                prop_assert!(pair[1].seq > pair[0].seq);
            }
        }

        #[test]
        fn constant_input_bands_are_non_increasing_after_first_frame(
            value in 0.01f32..1.0,
        ) {
            let samples = vec![value; FFT_SIZE * 5];
            let mut analyzer = SpectrumAnalyzer::new(44100);
            let frames = analyzer.feed(&samples);
            prop_assert!(frames.len() >= 2);
            for pair in frames[1..].windows(2) {
                for (a, b) in pair[0].bands.iter().zip(pair[1].bands.iter()) {
                    // Release decays; a constant DC-ish input can't push the
                    // smoothed value up again once past the first attack.
                    prop_assert!(*b <= *a + 1e-6, "{b} > {a}");
                }
            }
        }
    }
}

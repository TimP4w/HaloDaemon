// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
#[cfg(test)]
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::{dsp, dsp::SpectrumAnalyzer, AudioHandle, StopGuard, SESSION_RETRY_MS};

struct CoTaskMem<T>(*mut T);

impl<T> Drop for CoTaskMem<T> {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) }
    }
}

/// Spawns the WASAPI loopback capture thread. Returns immediately. The
/// thread runs capture sessions until no consumer has read for the idle
/// window, retrying failed sessions (rate-limited) while readers remain.
pub fn start(handle: Arc<AudioHandle>) {
    std::thread::spawn(move || run_capture(handle));
}

fn run_capture(handle: Arc<AudioHandle>) {
    let _guard = StopGuard(Arc::clone(&handle));
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            log::error!("Audio capture: CoInitializeEx: {e}");
            return;
        }
    }
    log::info!("Audio capture: WASAPI thread started");

    loop {
        if handle.idle_expired(super::monotonic_ms()) {
            return;
        }
        if let Err(e) = capture_session(&handle) {
            log::warn!("Audio capture: session ended ({e}), retrying");
        }
        std::thread::sleep(std::time::Duration::from_millis(SESSION_RETRY_MS));
    }
}

/// Interprets a raw capture buffer as samples given the negotiated format,
/// converting 16-bit PCM to f32 in [-1, 1] when needed.
fn buffer_to_f32(bytes: &[u8], format: &WAVEFORMATEX) -> Vec<f32> {
    let is_float = format.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT
        || (format.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE
            && unsafe {
                let extensible = format as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE;
                std::ptr::addr_of!((*extensible).SubFormat).read_unaligned()
                    == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            });
    if is_float {
        dsp::le_f32_samples(bytes)
    } else {
        match format.wBitsPerSample {
            16 => bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect(),
            24 => bytes
                .chunks_exact(3)
                .map(|c| {
                    let sample = i32::from_le_bytes([c[0], c[1], c[2], 0]) << 8 >> 8;
                    sample as f32 / 8_388_608.0
                })
                .collect(),
            32 => bytes
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes(c.try_into().unwrap()) as f32 / 2_147_483_648.0)
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn capture_session(handle: &Arc<AudioHandle>) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        let mix_format = CoTaskMem(audio_client.GetMixFormat()?);
        let format = *mix_format.0;
        let channels = format.nChannels as usize;

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            0, // buffer duration: 0 lets WASAPI pick a default for polling mode
            0,
            mix_format.0,
            None,
        )?;

        let capture_client: IAudioCaptureClient = audio_client.GetService()?;
        audio_client.Start()?;

        let mut analyzer = SpectrumAnalyzer::new(format.nSamplesPerSec);

        // WAVEFORMATEX is packed; copy fields to locals before formatting.
        let (tag, bits, rate, block) = (
            format.wFormatTag,
            format.wBitsPerSample,
            format.nSamplesPerSec,
            format.nBlockAlign,
        );
        log::info!(
            "Audio capture: session started, fmt tag={tag:#06x}, {channels} ch, {bits} bit, {rate} Hz, block {block} B",
        );

        // DIAG: per-second capture stats to triage "no output". Interpretation:
        //   "0 frames/s"          -> no packets arriving (routing/endpoint/loopback)
        //   "N frames/s peak 0.0" -> packets but silent (SILENT flag / decode)
        //   "N frames/s peak >0"  -> capture works; problem is downstream
        let mut stat_frames: u64 = 0;
        let mut stat_peak: f32 = 0.0;
        let mut stat_last_ms = super::monotonic_ms();

        let result = (|| -> windows::core::Result<()> {
            loop {
                let now = super::monotonic_ms();
                if handle.idle_expired(now) {
                    log::info!("Audio capture: idle window elapsed, ending session");
                    return Ok(());
                }
                if now.saturating_sub(stat_last_ms) >= 1000 {
                    log::trace!("Audio capture: {stat_frames} frames/s, peak {stat_peak:.4}");
                    stat_frames = 0;
                    stat_peak = 0.0;
                    stat_last_ms = now;
                }

                std::thread::sleep(std::time::Duration::from_millis(10));

                loop {
                    let packet_len = capture_client.GetNextPacketSize()?;
                    if packet_len == 0 {
                        break;
                    }

                    let mut data_ptr = std::ptr::null_mut();
                    let mut num_frames = 0u32;
                    let mut flags = 0u32;
                    capture_client.GetBuffer(
                        &mut data_ptr,
                        &mut num_frames,
                        &mut flags,
                        None,
                        None,
                    )?;

                    let byte_len = num_frames as usize * format.nBlockAlign as usize;
                    let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;

                    let samples = if silent || data_ptr.is_null() {
                        vec![0.0f32; num_frames as usize * channels]
                    } else {
                        let bytes = std::slice::from_raw_parts(data_ptr, byte_len);
                        buffer_to_f32(bytes, &format)
                    };

                    capture_client.ReleaseBuffer(num_frames)?;

                    let mono = dsp::downmix_to_mono(samples, channels);
                    stat_frames += mono.len() as u64;
                    for &s in &mono {
                        let a = s.abs();
                        if a > stat_peak {
                            stat_peak = a;
                        }
                    }
                    for frame in analyzer.feed(&mono) {
                        handle.publish(frame);
                    }
                }
            }
        })();

        let _ = audio_client.Stop();

        match result {
            Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod format_regression_tests {
    use super::*;

    #[test]
    fn extensible_pcm_is_not_misclassified_as_float() {
        let mut format = WAVEFORMATEXTENSIBLE::default();
        format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
        format.Format.wBitsPerSample = 32;
        format.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
        let samples = buffer_to_f32(&i32::MAX.to_le_bytes(), &format.Format);
        assert!(samples[0] > 0.99);
    }

    #[test]
    fn extensible_float_uses_its_subformat() {
        let mut format = WAVEFORMATEXTENSIBLE::default();
        format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
        format.Format.wBitsPerSample = 32;
        format.SubFormat = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        assert_eq!(
            buffer_to_f32(&0.5f32.to_le_bytes(), &format.Format),
            vec![0.5]
        );
    }

    #[test]
    fn packed_24_bit_pcm_is_sign_extended() {
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            wBitsPerSample: 24,
            ..Default::default()
        };
        let samples = buffer_to_f32(&[0, 0, 0x80], &format);
        assert_eq!(samples, vec![-1.0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(tag: u32, bits: u16) -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: tag as u16,
            wBitsPerSample: bits,
            ..Default::default()
        }
    }

    #[test]
    fn buffer_to_f32_decodes_ieee_float() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.25f32.to_le_bytes());
        bytes.extend_from_slice(&(-1.0f32).to_le_bytes());
        assert_eq!(
            buffer_to_f32(&bytes, &fmt(WAVE_FORMAT_IEEE_FLOAT, 32)),
            vec![0.25, -1.0]
        );
    }

    #[test]
    fn buffer_to_f32_decodes_16bit_pcm() {
        // 0x4000 = 16384, 0x8000 = -32768 (little-endian)
        let bytes = [0x00, 0x40, 0x00, 0x80];
        let out = buffer_to_f32(&bytes, &fmt(1, 16));
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert_eq!(out[1], -1.0);
    }
}

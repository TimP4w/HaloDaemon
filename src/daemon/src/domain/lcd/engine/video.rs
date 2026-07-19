// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-device looped video playback: an `ffmpeg` subprocess decodes a local
//! file into RGBA frames handed to the device's `stream_frame`. Cross-platform
//! — on Windows a bundled `ffmpeg.exe` beside the daemon is preferred, else
//! `ffmpeg` is resolved from `PATH` (the Linux path; see `ffmpeg_program`).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::FrameTx;
use crate::application::state::AppState;
use crate::domain::device::Device;
use halod_shared::types::LcdHealth;

fn set_device_health(device: &dyn Device, health: LcdHealth) {
    if let Some(lcd) = device.as_lcd() {
        lcd.lcd_state().set_health(health);
    }
}

/// Panel streaming is bandwidth-bound, so capping the decode avoids queueing
/// frames faster than they can be delivered.
const MAX_FPS: u32 = 30;

/// Emit a UI preview every N device frames.
const PREVIEW_EVERY: u64 = 2;

/// The ffmpeg binary to invoke. Prefers a copy bundled next to the daemon
/// executable (the Windows installer ships `ffmpeg.exe` there); otherwise falls
/// back to `ffmpeg` resolved from `PATH`.
fn ffmpeg_program() -> std::path::PathBuf {
    #[cfg(windows)]
    if let Some(bundled) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("ffmpeg.exe")))
        .filter(|p| p.is_file())
    {
        return bundled;
    }
    std::path::PathBuf::from("ffmpeg")
}

/// Whether `ffmpeg` is available (cached after the first probe). Used to gate
/// video mode in the UI and to fail fast before spawning a stream.
pub fn ffmpeg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new(ffmpeg_program())
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

enum VideoStream {
    Stopped,
    Starting,
    Playing {
        task: JoinHandle<()>,
        child: Arc<Mutex<tokio::process::Child>>,
    },
    Stopping,
    Failed,
}

pub struct VideoEngine {
    app: Arc<AppState>,
    streams: Mutex<HashMap<String, VideoStream>>,
    lifecycle: Mutex<()>,
    /// Sender shared with the LCD engine so video preview frames appear on the
    /// same `lcd_engine_frame` IPC channel the UI already subscribes to.
    preview_tx: FrameTx,
}

impl VideoEngine {
    pub fn new(app: Arc<AppState>, preview_tx: FrameTx) -> Arc<Self> {
        Arc::new(Self {
            app,
            streams: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            preview_tx,
        })
    }

    /// Start (or restart) playback after fully stopping any prior stream.
    pub async fn start(self: &Arc<Self>, device_id: &str, path: &str) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let device = self
            .app
            .find_device_by_id(device_id)
            .await
            .ok_or_else(|| anyhow!("device {device_id} not found"))?;
        let (w, h) = {
            let lcd = device
                .as_lcd()
                .ok_or_else(|| anyhow!("device does not support LCD"))?;
            let d = lcd.lcd_descriptor();
            (d.width, d.height)
        };

        if !std::path::Path::new(path).is_file() {
            let error = anyhow!("video file not found: {path}");
            set_device_health(device.as_ref(), LcdHealth::Failed(error.to_string()));
            return Err(error);
        }

        if !ffmpeg_available() {
            let error = anyhow!("ffmpeg is not installed or not on PATH");
            set_device_health(device.as_ref(), LcdHealth::Failed(error.to_string()));
            return Err(error);
        }

        self.stop_inner(device_id).await;
        set_device_health(device.as_ref(), LcdHealth::Starting);
        self.streams
            .lock()
            .await
            .insert(device_id.to_string(), VideoStream::Starting);

        let (task, child) = match self.spawn_player(
            Arc::clone(&device),
            path.to_string(),
            w,
            h,
            device_id.to_string(),
        ) {
            Ok(stream) => stream,
            Err(error) => {
                set_device_health(device.as_ref(), LcdHealth::Failed(error.to_string()));
                self.streams
                    .lock()
                    .await
                    .insert(device_id.to_string(), VideoStream::Failed);
                return Err(error);
            }
        };
        self.streams
            .lock()
            .await
            .insert(device_id.to_string(), VideoStream::Playing { task, child });
        set_device_health(device.as_ref(), LcdHealth::Stable);
        log::info!("[Video] streaming {path} to {device_id} ({w}x{h})");
        Ok(())
    }

    /// Stopping kills ffmpeg and awaits its exit, then joins the reader task.
    pub async fn stop(self: &Arc<Self>, device_id: &str) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_inner(device_id).await;
    }

    pub async fn stop_all(self: &Arc<Self>) {
        let ids: Vec<_> = self.streams.lock().await.keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    async fn stop_inner(self: &Arc<Self>, device_id: &str) {
        let playing = {
            let mut streams = self.streams.lock().await;
            match streams.get(device_id) {
                Some(VideoStream::Playing { .. }) => {
                    match streams.insert(device_id.to_string(), VideoStream::Stopping) {
                        Some(VideoStream::Playing { task, child }) => Some((task, child)),
                        _ => None,
                    }
                }
                _ => None,
            }
        };
        let Some((mut task, child)) = playing else {
            return;
        };
        let device = self.app.find_device_by_id(device_id).await;
        if let Some(device) = &device {
            set_device_health(device.as_ref(), LcdHealth::Stopping);
        }
        {
            let mut child = child.lock().await;
            let _ = child.kill().await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await {
            Ok(_) => {}
            Err(_) => {
                log::warn!("[Video] reader task for {device_id} did not stop in time");
                task.abort();
                let _ = task.await;
            }
        }
        self.streams
            .lock()
            .await
            .insert(device_id.to_string(), VideoStream::Stopped);
        if let Some(device) = device {
            set_device_health(device.as_ref(), LcdHealth::Stable);
        }
        log::info!("[Video] stopped stream for {device_id}");
    }

    fn spawn_player(
        self: &Arc<Self>,
        device: Arc<dyn Device>,
        path: String,
        w: u32,
        h: u32,
        device_id: String,
    ) -> Result<(JoinHandle<()>, Arc<Mutex<tokio::process::Child>>)> {
        let frame_bytes = (w as usize) * (h as usize) * 4;
        // Scale to cover, then centre-crop to the exact panel square.
        let vf = format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}",);
        let mut child = Command::new(ffmpeg_program())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-stream_loop",
                "-1",
                "-i",
                &path,
                "-vf",
                &vf,
                "-r",
                &MAX_FPS.to_string(),
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("failed to launch ffmpeg (is it installed?)")?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg stdout unavailable"))?;
        let mut stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));

        let preview_tx = self.preview_tx.clone();
        let engine = self.clone();
        let child_guard = child.clone();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; frame_bytes];
            let mut frame_id: u64 = 0;
            // One-permit gate: at most one preview encode in flight, so a slow
            // encoder degrades preview FPS instead of queueing frame clones.
            let encode_gate = Arc::new(tokio::sync::Semaphore::new(1));
            let failure = loop {
                if let Err(e) = stdout.read_exact(&mut buf).await {
                    // No full frame arrived — surface ffmpeg's own error (bad
                    // file, unsupported codec, …) instead of a bare EOF.
                    {
                        let mut child = child_guard.lock().await;
                        if child.try_wait().ok().flatten().is_none() {
                            let _ = child.start_kill();
                        }
                    }
                    let mut err = String::new();
                    if let Some(se) = stderr.as_mut() {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            se.read_to_string(&mut err),
                        )
                        .await;
                    }
                    break match err.trim() {
                        "" => format!("stream ended ({e})"),
                        msg => format!("ffmpeg failed: {msg}"),
                    };
                }
                let Some(lcd) = device.as_lcd() else {
                    break "device lost LCD capability".to_string();
                };
                if let Err(e) = lcd.stream_frame(&buf, w, h).await {
                    break format!("frame push failed: {e}");
                }
                frame_id += 1;
                if frame_id.is_multiple_of(PREVIEW_EVERY) && preview_tx.receiver_count() > 0 {
                    if let Some(permit) = try_begin_preview(&encode_gate) {
                        spawn_preview_encode(
                            permit,
                            buf.clone(),
                            w,
                            h,
                            device_id.clone(),
                            frame_id,
                            &preview_tx,
                        );
                    }
                }
            };
            log::warn!("[Video] {device_id}: {failure}");
            {
                let mut child = child_guard.lock().await;
                let _ = child.kill().await;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
            }
            // Only overwrite state we own — a concurrent stop()/start() may have already replaced this entry.
            let changed = {
                let mut streams = engine.streams.lock().await;
                if matches!(streams.get(&device_id), Some(VideoStream::Playing { .. })) {
                    set_device_health(device.as_ref(), LcdHealth::Failed(failure.clone()));
                    streams.insert(device_id.clone(), VideoStream::Failed);
                    true
                } else {
                    false
                }
            };
            if changed {
                crate::application::usecases::lcd::runtime::device_changed(&engine.app, &device_id)
                    .await;
            }
        });
        Ok((task, child))
    }
}

/// Claim the single preview-encode slot; `None` while an encode is in flight,
/// in which case the caller skips this preview (the UI keeps the last frame).
fn try_begin_preview(
    gate: &Arc<tokio::sync::Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    Arc::clone(gate).try_acquire_owned().ok()
}

/// Encode and broadcast one preview frame off the async task, so the
/// CPU-bound PNG work never stalls the device push loop. The permit is
/// released when the encode finishes, reopening `try_begin_preview`'s gate.
fn spawn_preview_encode(
    permit: tokio::sync::OwnedSemaphorePermit,
    buf: Vec<u8>,
    w: u32,
    h: u32,
    device_id: String,
    frame_id: u64,
    preview_tx: &super::FrameTx,
) {
    let tx = preview_tx.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Some(preview_b64) = encode_preview(buf, w, h) {
            if let Some(frame) = super::encode_wire_frame(&device_id, frame_id, &preview_b64) {
                let _ = tx.send(frame);
            }
        }
    });
}

/// PNG-encode + base64 a raw RGBA buffer. Returns `None` on encode failure
/// (the UI just keeps the last frame).
fn encode_preview(rgba: Vec<u8>, w: u32, h: u32) -> Option<String> {
    use base64::Engine as _;
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_raw(w, h, rgba)?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(&png))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::state::AppState, config::Config, domain::device::Device,
        test_support::MockDevice,
    };

    fn make_engine(app: Arc<AppState>) -> Arc<VideoEngine> {
        let (tx, _) = tokio::sync::broadcast::channel(2);
        VideoEngine::new(app, tx)
    }

    #[test]
    fn encode_preview_returns_none_for_undersized_buffer() {
        // A buffer too small for w*h*4 RGBA bytes is not decodable; the UI
        // must keep its last frame rather than the loop erroring out.
        let too_small = vec![0u8; 4];
        assert!(encode_preview(too_small, 4, 4).is_none());
    }

    #[test]
    fn encode_preview_succeeds_for_valid_buffer() {
        let buf = vec![0u8; 4 * 4 * 4];
        assert!(encode_preview(buf, 4, 4).is_some());
    }

    #[test]
    fn try_begin_preview_gates_to_one_in_flight() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = try_begin_preview(&gate).expect("first acquire must succeed");
        assert!(
            try_begin_preview(&gate).is_none(),
            "second acquire must be gated while an encode is in flight"
        );
        drop(permit);
        assert!(
            try_begin_preview(&gate).is_some(),
            "releasing the permit must reopen the gate"
        );
    }

    #[tokio::test]
    async fn start_returns_err_when_device_not_found() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = make_engine(Arc::clone(&app));
        let result = engine.start("no_such_device", "/tmp/test.mp4").await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error must mention the missing device"
        );
    }

    #[tokio::test]
    async fn start_returns_err_when_path_does_not_exist() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd1").with_lcd());
        app.device_registry
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = make_engine(Arc::clone(&app));
        let result = engine
            .start("lcd1", "/tmp/halod_test_nonexistent_file.mp4")
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found") || msg.contains("ffmpeg"),
            "error must be about missing file or ffmpeg: {msg}"
        );
    }

    #[tokio::test]
    async fn stop_on_unknown_device_does_not_panic() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = make_engine(app);
        // Must be a no-op — no stream registered for this id.
        engine.stop("no_such_device").await;
    }

    /// A tiny (2x2, 5-frame) local video file, built with the same `ffmpeg` the engine under test invokes.
    fn tiny_test_video(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("tiny.mp4");
        let status = std::process::Command::new(ffmpeg_program())
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=2x2:d=1",
                "-frames:v",
                "5",
                path.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("failed to run ffmpeg to build the test fixture");
        assert!(status.success(), "ffmpeg fixture generation failed");
        path
    }

    fn lcd_app_with_device(id: &str) -> (Arc<AppState>, Arc<MockDevice>) {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new(id).with_lcd().with_lcd_stream_ok());
        (app, dev)
    }

    fn lcd_health(device: &MockDevice) -> LcdHealth {
        device
            .as_lcd()
            .expect("LCD capability")
            .current_state()
            .health
    }

    #[tokio::test]
    async fn stop_awaits_the_child_exiting_before_returning() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let video = tiny_test_video(dir.path());
        let (app, dev) = lcd_app_with_device("lcd1");
        app.device_registry
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = make_engine(Arc::clone(&app));
        engine.start("lcd1", video.to_str().unwrap()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        engine.stop("lcd1").await;

        assert_eq!(
            lcd_health(&dev),
            LcdHealth::Stable,
            "stop() must await full teardown, leaving a clean Stable state"
        );
    }

    #[tokio::test]
    async fn restarting_while_playing_replaces_the_stream_cleanly() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let video = tiny_test_video(dir.path());
        let (app, dev) = lcd_app_with_device("lcd1");
        app.device_registry
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = make_engine(Arc::clone(&app));
        engine.start("lcd1", video.to_str().unwrap()).await.unwrap();
        engine.start("lcd1", video.to_str().unwrap()).await.unwrap();

        engine.stop("lcd1").await;
        assert_eq!(lcd_health(&dev), LcdHealth::Stable);
    }

    #[tokio::test]
    async fn a_stream_that_fails_after_spawning_transitions_to_failed() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("not_a_video.mp4");
        std::fs::write(&garbage, b"this is not a video file").unwrap();
        let (app, dev) = lcd_app_with_device("lcd1");
        app.device_registry
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = make_engine(Arc::clone(&app));
        engine
            .start("lcd1", garbage.to_str().unwrap())
            .await
            .unwrap();

        let mut health = lcd_health(&dev);
        for _ in 0..50 {
            if matches!(health, halod_shared::types::LcdHealth::Failed(_)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            health = lcd_health(&dev);
        }
        assert!(
            matches!(health, halod_shared::types::LcdHealth::Failed(_)),
            "expected Failed, got {health:?}"
        );
    }
}

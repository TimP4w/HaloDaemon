// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{anyhow, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::ipc::ClientHandle;
use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::state::AppState;
use crate::util::image::compress_for_storage;
use halod_shared::types::{validate_image_filename, LcdUploadProgress, LcdUploadStage, RgbState};

/// Clear a live editor session's decoded-image cache after the image library
/// changes underneath it (upload/delete), so a stale decode isn't served
/// until an unrelated prune happens to evict it.
fn invalidate_editor_image_cache(app: &Arc<AppState>) {
    if let Some(session) = app.lcd.editor_session.lock().unwrap().as_ref() {
        session.invalidate_image_cache();
    }
}

/// Stop any running video stream and clear its path. Called on every transition
/// to another mode so a stale video subprocess can't keep driving the panel.
/// Also asks glibc to return freed pages to the OS, since stopping a video
/// stream drops its ~230 KB+ frame buffer (the allocator retains those pages).
async fn stop_video_for_device(
    app: &Arc<AppState>,
    lcd: &dyn crate::drivers::LcdCapability,
    device_id: &str,
) {
    if let Some(video) = app.lcd.video() {
        video.stop(device_id).await;
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
    }
    if lcd.video_path().is_some() {
        lcd.set_video_path(None);
    }
}

/// If the device is currently driven by the LCD engine, take it out of engine
/// mode so the engine stops overwriting the image/brightness we're about to set.
/// Clears the slot's template (otherwise the next engine tick re-derives and
/// re-adds the device) and removes it from the running engine. The cleared
/// template is persisted by the caller's `persist_device_state`.
async fn deactivate_engine_for_device(
    app: &Arc<AppState>,
    lcd: &dyn crate::drivers::LcdCapability,
    device_id: &str,
) {
    // Drop the editor session immediately (engine-driven or not) — its
    // CustomTemplate holds decoded image caches (up to 64 MB per image) which
    // would otherwise persist for the 30-second idle timeout, causing apparent
    // memory growth when switching between Editor and other modes.
    if let Ok(mut session) = app.lcd.editor_session.try_lock() {
        if session.as_ref().is_some_and(|s| s.device_id == device_id) {
            *session = None;
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }
    if lcd.lcd_template_id().is_none() {
        return; // not engine-driven; nothing to deactivate
    }
    lcd.set_lcd_template_id(None);
    lcd.set_lcd_template_params(Default::default());
    if let Some(lcd_engine) = app.lcd.engine() {
        lcd_engine.remove_device(device_id).await;
    }
}

/// Re-apply the device's saved RGB state after an LCD image upload, for devices
/// whose panel upload resets the LEDs (see `needs_rgb_restore_after_upload`).
/// Skipped when the current state is Engine (controlled externally) or not yet set.
async fn restore_rgb(device: &dyn crate::drivers::Device, lcd: &dyn crate::drivers::LcdCapability) {
    if !lcd.needs_rgb_restore_after_upload() {
        return;
    }
    if let Some(rgb) = device.as_rgb() {
        if let Some(state) = rgb.current_state() {
            if !matches!(state, RgbState::Engine) {
                if let Err(e) = rgb.apply(state).await {
                    log::warn!("[LCD] RGB restore failed after image upload: {e}");
                }
            }
        }
    }
}

fn send_upload_progress(
    client: &ClientHandle,
    device_id: &str,
    stage: LcdUploadStage,
    percent: Option<u8>,
) {
    client.send_json(&json!({
        "type": "lcd_upload_progress",
        "data": LcdUploadProgress {
            device_id: device_id.to_string(),
            stage,
            percent,
        },
    }));
}

/// A callback for `compress_for_storage` that pushes Processing progress to
/// the uploading client, deduplicated per percent step. Safe to call from the
/// blocking compression thread (`send_json` is a non-blocking `try_send`).
fn upload_progress_reporter(client: &ClientHandle, device_id: &str) -> impl FnMut(u8) {
    let client = client.clone();
    let device_id = device_id.to_string();
    let mut last = None;
    move |percent| {
        if last != Some(percent) {
            last = Some(percent);
            send_upload_progress(
                &client,
                &device_id,
                LcdUploadStage::Processing,
                Some(percent),
            );
        }
    }
}

/// Upload a new image (binary data arrives as base64 in `data_b64`).
/// Saves the file to lcd_images_dir and applies it to the device.
pub async fn set_screen_image(
    id: String,
    data_b64: String,
    request_id: Option<String>,
    app: Arc<AppState>,
    client: ClientHandle,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;

    let data = base64::engine::general_purpose::STANDARD.decode(&data_b64)?;
    log::debug!(
        "[LCD] set_screen_image: received {} bytes for device {}",
        data.len(),
        device.id()
    );

    let desc = lcd.lcd_descriptor();
    let (width, height) = (desc.width, desc.height);

    // Compression is CPU-intensive (Lanczos3 resize). Run it off the async thread
    // so the IPC loop stays responsive during processing.
    let progress = upload_progress_reporter(&client, device.id());
    let (compressed, ext) =
        tokio::task::spawn_blocking(move || compress_for_storage(&data, width, height, progress))
            .await??;
    log::debug!("[LCD] compressed to {} bytes ({})", compressed.len(), ext);
    send_upload_progress(&client, device.id(), LcdUploadStage::Applying, None);

    let filename = format!("{}.{}", Uuid::new_v4(), ext);
    let dir = crate::config::lcd_images_dir();
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join(&filename), &compressed).await?;
    log::info!("[LCD] saved image as {}", filename);
    invalidate_editor_image_cache(&app);

    stop_video_for_device(&app, lcd, device.id()).await;
    deactivate_engine_for_device(&app, lcd, device.id()).await;
    lcd.set_image(&compressed).await?;
    restore_rgb(device.as_ref(), lcd).await;
    lcd.set_active_image_filename(Some(filename)).await;
    persist_device_state(&app, device.as_ref()).await;

    client.send_json(
        &json!({ "type": "image_uploaded", "request_id": request_id.unwrap_or_default() }),
    );

    // The spawn_blocking decode + upload just freed 1-3 copies of the RGBA
    // frame buffer; ask glibc to return those pages to the OS.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    Ok(())
}

/// Apply an image already present in the library by filename.
pub async fn set_screen_image_from_library(
    id: String,
    filename: String,
    request_id: Option<String>,
    app: Arc<AppState>,
    client: ClientHandle,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;

    validate_image_filename(&filename).map_err(|e| anyhow!(e))?;

    let path = crate::config::lcd_images_dir().join(&filename);
    let data = tokio::fs::read(&path)
        .await
        .map_err(|e| anyhow!("image not found: {e}"))?;
    log::info!(
        "[LCD] applying library image {} ({} bytes) to device {}",
        filename,
        data.len(),
        device.id()
    );

    stop_video_for_device(&app, lcd, device.id()).await;
    deactivate_engine_for_device(&app, lcd, device.id()).await;
    lcd.set_image(&data).await?;
    restore_rgb(device.as_ref(), lcd).await;
    lcd.set_active_image_filename(Some(filename)).await;
    persist_device_state(&app, device.as_ref()).await;

    client.send_json(
        &json!({ "type": "image_uploaded", "request_id": request_id.unwrap_or_default() }),
    );

    // The read + decode + upload just allocated and freed the file contents
    // plus the RGBA frame buffer; return those pages to the OS.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    Ok(())
}

/// Static modes (Image/Gif) hold a single uploaded frame that must be re-sent to
/// pick up a rotation change; Default/Engine/Video redraw on their own.
fn rotation_needs_image_reapply(mode: halod_shared::types::LcdMode) -> bool {
    use halod_shared::types::LcdMode;
    matches!(mode, LcdMode::Image | LcdMode::Gif)
}

async fn reapply_static_image(
    device: &dyn crate::drivers::Device,
    lcd: &dyn crate::drivers::LcdCapability,
) {
    let state = lcd.current_state();
    if !rotation_needs_image_reapply(state.mode) {
        return;
    }
    let Some(filename) = state.active_image else {
        return;
    };
    let path = crate::config::lcd_images_dir().join(&filename);
    match tokio::fs::read(&path).await {
        Ok(data) => {
            if let Err(e) = lcd.set_image(&data).await {
                log::warn!("[LCD] re-applying {filename} after rotation failed: {e}");
                return;
            }
            restore_rgb(device, lcd).await;
        }
        Err(e) => log::warn!("[LCD] image {filename} missing, can't re-apply after rotation: {e}"),
    }
}

pub async fn set_screen_rotation(
    id: String,
    rotation: halod_shared::types::ScreenRotation,
    app: Arc<AppState>,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    let degrees: u32 = match rotation {
        halod_shared::types::ScreenRotation::R0 => 0,
        halod_shared::types::ScreenRotation::R90 => 90,
        halod_shared::types::ScreenRotation::R180 => 180,
        halod_shared::types::ScreenRotation::R270 => 270,
    };
    log::info!(
        "[LCD] set_screen_rotation: {degrees}° for device {}",
        device.id()
    );
    lcd.set_rotation(degrees).await?;
    reapply_static_image(device.as_ref(), lcd).await;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_screen_brightness(id: String, brightness: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    log::info!(
        "[LCD] set_screen_brightness: {} for device {}",
        brightness,
        device.id()
    );
    lcd.set_brightness(brightness).await?;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_screen_default(id: String, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    log::info!("[LCD] set_screen_default for device {}", device.id());
    stop_video_for_device(&app, lcd, device.id()).await;
    deactivate_engine_for_device(&app, lcd, device.id()).await;
    lcd.reset_to_default().await?;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

/// Toggle the raw (uncompressed, 24-bit) LCD streaming path for a device.
pub async fn set_screen_raw_streaming(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    log::info!("[LCD] set_screen_raw_streaming: {enabled} for device {id}");
    lcd.set_raw_streaming(enabled);
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

/// Resolve `path` to a canonical regular-file path. The daemon runs elevated and
/// hands this straight to ffmpeg, so a client must not be able to point it at a
/// directory, device node, or symlink trap. The canonical path is what we hand
/// downstream so the engine and ffmpeg agree on exactly which inode is read.
fn resolve_video_path(path: &str) -> Result<String> {
    let canonical =
        std::fs::canonicalize(path).map_err(|e| anyhow!("video file not accessible: {e}"))?;
    if !canonical.is_file() {
        anyhow::bail!("video path is not a regular file");
    }
    Ok(canonical.to_string_lossy().to_string())
}

/// Play a looped local video file on the panel via the video engine.
pub async fn set_screen_video(id: String, path: String, app: Arc<AppState>) -> Result<()> {
    use halod_shared::types::LcdMode;

    let device = require_device_owned_id(&id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    log::info!("[LCD] set_screen_video: {path} for device {id}");

    let path_clone = path.clone();
    let canonical = tokio::task::spawn_blocking(move || resolve_video_path(&path_clone)).await??;

    let video = app
        .lcd
        .video()
        .ok_or_else(|| anyhow!("engines not initialized"))?;

    // Start the engine before mutating device state, so a failure here can't
    // leave the device advertising Video mode when nothing is playing.
    deactivate_engine_for_device(&app, lcd, device.id()).await;
    video.start(&id, &canonical).await?;

    lcd.set_active_image_filename(None).await;
    lcd.set_video_path(Some(canonical));
    lcd.lcd_state().set_mode(LcdMode::Video);

    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

/// Return a list of all images in the lcd_images_dir to the requesting client.
pub async fn list_lcd_images(client: ClientHandle) -> Result<()> {
    let dir = crate::config::lcd_images_dir();
    let mut files: Vec<Value> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            files.push(json!({ "name": name, "size_bytes": size }));
        }
    }
    log::debug!("[LCD] list_lcd_images: {} files", files.len());
    client.send_json(&json!({ "type": "lcd_images", "files": files }));
    Ok(())
}

/// Delete a named image from the lcd_images_dir.
pub async fn delete_lcd_image(filename: String, app: Arc<AppState>) -> Result<()> {
    validate_image_filename(&filename).map_err(|e| anyhow!(e))?;
    let path = crate::config::lcd_images_dir().join(&filename);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => log::info!("[LCD] deleted image {}", filename),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    invalidate_editor_image_cache(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_video_path ────────────────────────────────────────────────

    #[test]
    fn resolve_video_path_accepts_regular_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let resolved = resolve_video_path(f.path().to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(f.path()).unwrap()
        );
    }

    #[test]
    fn resolve_video_path_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_video_path(dir.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }

    #[test]
    fn resolve_video_path_rejects_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.mp4");
        let err = resolve_video_path(missing.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    // ── upload progress ───────────────────────────────────────────────────

    /// The reporter must put a decodable `lcd_upload_progress` frame on the
    /// client queue per distinct percent — the GUI's spinner depends on the
    /// exact `type` tag and `data` shape.
    #[tokio::test]
    async fn upload_progress_reporter_emits_decodable_frames_once_per_percent() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::sync::Arc<Vec<u8>>>(16);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        let mut report = upload_progress_reporter(&client, "lcd_dev");
        report(50);
        report(50); // deduplicated
        report(100);

        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 2, "one frame per distinct percent");
        let payload = &frames[0][5..]; // skip the 5-byte frame header
        let v: Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(v["type"], "lcd_upload_progress");
        let p: halod_shared::types::LcdUploadProgress =
            serde_json::from_value(v["data"].clone()).unwrap();
        assert_eq!(p.device_id, "lcd_dev");
        assert_eq!(p.stage, LcdUploadStage::Processing);
        assert_eq!(p.percent, Some(50));
    }

    #[test]
    fn rotation_reapplies_only_static_image_modes() {
        use halod_shared::types::LcdMode;
        assert!(rotation_needs_image_reapply(LcdMode::Image));
        assert!(rotation_needs_image_reapply(LcdMode::Gif));
        assert!(!rotation_needs_image_reapply(LcdMode::Default));
        assert!(!rotation_needs_image_reapply(LcdMode::Engine));
        assert!(!rotation_needs_image_reapply(LcdMode::Video));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_video_path_rejects_symlink_to_directory() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let err = resolve_video_path(link.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }

    // ── set_screen_rotation ───────────────────────────────────────────────

    fn make_app_with_lcd_device(
        dev_id: &str,
    ) -> (
        std::sync::Arc<crate::state::AppState>,
        std::sync::Arc<crate::test_support::MockDevice>,
    ) {
        let app =
            std::sync::Arc::new(crate::state::AppState::new(crate::config::Config::default()));
        let dev = std::sync::Arc::new(crate::test_support::MockDevice::new(dev_id).with_lcd());
        app.devices
            .try_write()
            .unwrap()
            .push(dev.clone() as std::sync::Arc<dyn crate::drivers::Device>);
        (app, dev)
    }

    #[tokio::test]
    async fn set_screen_rotation_accepts_all_valid_values() {
        use halod_shared::types::ScreenRotation;
        for rotation in [
            ScreenRotation::R0,
            ScreenRotation::R90,
            ScreenRotation::R180,
            ScreenRotation::R270,
        ] {
            let (app, _) = make_app_with_lcd_device("lcd_dev");
            set_screen_rotation("lcd_dev".into(), rotation, app)
                .await
                .unwrap();
        }
    }

    // ── editor session drop on mode switch ───────────────────────────────

    /// Switching a device to a concrete mode must free its editor session
    /// immediately (even when not engine-driven — the mock device has no
    /// template active), not after the 30s idle eviction.
    #[tokio::test]
    async fn mode_switch_drops_editor_session_for_device() {
        use crate::lcd::engine::custom::EditorSession;
        let (app, _) = make_app_with_lcd_device("lcd_dev");
        *app.lcd.editor_session.lock().unwrap() = Some(EditorSession::new_idle_for_test("lcd_dev"));
        set_screen_default("lcd_dev".into(), app.clone())
            .await
            .unwrap();
        assert!(app.lcd.editor_session.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn mode_switch_keeps_other_devices_editor_session() {
        use crate::lcd::engine::custom::EditorSession;
        let (app, _) = make_app_with_lcd_device("lcd_dev");
        *app.lcd.editor_session.lock().unwrap() =
            Some(EditorSession::new_idle_for_test("other_dev"));
        set_screen_default("lcd_dev".into(), app.clone())
            .await
            .unwrap();
        assert!(app.lcd.editor_session.lock().unwrap().is_some());
    }

    // ── set_screen_video ─────────────────────────────────────────────────

    /// `AppState::new` never wires up `lcd.set_engine(...)`, so `app.lcd.video()`
    /// is `None` here. That makes the video engine "fail" deterministically,
    /// without ffmpeg, *after* `resolve_video_path` has already succeeded on a
    /// real temp file — exercising the ordering guarantee from the comment in
    /// `set_screen_video`: a failure past that point must not leave the device
    /// advertising `LcdMode::Video`.
    #[tokio::test]
    async fn set_screen_video_failure_does_not_set_video_mode() {
        use halod_shared::types::LcdMode;

        let (app, dev) = make_app_with_lcd_device("lcd_dev");
        let file = tempfile::NamedTempFile::new().unwrap();

        let err = set_screen_video(
            "lcd_dev".into(),
            file.path().to_str().unwrap().to_string(),
            app,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("engines not initialized"),
            "expected the video-engine-missing error, got: {err}"
        );

        let lcd = crate::drivers::LcdCapability::lcd_state(dev.as_ref());
        assert_eq!(lcd.mode(), LcdMode::Default);
        assert!(lcd.video_path().is_none());
    }

    // ── set_screen_raw_streaming ──────────────────────────────────────────

    #[tokio::test]
    async fn set_screen_raw_streaming_updates_flag() {
        let app =
            std::sync::Arc::new(crate::state::AppState::new(crate::config::Config::default()));
        let dev = std::sync::Arc::new(crate::test_support::MockDevice::new("lcd_dev").with_lcd());
        let dev_ref = dev.clone();
        app.devices
            .try_write()
            .unwrap()
            .push(dev as std::sync::Arc<dyn crate::drivers::Device>);

        assert!(!dev_ref.lcd.as_ref().unwrap().raw_streaming());
        set_screen_raw_streaming("lcd_dev".into(), true, app.clone())
            .await
            .unwrap();
        assert!(dev_ref.lcd.as_ref().unwrap().raw_streaming());

        set_screen_raw_streaming("lcd_dev".into(), false, app)
            .await
            .unwrap();
        assert!(!dev_ref.lcd.as_ref().unwrap().raw_streaming());
    }

    #[tokio::test]
    async fn set_screen_raw_streaming_errors_on_missing_device() {
        let app =
            std::sync::Arc::new(crate::state::AppState::new(crate::config::Config::default()));
        let err = set_screen_raw_streaming("ghost".into(), true, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── delete_lcd_image ──────────────────────────────────────────────────

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_lcd_image_removes_existing_file() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };
        let lcd_dir = crate::config::lcd_images_dir();
        std::fs::create_dir_all(&lcd_dir).unwrap();
        let filename = "test_image.png";
        let path = lcd_dir.join(filename);
        std::fs::write(&path, b"fake png data").unwrap();
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        delete_lcd_image(filename.to_string(), app).await.unwrap();
        assert!(!path.exists());
        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_lcd_image_noop_when_file_does_not_exist() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };
        std::fs::create_dir_all(crate::config::lcd_images_dir()).unwrap();
        // This should return Ok even though the file doesn't exist.
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        delete_lcd_image("nonexistent.png".to_string(), app)
            .await
            .unwrap();
        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_lcd_image_rejects_path_traversal() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };
        std::fs::create_dir_all(crate::config::lcd_images_dir()).unwrap();
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let err = delete_lcd_image("../escape.png".to_string(), app)
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
        assert!(err.to_string().contains("invalid filename"));
    }
}

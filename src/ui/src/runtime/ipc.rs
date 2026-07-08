// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal read-only IPC client for the egui prototype.
//!
//! The daemon pushes a `state` frame on connect and re-broadcasts every 250 ms,
//! so a client only has to connect and decode inbound JSON frames — no
//! subscribe handshake is required (except the LCD engine preview, which is
//! lease-gated: see `device/lcd.rs`'s keepalive). Each distinct data stream
//! (state, canvas frames, LCD previews, …) lives on its own
//! [`tokio::sync::watch`] channel so a high-frequency canvas frame never
//! blocks a state read and the UI can cheaply gate work on `has_changed()`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use halod_shared::commands::DaemonCommand;
use halod_shared::frames::{decode_header, encode_json_frame, payload_exceeds_max, FRAME_JSON};
use halod_shared::lcd_custom::{CustomTemplateDef, LcdEditorRender};
use halod_shared::socket::socket_path;
use halod_shared::types::{
    AppState, CanvasFrame, LcdEngineFrame, LcdUploadProgress, Notification, NotificationCode,
    RunningApp,
};
use tokio::sync::{mpsc, watch};

/// Shared inbox for daemon notifications. The IPC thread appends and the UI
/// drains it each frame — a queue (not a `watch`) so bursts aren't coalesced
/// into a single value and lost.
pub type NotifyQueue = Arc<Mutex<Vec<Notification>>>;

/// Pre-decoded LCD engine preview frame ready for GPU upload.
///
/// Decoded eagerly in the IPC thread (base64 + PNG) so the render loop
/// only needs a cheap `Vec<Color32>` conversion + `load_texture` call.
#[derive(Clone)]
pub struct DecodedFrame {
    pub frame_id: u64,
    pub width: usize,
    pub height: usize,
    /// Raw RGBA8 pixels: `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// A widget sprite decoded off the IPC thread (base64 → straight-alpha RGBA),
/// ready for a cheap `ColorImage` + texture upload on the UI thread.
#[derive(Clone)]
pub struct DecodedSprite {
    pub id: String,
    pub signature: u64,
    pub w: usize,
    pub h: usize,
    /// Straight-alpha RGBA8 pixels: `w * h * 4` bytes.
    pub rgba: Vec<u8>,
}

/// The daemon's editor render: sprites for the widgets that changed since the
/// `known` signatures this request sent, plus the current signature of every
/// widget. Empty `signatures` means a pre-delta daemon: `sprites` is the
/// complete set (legacy full-replace semantics).
#[derive(Clone)]
pub struct DecodedEditorRender {
    pub device_id: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub sprites: Vec<DecodedSprite>,
    pub signatures: Vec<(String, u64)>,
}

/// Receiver side of every daemon-driven data stream, held by the GUI. Each
/// `watch::Receiver` exposes the latest value via a lock-free `borrow()` and a
/// `has_changed()` flag so the UI re-clones only what actually moved.
#[derive(Clone)]
pub struct UiRx {
    pub state: watch::Receiver<AppState>,
    pub connected: watch::Receiver<bool>,
    pub debug: watch::Receiver<Option<halod_shared::debug_info::DebugInfo>>,
    pub lcd_images: watch::Receiver<Vec<String>>,
    pub lcd_frames: watch::Receiver<HashMap<String, DecodedFrame>>,
    pub canvas_frame: watch::Receiver<Option<CanvasFrame>>,
    pub running_apps: watch::Receiver<Vec<RunningApp>>,
    /// Stage/percent of the in-flight LCD image upload; `None` when idle.
    pub lcd_upload: watch::Receiver<Option<LcdUploadProgress>>,
    /// The most recently loaded named LCD template, pushed in response to
    /// `LoadLcdTemplate`. `None` until one is requested.
    pub lcd_template: watch::Receiver<Option<(String, CustomTemplateDef)>>,
    /// Latest on-demand LCD editor render (per-widget sprites). `None` until the
    /// editor requests one via `RenderLcdEditor`.
    pub lcd_editor_render: watch::Receiver<Option<DecodedEditorRender>>,
    pub notifications: NotifyQueue,
}

struct UiTx {
    state: watch::Sender<AppState>,
    connected: watch::Sender<bool>,
    debug: watch::Sender<Option<halod_shared::debug_info::DebugInfo>>,
    lcd_images: watch::Sender<Vec<String>>,
    lcd_frames: watch::Sender<HashMap<String, DecodedFrame>>,
    canvas_frame: watch::Sender<Option<CanvasFrame>>,
    running_apps: watch::Sender<Vec<RunningApp>>,
    lcd_upload: watch::Sender<Option<LcdUploadProgress>>,
    lcd_template: watch::Sender<Option<(String, CustomTemplateDef)>>,
    lcd_editor_render: watch::Sender<Option<DecodedEditorRender>>,
    notifications: NotifyQueue,
}

/// Channel the UI uses to send commands to the daemon (unbounded: low-rate,
/// user-paced, and dropping a user action would be worse than a queue build-up).
/// Worst-case burst: slider drag at 60 Hz for 5 s ≈ 300 messages × ~1 KiB each
/// ≈ 300 KiB, bounded in practice by human input rate.
pub type CommandTx = mpsc::UnboundedSender<DaemonCommand>;
type CommandRx = mpsc::UnboundedReceiver<DaemonCommand>;

/// Queue a typed command for delivery to the daemon (non-blocking).
pub fn send(tx: &CommandTx, cmd: DaemonCommand) {
    if tx.send(cmd).is_err() {
        log::warn!("IPC send failed (channel closed)");
    }
}

/// Spawn the background reader/writer on its own thread + current-thread
/// runtime, returning the command sender and the receiver bundle. `repaint` is
/// called whenever new data lands so egui wakes up.
pub fn spawn(repaint: impl Fn() + Send + 'static) -> (CommandTx, UiRx) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (state_s, state_r) = watch::channel(AppState::default());
    let (conn_s, conn_r) = watch::channel(false);
    let (debug_s, debug_r) = watch::channel(None);
    let (imgs_s, imgs_r) = watch::channel(Vec::new());
    let (frames_s, frames_r) = watch::channel(HashMap::new());
    let (canvas_s, canvas_r) = watch::channel(None);
    let (apps_s, apps_r) = watch::channel(Vec::new());
    let (upload_s, upload_r) = watch::channel(None);
    let (template_s, template_r) = watch::channel(None);
    let (editor_render_s, editor_render_r) = watch::channel(None);
    let notifications: NotifyQueue = Arc::new(Mutex::new(Vec::new()));

    let tx = UiTx {
        state: state_s,
        connected: conn_s,
        debug: debug_s,
        lcd_images: imgs_s,
        lcd_frames: frames_s,
        canvas_frame: canvas_s,
        running_apps: apps_s,
        lcd_upload: upload_s,
        lcd_template: template_s,
        lcd_editor_render: editor_render_s,
        notifications: Arc::clone(&notifications),
    };
    let rx = UiRx {
        state: state_r,
        connected: conn_r,
        debug: debug_r,
        lcd_images: imgs_r,
        lcd_frames: frames_r,
        canvas_frame: canvas_r,
        running_apps: apps_r,
        lcd_upload: upload_r,
        lcd_template: template_r,
        lcd_editor_render: editor_render_r,
        notifications,
    };

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("failed to build tokio runtime for IPC: {e}");
                return;
            }
        };
        rt.block_on(reconnect_loop(tx, cmd_rx, repaint));
    });

    (cmd_tx, rx)
}

/// Retry a daemon start every Nth reconnect attempt (~500ms apart) rather than
/// every attempt, so a long outage doesn't hammer `sc.exe`/spawn a process
/// every 500ms.
const ENSURE_DAEMON_UP_EVERY: u32 = 10;

async fn reconnect_loop(tx: UiTx, mut cmd_rx: CommandRx, repaint: impl Fn()) {
    let mut attempt: u32 = 0;
    loop {
        if attempt.is_multiple_of(ENSURE_DAEMON_UP_EVERY) {
            tokio::task::spawn_blocking(crate::domain::lifecycle::ensure_daemon_up);
        }
        if let Err(e) = connect_once(&tx, &mut cmd_rx, &repaint).await {
            log::warn!("IPC connection error: {e}");
        }
        let _ = tx.connected.send(false);
        repaint();
        attempt = attempt.wrapping_add(1);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(unix)]
async fn connect_once(
    tx: &UiTx,
    cmd_rx: &mut CommandRx,
    repaint: &impl Fn(),
) -> anyhow::Result<()> {
    use tokio::net::UnixStream;
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .map_err(|e| anyhow::anyhow!("connect {path}: {e}"))?;
    read_frames(&mut stream, tx, cmd_rx, repaint).await
}

#[cfg(not(unix))]
async fn connect_once(
    tx: &UiTx,
    cmd_rx: &mut CommandRx,
    repaint: &impl Fn(),
) -> anyhow::Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let path = socket_path();
    let mut pipe = ClientOptions::new()
        .open(&path)
        .map_err(|e| anyhow::anyhow!("connect {path}: {e}"))?;
    read_frames(&mut pipe, tx, cmd_rx, repaint).await
}

async fn read_frames<S>(
    stream: &mut S,
    tx: &UiTx,
    cmd_rx: &mut CommandRx,
    repaint: &impl Fn(),
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _ = tx.connected.send(true);
    repaint();

    // Bootstrap subscriptions. LCD preview lease keepalive is handled by the LCD tab.
    for cmd in [
        DaemonCommand::ListLcdImages,
        DaemonCommand::CanvasSubscribe,
        DaemonCommand::GetDebugInfo,
    ] {
        write_cmd(stream, &cmd).await?;
    }
    stream.flush().await?;

    // Keepalive: the daemon drops any client that sends nothing for 60 s, so an
    // idle UI must ping well within that window or it sees a spurious
    // disconnect/reconnect flap. A `select!` loop also lets queued commands and
    // pings go out immediately instead of waiting for the next inbound frame.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut header = [0u8; 5];
    // Biased: heartbeat and outbound commands take priority over inbound
    // reads so queued commands and pings go out immediately.
    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                write_cmd(stream, &DaemonCommand::Ping).await?;
                stream.flush().await?;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                write_cmd(stream, &cmd).await?;
                stream.flush().await?;
            }
            result = stream.read_exact(&mut header) => {
                result?;
                let (frame_type, payload_len) = decode_header(&header);
                if payload_exceeds_max(payload_len) {
                    anyhow::bail!("daemon sent oversized frame: {payload_len} bytes");
                }
                let mut payload = vec![0u8; payload_len as usize];
                if payload_len > 0 {
                    stream.read_exact(&mut payload).await?;
                }
                // An upload completing asks us to re-fetch the image library.
                if frame_type == FRAME_JSON && handle_json(&payload, tx, repaint) {
                    write_cmd(stream, &DaemonCommand::ListLcdImages).await?;
                    stream.flush().await?;
                }
            }
        }
    }
}

/// Encode and write one typed command to the stream (no flush). A bad command
/// only warns and skips — it never tears down the connection.
async fn write_cmd<S>(stream: &mut S, cmd: &DaemonCommand) -> anyhow::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    match serde_json::to_value(cmd) {
        Ok(v) => {
            if let Some(frame) = encode_json_frame(&v) {
                stream.write_all(&frame).await?;
            }
        }
        Err(e) => log::warn!("encode command: {e}"),
    }
    Ok(())
}

/// Append a notification to the shared inbox and wake the UI. A poisoned lock
/// only drops the notification — it never panics the IPC thread.
fn push_notification(tx: &UiTx, n: Notification, repaint: &impl Fn()) {
    if let Ok(mut q) = tx.notifications.lock() {
        q.push(n);
    }
    repaint();
}

/// Route one inbound JSON frame onto the right channel. Returns `true` when the
/// caller should re-request the LCD image library (an upload just completed).
fn handle_json(payload: &[u8], tx: &UiTx, repaint: &impl Fn()) -> bool {
    let value = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("dropping malformed daemon frame ({e})");
            return false;
        }
    };
    match value.get("type").and_then(|t| t.as_str()) {
        Some("state") => {}
        Some("debug_info") => {
            if let Some(data) = value.get("data").cloned() {
                if let Ok(info) =
                    serde_json::from_value::<halod_shared::debug_info::DebugInfo>(data)
                {
                    tx.debug.send_replace(Some(info));
                    repaint();
                }
            }
            return false;
        }
        Some("image_uploaded") => {
            // Upload finished; drop any stale progress and trigger a library
            // refresh on the next read-loop iteration.
            tx.lcd_upload.send_replace(None);
            return true;
        }
        Some("lcd_upload_progress") => {
            if let Some(p) = value
                .get("data")
                .and_then(|d| serde_json::from_value::<LcdUploadProgress>(d.clone()).ok())
            {
                tx.lcd_upload.send_replace(Some(p));
                repaint();
            }
            return false;
        }
        Some("lcd_engine_frame") => {
            if let Some(data) = value.get("data").cloned() {
                if let Ok(frame) = serde_json::from_value::<LcdEngineFrame>(data) {
                    use base64::Engine as _;
                    if let Ok(png) =
                        base64::engine::general_purpose::STANDARD.decode(&frame.preview_b64)
                    {
                        if let Ok(img) = image::load_from_memory(&png) {
                            let rgba = img.into_rgba8();
                            let decoded = DecodedFrame {
                                frame_id: frame.frame_id,
                                width: rgba.width() as usize,
                                height: rgba.height() as usize,
                                rgba: rgba.into_raw(),
                            };
                            tx.lcd_frames.send_modify(|m| {
                                m.insert(frame.device_id, decoded);
                            });
                            repaint();
                        }
                    }
                }
            }
            return false;
        }
        Some("lcd_editor_render") => {
            if let Some(data) = value.get("data").cloned() {
                if let Ok(r) = serde_json::from_value::<LcdEditorRender>(data) {
                    use base64::Engine as _;
                    let sprites = r
                        .sprites
                        .into_iter()
                        .filter_map(|s| {
                            let rgba = base64::engine::general_purpose::STANDARD
                                .decode(&s.rgba_b64)
                                .ok()?;
                            Some(DecodedSprite {
                                id: s.id,
                                signature: s.signature,
                                w: s.w as usize,
                                h: s.h as usize,
                                rgba,
                            })
                        })
                        .collect();
                    tx.lcd_editor_render.send_replace(Some(DecodedEditorRender {
                        device_id: r.device_id,
                        canvas_w: r.canvas_w,
                        canvas_h: r.canvas_h,
                        sprites,
                        signatures: r.signatures,
                    }));
                    repaint();
                }
            }
            return false;
        }
        Some("canvas_frame") => {
            if let Some(data) = value.get("data").cloned() {
                if let Ok(frame) = serde_json::from_value::<CanvasFrame>(data) {
                    tx.canvas_frame.send_replace(Some(frame));
                    repaint();
                }
            }
            return false;
        }
        Some("notification") => {
            if let Some(n) = value
                .get("data")
                .and_then(|d| serde_json::from_value::<Notification>(d.clone()).ok())
            {
                push_notification(tx, n, repaint);
            }
            return false;
        }
        Some("error") => {
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| t!("notify.unknown_error").to_string());
            push_notification(
                tx,
                Notification {
                    code: NotificationCode::Generic { message },
                    timestamp_ms: 0,
                },
                repaint,
            );
            return false;
        }
        Some("running_apps_list") => {
            let apps: Vec<RunningApp> = value
                .get("apps")
                .and_then(|a| serde_json::from_value(a.clone()).ok())
                .unwrap_or_default();
            tx.running_apps.send_replace(apps);
            repaint();
            return false;
        }
        Some("lcd_images") => {
            let names: Vec<String> = value
                .get("files")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| f["name"].as_str().map(str::to_string))
                        .filter(|n| !n.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            tx.lcd_images.send_replace(names);
            repaint();
            return false;
        }
        Some("lcd_template") => {
            let name = value
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_string);
            let def = value
                .get("def")
                .and_then(|d| serde_json::from_value::<CustomTemplateDef>(d.clone()).ok());
            if let (Some(name), Some(def)) = (name, def) {
                tx.lcd_template.send_replace(Some((name, def)));
                repaint();
            }
            return false;
        }
        Some(unknown) => {
            log::debug!("IPC: dropping unknown frame type: {unknown:?}");
            return false;
        }
        None => return false,
    }
    // `ScreenRotation` deserializes legacy integer degrees on its own, so no
    // pre-pass over the JSON tree is needed here.
    let data = value.get("data").cloned().unwrap_or(value);
    match serde_json::from_value::<AppState>(data) {
        Ok(state) => {
            tx.state.send_replace(state);
            repaint();
        }
        Err(e) => log::warn!("state parse failed: {e}"),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tx() -> (UiTx, watch::Receiver<Option<LcdUploadProgress>>) {
        let (upload_s, upload_r) = watch::channel(None);
        let tx = UiTx {
            state: watch::channel(AppState::default()).0,
            connected: watch::channel(false).0,
            debug: watch::channel(None).0,
            lcd_images: watch::channel(Vec::new()).0,
            lcd_frames: watch::channel(HashMap::new()).0,
            canvas_frame: watch::channel(None).0,
            running_apps: watch::channel(Vec::new()).0,
            lcd_upload: upload_s,
            lcd_template: watch::channel(None).0,
            lcd_editor_render: watch::channel(None).0,
            notifications: Arc::new(Mutex::new(Vec::new())),
        };
        (tx, upload_r)
    }

    #[test]
    fn lcd_upload_progress_frame_lands_on_the_watch_channel() {
        let (tx, rx) = test_tx();
        let payload = br#"{"type":"lcd_upload_progress","data":{"device_id":"lcd","stage":"processing","percent":42}}"#;
        assert!(!handle_json(payload, &tx, &|| {}));
        let got = rx.borrow().clone().expect("progress routed");
        assert_eq!(got.device_id, "lcd");
        assert_eq!(got.percent, Some(42));
    }

    #[test]
    fn image_uploaded_clears_progress_and_requests_refresh() {
        let (tx, rx) = test_tx();
        tx.lcd_upload.send_replace(Some(LcdUploadProgress {
            device_id: "lcd".into(),
            stage: halod_shared::types::LcdUploadStage::Applying,
            percent: None,
        }));
        assert!(handle_json(br#"{"type":"image_uploaded"}"#, &tx, &|| {}));
        assert!(rx.borrow().is_none(), "stale progress cleared");
    }
}

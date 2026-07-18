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
use halod_shared::lcd_custom::{CustomTemplateDef, LcdEditorRender, WidgetRenderState};
use halod_shared::socket::socket_path;
use halod_shared::types::{
    AppState, CanvasFrame, LcdEngineFrame, LcdUploadProgress, Notification, NotificationCode,
    PluginUpdateStatus, RepoUpdateStatus, RunningApp,
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
/// widget.
#[derive(Clone)]
pub struct DecodedEditorRender {
    pub device_id: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub sprites: Vec<DecodedSprite>,
    pub signatures: Vec<(String, u64)>,
    pub widgets: Vec<WidgetRenderState>,
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
    /// Decoded plugin display assets, keyed by `plugin_asset_cache_key`.
    pub plugin_assets: watch::Receiver<HashMap<String, Vec<u8>>>,
    /// Latest repo update-check result; empty until one is requested.
    pub repo_updates: watch::Receiver<Vec<RepoUpdateStatus>>,
    /// Latest per-plugin update-check result; empty until one is requested.
    pub plugin_updates: watch::Receiver<Vec<PluginUpdateStatus>>,
    pub udev_rules: watch::Receiver<Option<halod_shared::types::UdevRulesStatus>>,
    /// Remote branch lists keyed by the repo URL they were fetched for; empty
    /// until the Add-repository picker requests one via `ListRepoBranches`.
    pub repo_branches: watch::Receiver<HashMap<String, Vec<String>>>,
    /// Host serial ports for the `serial_port` config-field dropdown; empty until
    /// a config screen requests one via `ListSerialPorts`.
    pub serial_ports: watch::Receiver<Vec<halod_shared::types::SerialPortInfo>>,
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

pub(crate) struct UiTx {
    state: watch::Sender<AppState>,
    connected: watch::Sender<bool>,
    debug: watch::Sender<Option<halod_shared::debug_info::DebugInfo>>,
    lcd_images: watch::Sender<Vec<String>>,
    lcd_frames: watch::Sender<HashMap<String, DecodedFrame>>,
    canvas_frame: watch::Sender<Option<CanvasFrame>>,
    running_apps: watch::Sender<Vec<RunningApp>>,
    plugin_assets: watch::Sender<HashMap<String, Vec<u8>>>,
    repo_updates: watch::Sender<Vec<RepoUpdateStatus>>,
    plugin_updates: watch::Sender<Vec<PluginUpdateStatus>>,
    udev_rules: watch::Sender<Option<halod_shared::types::UdevRulesStatus>>,
    repo_branches: watch::Sender<HashMap<String, Vec<String>>>,
    serial_ports: watch::Sender<Vec<halod_shared::types::SerialPortInfo>>,
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

/// Parse `value[field]` into `T`, falling back to `T::default()` when the
/// field is missing or malformed — the tolerant shape every push-stream
/// payload field is read with.
fn field_or_default<T: serde::de::DeserializeOwned + Default>(
    value: &serde_json::Value,
    field: &str,
) -> T {
    value
        .get(field)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Take a watch channel's new value, if any: `Some` exactly once per change.
/// Logs once per call under `name` if the channel has closed.
pub fn take_changed<T: Clone>(rx: &mut watch::Receiver<T>, name: &str) -> Option<T> {
    match rx.has_changed() {
        Ok(true) => Some(rx.borrow_and_update().clone()),
        Ok(false) => None,
        Err(_) => {
            log::warn!("IPC {name} channel closed");
            None
        }
    }
}

/// Queue a typed command for delivery to the daemon (non-blocking).
pub fn send(tx: &CommandTx, cmd: DaemonCommand) {
    if tx.send(cmd).is_err() {
        log::warn!("IPC send failed (channel closed)");
    }
}

/// Build the paired sender/receiver bundles for every daemon-driven stream.
/// Split out from [`spawn`] so headless tests can wire a `UiRx` to senders they
/// keep and drive directly (see [`fake`]).
fn channels() -> (UiTx, UiRx) {
    let (state_s, state_r) = watch::channel(AppState::default());
    let (conn_s, conn_r) = watch::channel(false);
    let (debug_s, debug_r) = watch::channel(None);
    let (imgs_s, imgs_r) = watch::channel(Vec::new());
    let (frames_s, frames_r) = watch::channel(HashMap::new());
    let (canvas_s, canvas_r) = watch::channel(None);
    let (apps_s, apps_r) = watch::channel(Vec::new());
    let (assets_s, assets_r) = watch::channel(HashMap::new());
    let (repo_updates_s, repo_updates_r) = watch::channel(Vec::new());
    let (plugin_updates_s, plugin_updates_r) = watch::channel(Vec::new());
    let (udev_rules_s, udev_rules_r) = watch::channel(None);
    let (repo_branches_s, repo_branches_r) = watch::channel(HashMap::new());
    let (serial_ports_s, serial_ports_r) = watch::channel(Vec::new());
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
        plugin_assets: assets_s,
        repo_updates: repo_updates_s,
        plugin_updates: plugin_updates_s,
        udev_rules: udev_rules_s,
        repo_branches: repo_branches_s,
        serial_ports: serial_ports_s,
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
        plugin_assets: assets_r,
        repo_updates: repo_updates_r,
        plugin_updates: plugin_updates_r,
        udev_rules: udev_rules_r,
        repo_branches: repo_branches_r,
        serial_ports: serial_ports_r,
        lcd_upload: upload_r,
        lcd_template: template_r,
        lcd_editor_render: editor_render_r,
        notifications,
    };
    (tx, rx)
}

/// Seed a `UiRx` with a fixed snapshot for headless rendering, no socket
/// involved. The returned `UiTx` owns the sender halves and must be held for
/// the receiver's lifetime, or every channel reports closed.
#[cfg(all(test, target_os = "linux", feature = "screenshots"))]
pub(crate) fn fake(state: AppState, connected: bool) -> (CommandTx, UiRx, UiTx) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (tx, rx) = channels();
    tx.state.send_replace(state);
    tx.connected.send_replace(connected);
    // Keep the command channel open so the draw path's `send`s stay quiet.
    std::mem::forget(cmd_rx);
    (cmd_tx, rx, tx)
}

/// Spawn the background reader/writer on its own thread + current-thread
/// runtime, returning the command sender and the receiver bundle. `repaint` is
/// called whenever new data lands so egui wakes up.
pub fn spawn(repaint: impl Fn() + Send + 'static) -> (CommandTx, UiRx) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (tx, rx) = channels();

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
/// every attempt, so a long outage doesn't spawn a process every 500ms.
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

    // Bootstrap subscriptions. LCD preview lease keepalive is handled by the LCD tab.
    for cmd in [
        DaemonCommand::ListLcdImages,
        DaemonCommand::CanvasSubscribe,
        DaemonCommand::GetDebugInfo,
    ] {
        write_cmd(stream, &cmd).await?;
    }
    #[cfg(target_os = "linux")]
    write_cmd(stream, &DaemonCommand::GetUdevRulesStatus).await?;
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
                #[cfg(target_os = "linux")]
                write_cmd(stream, &DaemonCommand::GetUdevRulesStatus).await?;
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

/// Key an asset is stored/looked up under in `UiRx::plugin_assets`.
pub fn plugin_asset_cache_key(plugin_id: &str, name: &str) -> String {
    format!("{plugin_id}/{name}")
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
                        widgets: r.widgets,
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
            // The plugin layer already sent a structured notification whose
            // Details modal contains the full callback error.
            if value.get("handled").and_then(|v| v.as_bool()) == Some(true) {
                return false;
            }
            let message = if value.get("code").and_then(|code| code.as_str())
                == Some(halod_shared::types::ERROR_REPOSITORY_SIGNATURE_VERIFICATION_FAILED)
            {
                t!("plugins.repo_signature_verification_failed").to_string()
            } else {
                value
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| t!("notify.unknown_error").to_string())
            };
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
        Some("plugin_asset") => {
            let plugin_id = value.get("plugin_id").and_then(|v| v.as_str());
            let name = value.get("name").and_then(|v| v.as_str());
            let data_b64 = value.get("data_b64").and_then(|v| v.as_str());
            if let (Some(plugin_id), Some(name), Some(data_b64)) = (plugin_id, name, data_b64) {
                use base64::Engine as _;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                    let key = plugin_asset_cache_key(plugin_id, name);
                    tx.plugin_assets.send_modify(|m| {
                        m.insert(key, bytes);
                    });
                    repaint();
                }
            }
            return false;
        }
        Some("lcd_widget_icon") => {
            let catalog_id = value.get("catalog_id").and_then(|value| value.as_str());
            let data_b64 = value.get("data_b64").and_then(|value| value.as_str());
            if let (Some(catalog_id), Some(data_b64)) = (catalog_id, data_b64) {
                use base64::Engine as _;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                    tx.plugin_assets.send_modify(|assets| {
                        assets.insert(format!("lcd/{catalog_id}"), bytes);
                    });
                    repaint();
                }
            }
            return false;
        }
        Some("plugin_repo_updates") => {
            tx.repo_updates
                .send_replace(field_or_default(&value, "repos"));
            repaint();
            return false;
        }
        Some("plugin_updates") => {
            tx.plugin_updates
                .send_replace(field_or_default(&value, "plugins"));
            repaint();
            return false;
        }
        Some("udev_rules_status") => {
            tx.udev_rules.send_replace(field_or_default(&value, "data"));
            repaint();
            return false;
        }
        Some("serial_ports") => {
            if let Some(ports) = value.get("ports").and_then(|p| {
                serde_json::from_value::<Vec<halod_shared::types::SerialPortInfo>>(p.clone()).ok()
            }) {
                tx.serial_ports.send_replace(ports);
                repaint();
            }
            return false;
        }
        Some("repo_branches") => {
            let url = value.get("url").and_then(|u| u.as_str());
            let branches: Option<Vec<String>> = value
                .get("branches")
                .and_then(|b| serde_json::from_value(b.clone()).ok());
            if let (Some(url), Some(branches)) = (url, branches) {
                let url = url.to_owned();
                tx.repo_branches.send_modify(|m| {
                    m.insert(url, branches);
                });
                repaint();
            }
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
        Some("lcd_plugin_preset") => {
            let name = value
                .get("catalog_id")
                .and_then(|name| name.as_str())
                .map(str::to_owned);
            let def = value
                .get("def")
                .and_then(|def| serde_json::from_value::<CustomTemplateDef>(def.clone()).ok());
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
    let data = value.get("data").cloned().unwrap_or(value);
    match serde_json::from_value::<AppState>(data) {
        Ok(state) => {
            // Make the first daemon-backed frame paint in the configured
            // language. `connected` deliberately flips only after this point,
            // so the radar never renders discovery phases using the default
            // locale while the initial state is still in flight.
            crate::ui::screens::settings::apply_locale(&state.gui.language);
            tx.state.send_replace(state);
            tx.connected.send_replace(true);
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
            plugin_assets: watch::channel(HashMap::new()).0,
            repo_updates: watch::channel(Vec::new()).0,
            plugin_updates: watch::channel(Vec::new()).0,
            udev_rules: watch::channel(None).0,
            repo_branches: watch::channel(HashMap::new()).0,
            serial_ports: watch::channel(Vec::new()).0,
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
    fn connection_becomes_ready_only_after_initial_state() {
        let (tx, _upload_r) = test_tx();
        let connected = tx.connected.subscribe();
        assert!(!*connected.borrow());

        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "state",
            "data": AppState::default(),
        }))
        .unwrap();
        assert!(!handle_json(&payload, &tx, &|| {}));

        assert!(*connected.borrow());
    }

    #[test]
    fn plugin_asset_cache_key_is_scoped_per_plugin() {
        assert_eq!(plugin_asset_cache_key("acme", "logo.png"), "acme/logo.png");
        assert_ne!(
            plugin_asset_cache_key("acme", "logo.png"),
            plugin_asset_cache_key("other", "logo.png"),
        );
    }

    #[test]
    fn plugin_asset_frame_lands_on_the_watch_channel() {
        use base64::Engine as _;
        let (tx, _upload_r) = test_tx();
        let mut assets_r = tx.plugin_assets.subscribe();
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"PNGDATA");
        let payload = format!(
            r#"{{"type":"plugin_asset","plugin_id":"acme","name":"logo.png","data_b64":"{data_b64}"}}"#
        );
        assert!(!handle_json(payload.as_bytes(), &tx, &|| {}));
        let got = assets_r
            .borrow_and_update()
            .get("acme/logo.png")
            .cloned()
            .expect("asset routed");
        assert_eq!(got, b"PNGDATA");
    }

    #[test]
    fn plugin_repo_updates_frame_lands_on_the_watch_channel() {
        let (tx, _upload_r) = test_tx();
        let mut repo_updates_r = tx.repo_updates.subscribe();
        let payload = br#"{"type":"plugin_repo_updates","repos":[
            {"slug":"foo","locked_sha":"aaa","remote_sha":"bbb","behind":true}
        ]}"#;
        assert!(!handle_json(payload, &tx, &|| {}));
        let got = repo_updates_r.borrow_and_update().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].slug, "foo");
        assert!(got[0].behind);
    }

    #[test]
    fn udev_status_frame_lands_on_the_watch_channel() {
        let (tx, _) = test_tx();
        let mut rx = tx.udev_rules.subscribe();
        let payload = br#"{"type":"udev_rules_status","data":{"supported":true,"current":false,"installed_path":"/usr/lib/udev/rules.d/60-halod.rules","generated_rule_count":43}}"#;
        assert!(!handle_json(payload, &tx, &|| {}));
        let got = rx.borrow_and_update().clone().unwrap();
        assert!(got.supported);
        assert!(!got.current);
        assert_eq!(got.generated_rule_count, 43);
    }

    #[test]
    fn plugin_updates_frame_lands_on_the_watch_channel() {
        let (tx, _upload_r) = test_tx();
        let mut plugin_updates_r = tx.plugin_updates.subscribe();
        let payload = br#"{"type":"plugin_updates","plugins":[
            {"plugin_id":"wled_udp","slug":"foo","update_available":true,"current_version":"1.0.0","available_version":"1.1.0"}
        ]}"#;
        assert!(!handle_json(payload, &tx, &|| {}));
        let got = plugin_updates_r.borrow_and_update().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].plugin_id, "wled_udp");
        assert!(got[0].update_available);
    }

    #[test]
    fn terminal_upload_frames_land_on_the_watch_channel() {
        use halod_shared::types::LcdUploadStage;
        // The spinner-clearing terminals ride the same untouched handler.
        for (raw, stage) in [
            (
                br#"{"type":"lcd_upload_progress","data":{"device_id":"lcd","stage":"done"}}"#
                    .as_slice(),
                LcdUploadStage::Done,
            ),
            (
                br#"{"type":"lcd_upload_progress","data":{"device_id":"lcd","stage":"failed"}}"#
                    .as_slice(),
                LcdUploadStage::Failed,
            ),
        ] {
            let (tx, rx) = test_tx();
            assert!(!handle_json(raw, &tx, &|| {}));
            let got = rx.borrow().clone().expect("terminal routed");
            assert_eq!(got.stage, stage);
        }
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

    #[test]
    fn handled_command_error_does_not_create_a_duplicate_generic_toast() {
        let (tx, _rx) = test_tx();
        assert!(!handle_json(
            br#"{"type":"error","message":"raw stack trace","handled":true}"#,
            &tx,
            &|| {}
        ));
        assert!(tx.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn signature_failure_uses_localized_message_instead_of_backend_text() {
        let (tx, _rx) = test_tx();
        assert!(!handle_json(
            br#"{"type":"error","code":"repository_signature_verification_failed","message":"repository signature does not match repository.yaml"}"#,
            &tx,
            &|| {}
        ));
        let notification = tx.notifications.lock().unwrap().pop().unwrap();
        assert_eq!(
            notification.code,
            NotificationCode::Generic {
                message: "Repository signature verification failed.".into()
            }
        );
    }

    #[test]
    fn take_changed_yields_each_value_exactly_once() {
        let (tx, mut rx) = watch::channel(0u32);
        assert_eq!(take_changed(&mut rx, "test"), None);
        tx.send_replace(7);
        assert_eq!(take_changed(&mut rx, "test"), Some(7));
        assert_eq!(take_changed(&mut rx, "test"), None);
        // A closed channel yields None instead of panicking.
        drop(tx);
        assert_eq!(take_changed(&mut rx, "test"), None);
    }

    #[test]
    fn field_or_default_tolerates_missing_and_malformed_fields() {
        let valid = serde_json::json!({ "repos": ["a", "b"] });
        assert_eq!(
            field_or_default::<Vec<String>>(&valid, "repos"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            field_or_default::<Vec<String>>(&valid, "missing"),
            Vec::<String>::new()
        );
        let malformed = serde_json::json!({ "repos": 42 });
        assert_eq!(
            field_or_default::<Vec<String>>(&malformed, "repos"),
            Vec::<String>::new()
        );
    }
}

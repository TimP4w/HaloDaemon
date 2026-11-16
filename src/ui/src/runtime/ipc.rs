// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal read-only IPC client for the egui prototype.
//!
//! After its bus subscription the client applies the daemon's typed
//! [`DaemonMessage`]s. Each distinct data stream
//! (state, canvas frames, LCD previews, …) lives on its own
//! [`tokio::sync::watch`] channel so a high-frequency canvas frame never
//! blocks a state read and the UI can cheaply gate work on `has_changed()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::domain::topic_store::TopicStore;
use halod_shared::bus::{BusEvent, BusEventPayload, BusSubscribe, ClientHello};
use halod_shared::commands::DaemonCommand;
use halod_shared::frames::{decode_header, encode_json_frame, payload_exceeds_max, FRAME_JSON};
use halod_shared::lcd_custom::{CustomTemplateDef, LcdEditorRender, WidgetRenderState};
use halod_shared::messages::DaemonMessage;
use halod_shared::socket::socket_path;
use halod_shared::types::{
    CanvasFrame, LcdEngineFrame, LcdUploadProgress, Notification, NotificationCode, RunningApp,
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
    pub state: Synced<TopicStore>,
    pub connected: watch::Receiver<bool>,
    pub debug: Synced<Option<halod_shared::debug_info::DebugInfo>>,
    pub lcd_images: Synced<Vec<String>>,
    pub lcd_frames: watch::Receiver<HashMap<String, DecodedFrame>>,
    pub canvas_frame: Synced<Option<CanvasFrame>>,
    pub running_apps: Synced<Vec<RunningApp>>,
    /// Decoded plugin display assets, keyed by `plugin_asset_cache_key`.
    pub plugin_assets: Synced<HashMap<String, Vec<u8>>>,
    pub udev_rules: Synced<Option<halod_shared::types::UdevRulesStatus>>,
    /// Host serial ports for the `serial_port` config-field dropdown; empty until
    /// a config screen requests one via `ListSerialPorts`.
    pub serial_ports: Synced<Vec<halod_shared::types::SerialPortInfo>>,
    /// Stage/percent of the in-flight LCD image upload; `None` when idle.
    pub lcd_upload: Synced<Option<LcdUploadProgress>>,
    /// The most recently loaded named LCD template, pushed in response to
    /// `LoadLcdTemplate`. `None` until one is requested.
    pub lcd_template: watch::Receiver<Option<(String, CustomTemplateDef)>>,
    /// Latest on-demand LCD editor render (per-widget sprites). `None` until the
    /// editor requests one via `RenderLcdEditor`.
    pub lcd_editor_render: Synced<Option<DecodedEditorRender>>,
    pub notifications: NotifyQueue,
}

pub(crate) struct UiTx {
    state: watch::Sender<TopicStore>,
    connected: watch::Sender<bool>,
    debug: watch::Sender<Option<halod_shared::debug_info::DebugInfo>>,
    lcd_images: watch::Sender<Vec<String>>,
    lcd_frames: watch::Sender<HashMap<String, DecodedFrame>>,
    canvas_frame: watch::Sender<Option<CanvasFrame>>,
    running_apps: watch::Sender<Vec<RunningApp>>,
    plugin_assets: watch::Sender<HashMap<String, Vec<u8>>>,
    udev_rules: watch::Sender<Option<halod_shared::types::UdevRulesStatus>>,
    serial_ports: watch::Sender<Vec<halod_shared::types::SerialPortInfo>>,
    lcd_upload: watch::Sender<Option<LcdUploadProgress>>,
    lcd_template: watch::Sender<Option<(String, CustomTemplateDef)>>,
    lcd_editor_render: watch::Sender<Option<DecodedEditorRender>>,
    notifications: NotifyQueue,
    hidden: Arc<AtomicBool>,
    last_event_id: Arc<AtomicU64>,
    event_session_id: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct FrameSinks {
    lcd_frames: watch::Sender<HashMap<String, DecodedFrame>>,
    canvas_frame: watch::Sender<Option<CanvasFrame>>,
    plugin_assets: watch::Sender<HashMap<String, Vec<u8>>>,
    lcd_editor_render: watch::Sender<Option<DecodedEditorRender>>,
}

impl FrameSinks {
    pub fn clear(&self) {
        self.lcd_frames.send_replace(HashMap::new());
        self.canvas_frame.send_replace(None);
        self.plugin_assets.send_replace(HashMap::new());
        self.lcd_editor_render.send_replace(None);
    }
}

/// Channel the UI uses to send commands to the daemon (unbounded: low-rate,
/// user-paced, and dropping a user action would be worse than a queue build-up).
/// Worst-case burst: slider drag at 60 Hz for 5 s ≈ 300 messages × ~1 KiB each
/// ≈ 300 KiB, bounded in practice by human input rate.
pub type CommandTx = mpsc::UnboundedSender<DaemonCommand>;
type CommandRx = mpsc::UnboundedReceiver<DaemonCommand>;

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

/// A watch receiver plus the last value it delivered, re-cloned only on change.
#[derive(Clone)]
pub struct Synced<T> {
    rx: watch::Receiver<T>,
    value: Arc<T>,
    name: &'static str,
}

impl<T: Clone> Synced<T> {
    fn new(rx: watch::Receiver<T>, name: &'static str) -> Self {
        let value = Arc::new(rx.borrow().clone());
        Self { rx, value, name }
    }

    /// Adopt the channel's new value, if any; true when it changed.
    pub fn poll(&mut self) -> bool {
        take_changed(&mut self.rx, self.name)
            .map(|v| self.value = Arc::new(v))
            .is_some()
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn arc(&self) -> Arc<T> {
        Arc::clone(&self.value)
    }

    pub fn set(&mut self, value: T) {
        self.value = Arc::new(value);
    }

    #[cfg(target_os = "linux")]
    pub fn subscribe(&self) -> watch::Receiver<T> {
        self.rx.clone()
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
fn channels(hidden: Arc<AtomicBool>) -> (UiTx, UiRx, FrameSinks) {
    let (state_s, state_r) = watch::channel(TopicStore::default());
    let (conn_s, conn_r) = watch::channel(false);
    let (debug_s, debug_r) = watch::channel(None);
    let (imgs_s, imgs_r) = watch::channel(Vec::new());
    let (frames_s, frames_r) = watch::channel(HashMap::new());
    let (canvas_s, canvas_r) = watch::channel(None);
    let (apps_s, apps_r) = watch::channel(Vec::new());
    let (assets_s, assets_r) = watch::channel(HashMap::new());
    let (udev_rules_s, udev_rules_r) = watch::channel(None);
    let (serial_ports_s, serial_ports_r) = watch::channel(Vec::new());
    let (upload_s, upload_r) = watch::channel(None);
    let (template_s, template_r) = watch::channel(None);
    let (editor_render_s, editor_render_r) = watch::channel(None);
    let notifications: NotifyQueue = Arc::new(Mutex::new(Vec::new()));

    let sinks = FrameSinks {
        lcd_frames: frames_s.clone(),
        canvas_frame: canvas_s.clone(),
        plugin_assets: assets_s.clone(),
        lcd_editor_render: editor_render_s.clone(),
    };

    let tx = UiTx {
        state: state_s,
        connected: conn_s,
        debug: debug_s,
        lcd_images: imgs_s,
        lcd_frames: frames_s,
        canvas_frame: canvas_s,
        running_apps: apps_s,
        plugin_assets: assets_s,
        udev_rules: udev_rules_s,
        serial_ports: serial_ports_s,
        lcd_upload: upload_s,
        lcd_template: template_s,
        lcd_editor_render: editor_render_s,
        notifications: Arc::clone(&notifications),
        hidden,
        last_event_id: Arc::new(AtomicU64::new(0)),
        event_session_id: Arc::new(AtomicU64::new(0)),
    };
    let rx = UiRx {
        state: Synced::new(state_r, "state"),
        connected: conn_r,
        debug: Synced::new(debug_r, "debug"),
        lcd_images: Synced::new(imgs_r, "lcd_images"),
        lcd_frames: frames_r,
        canvas_frame: Synced::new(canvas_r, "canvas_frame"),
        running_apps: Synced::new(apps_r, "running_apps"),
        plugin_assets: Synced::new(assets_r, "plugin_assets"),
        udev_rules: Synced::new(udev_rules_r, "udev_rules"),
        serial_ports: Synced::new(serial_ports_r, "serial_ports"),
        lcd_upload: Synced::new(upload_r, "lcd_upload"),
        lcd_template: template_r,
        lcd_editor_render: Synced::new(editor_render_r, "lcd_editor_render"),
        notifications,
    };
    (tx, rx, sinks)
}

/// Seed a `UiRx` with a fixed snapshot for headless rendering, no socket
/// involved. The returned `UiTx` owns the sender halves and must be held for
/// the receiver's lifetime, or every channel reports closed.
#[cfg(all(test, target_os = "linux", feature = "screenshots"))]
pub(crate) fn fake(state: TopicStore, connected: bool) -> (CommandTx, UiRx, UiTx, FrameSinks) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (tx, rx, sinks) = channels(Arc::new(AtomicBool::new(false)));
    tx.state.send_replace(state);
    tx.connected.send_replace(connected);
    // Keep the command channel open so the draw path's `send`s stay quiet.
    std::mem::forget(cmd_rx);
    (cmd_tx, rx, tx, sinks)
}

/// Spawn the background reader/writer on its own thread + current-thread
/// runtime, returning the command sender and the receiver bundle. `repaint` is
/// called whenever new data lands so egui wakes up.
pub fn spawn(
    repaint: impl Fn() + Send + 'static,
    hidden: Arc<AtomicBool>,
) -> (CommandTx, UiRx, FrameSinks) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (tx, rx, sinks) = channels(hidden);

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

    (cmd_tx, rx, sinks)
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
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", path.display()))?;
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

    let subscription = BusSubscribe {
        prefixes: Vec::new(),
        last_event_id: Some(tx.last_event_id.load(Ordering::Acquire)),
        event_session_id: Some(tx.event_session_id.load(Ordering::Acquire)),
    };
    let frame = encode_json_frame(&ClientHello::BusSubscribe(subscription))
        .ok_or_else(|| anyhow::anyhow!("bus subscription exceeds IPC frame limit"))?;
    stream.write_all(&frame).await?;

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
                if frame_type == FRAME_JSON {
                    let reaction = handle_json(&payload, tx, repaint);
                    if reaction.relist_lcd_images {
                        write_cmd(stream, &DaemonCommand::ListLcdImages).await?;
                    }
                    if reaction.relist_lcd_images {
                        stream.flush().await?;
                    }
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
    // Native delivery must not depend on an egui frame. On Linux the Wayland
    // window is destroyed while HaloDaemon is resident in the tray, so there
    // may be no frame to drain this inbox until the user opens the window
    // again. Deliver at the authoritative IPC ingestion point and retain the
    // queue solely for in-app toasts and UI observers.
    #[cfg(not(test))]
    crate::ui::root::show_native_notifications(std::slice::from_ref(&n));
    if let Ok(mut q) = tx.notifications.lock() {
        q.push(n);
    }
    repaint();
}

fn handle_bus_event(tx: &UiTx, event: BusEvent, repaint: &impl Fn()) {
    let mut observed = tx.last_event_id.load(Ordering::Acquire);
    loop {
        if event.id <= observed {
            return;
        }
        match tx.last_event_id.compare_exchange_weak(
            observed,
            event.id,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => observed = current,
        }
    }
    match event.payload {
        BusEventPayload::Notification(notification) => {
            push_notification(tx, notification, repaint);
        }
    }
}

/// Key an asset is stored/looked up under in `UiRx::plugin_assets`.
pub fn plugin_asset_cache_key(plugin_id: &str, name: &str) -> String {
    format!("{plugin_id}/{name}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reaction {
    pub relist_lcd_images: bool,
}

fn handle_json(payload: &[u8], tx: &UiTx, repaint: &impl Fn()) -> Reaction {
    let message = match serde_json::from_slice::<DaemonMessage>(payload) {
        Ok(message) => message,
        Err(e) => {
            log::debug!("IPC: dropping undecodable daemon frame ({e})");
            return Reaction::default();
        }
    };
    let hidden = tx.hidden.load(Ordering::Relaxed);
    match message {
        DaemonMessage::Pong => {}
        DaemonMessage::BusSnapshot { data } => {
            tx.state.send_modify(|store| store.replace_snapshot(data));
            crate::ui::screens::settings::apply_locale(&tx.state.borrow().gui.language);
            tx.connected.send_replace(true);
            repaint();
        }
        DaemonMessage::BusTransaction { data } => {
            tx.state.send_modify(|store| store.apply_transaction(data));
            crate::ui::screens::settings::apply_locale(&tx.state.borrow().gui.language);
            repaint();
        }
        DaemonMessage::BusEvent { data } => handle_bus_event(tx, data, repaint),
        DaemonMessage::BusEventReplay { data } => {
            let previous = tx.event_session_id.swap(data.session_id, Ordering::AcqRel);
            if previous != data.session_id {
                tx.last_event_id.store(0, Ordering::Release);
            }
            for event in data.events {
                handle_bus_event(tx, event, repaint);
            }
        }
        DaemonMessage::DebugInfo { data } => {
            tx.debug.send_replace(Some(data));
            repaint();
        }
        DaemonMessage::ImageUploaded => {
            tx.lcd_upload.send_replace(None);
            return Reaction {
                relist_lcd_images: true,
            };
        }
        DaemonMessage::LcdUploadProgress { data } => {
            tx.lcd_upload.send_replace(Some(data));
            repaint();
        }
        DaemonMessage::LcdEngineFrame { data } if !hidden => {
            if let Some(decoded) = decode_lcd_frame(&data) {
                tx.lcd_frames
                    .send_replace(HashMap::from([(data.device_id, decoded)]));
                repaint();
            }
        }
        DaemonMessage::LcdEditorRender { data } if !hidden => {
            tx.lcd_editor_render
                .send_replace(Some(decode_editor_render(data)));
            repaint();
        }
        DaemonMessage::CanvasFrame { data } if !hidden => {
            tx.canvas_frame
                .send_replace(Some(Arc::unwrap_or_clone(data)));
            repaint();
        }
        DaemonMessage::Error(error) => {
            if error.handled {
                return Reaction::default();
            }
            let message = match error.code {
                Some(code) => catalog_error_message(code),
                None => error.message,
            };
            push_notification(
                tx,
                Notification {
                    code: NotificationCode::Generic { message },
                    show_native: false,
                    timestamp_ms: 0,
                },
                repaint,
            );
        }
        DaemonMessage::RunningAppsList { apps } => {
            tx.running_apps.send_replace(apps);
            repaint();
        }
        DaemonMessage::LcdImages { files } => {
            tx.lcd_images.send_replace(
                files
                    .into_iter()
                    .map(|file| file.name)
                    .filter(|name| !name.is_empty())
                    .collect(),
            );
            repaint();
        }
        DaemonMessage::PluginAsset {
            plugin_id,
            name,
            data_b64,
        } if !hidden => {
            if let Some(bytes) = decode_b64(&data_b64) {
                tx.plugin_assets.send_modify(|assets| {
                    assets.insert(plugin_asset_cache_key(&plugin_id, &name), bytes);
                });
                repaint();
            }
        }
        DaemonMessage::LcdWidgetIcon {
            catalog_id,
            data_b64,
        } if !hidden => {
            if let Some(bytes) = decode_b64(&data_b64) {
                tx.plugin_assets.send_modify(|assets| {
                    assets.insert(format!("lcd/{catalog_id}"), bytes);
                });
                repaint();
            }
        }
        DaemonMessage::UdevRulesStatus { data } => {
            tx.udev_rules.send_replace(Some(data));
            repaint();
        }
        DaemonMessage::SerialPorts { ports } => {
            tx.serial_ports.send_replace(ports);
            repaint();
        }
        DaemonMessage::LcdTemplate { name, def }
        | DaemonMessage::LcdPluginPreset {
            catalog_id: name,
            def,
        } => {
            tx.lcd_template.send_replace(Some((name, def)));
            repaint();
        }
        DaemonMessage::LcdEngineFrame { .. }
        | DaemonMessage::LcdEditorRender { .. }
        | DaemonMessage::CanvasFrame { .. }
        | DaemonMessage::PluginAsset { .. }
        | DaemonMessage::LcdWidgetIcon { .. } => {}
    }
    Reaction::default()
}

fn decode_b64(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

fn decode_lcd_frame(frame: &LcdEngineFrame) -> Option<DecodedFrame> {
    let rgba = image::load_from_memory(&decode_b64(&frame.preview_b64)?)
        .ok()?
        .into_rgba8();
    Some(DecodedFrame {
        frame_id: frame.frame_id,
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

fn decode_editor_render(render: LcdEditorRender) -> DecodedEditorRender {
    let sprites = render
        .sprites
        .into_iter()
        .filter_map(|sprite| {
            Some(DecodedSprite {
                rgba: decode_b64(&sprite.rgba_b64)?,
                id: sprite.id,
                signature: sprite.signature,
                w: sprite.w as usize,
                h: sprite.h as usize,
            })
        })
        .collect();
    DecodedEditorRender {
        device_id: render.device_id,
        canvas_w: render.canvas_w,
        canvas_h: render.canvas_h,
        sprites,
        signatures: render.signatures,
        widgets: render.widgets,
    }
}

fn catalog_error_message(code: halod_shared::types::CatalogErrorCode) -> String {
    use halod_shared::types::CatalogErrorCode;
    match code {
        CatalogErrorCode::Malformed => t!("plugins.catalog_malformed"),
        CatalogErrorCode::Unsigned => t!("plugins.catalog_unsigned"),
        CatalogErrorCode::UntrustedKey => t!("plugins.catalog_untrusted_key"),
        CatalogErrorCode::SignatureMismatch => t!("plugins.catalog_signature_mismatch"),
        CatalogErrorCode::Stale => t!("plugins.catalog_stale"),
        CatalogErrorCode::HashMismatch => t!("plugins.catalog_hash_mismatch"),
        CatalogErrorCode::ApiIncompatible => t!("plugins.requires_newer_daemon"),
    }
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::bus::BusEventReplay;

    fn test_tx() -> (UiTx, watch::Receiver<Option<LcdUploadProgress>>) {
        let (upload_s, upload_r) = watch::channel(None);
        let tx = UiTx {
            state: watch::channel(TopicStore::default()).0,
            connected: watch::channel(false).0,
            debug: watch::channel(None).0,
            lcd_images: watch::channel(Vec::new()).0,
            lcd_frames: watch::channel(HashMap::new()).0,
            canvas_frame: watch::channel(None).0,
            running_apps: watch::channel(Vec::new()).0,
            plugin_assets: watch::channel(HashMap::new()).0,
            udev_rules: watch::channel(None).0,
            serial_ports: watch::channel(Vec::new()).0,
            lcd_upload: upload_s,
            lcd_template: watch::channel(None).0,
            lcd_editor_render: watch::channel(None).0,
            notifications: Arc::new(Mutex::new(Vec::new())),
            hidden: Arc::new(AtomicBool::new(false)),
            last_event_id: Arc::new(AtomicU64::new(0)),
            event_session_id: Arc::new(AtomicU64::new(0)),
        };
        (tx, upload_r)
    }

    #[test]
    fn lcd_upload_progress_frame_lands_on_the_watch_channel() {
        let (tx, rx) = test_tx();
        let payload = br#"{"type":"lcd_upload_progress","data":{"device_id":"lcd","stage":"processing","percent":42}}"#;
        assert!(!handle_json(payload, &tx, &|| {}).relist_lcd_images);
        let got = rx.borrow().clone().expect("progress routed");
        assert_eq!(got.device_id, "lcd");
        assert_eq!(got.percent, Some(42));
    }

    #[test]
    fn new_daemon_event_session_resets_stale_notification_cursor() {
        let (tx, _) = test_tx();
        tx.event_session_id.store(11, Ordering::Release);
        tx.last_event_id.store(900, Ordering::Release);
        let replay = BusEventReplay {
            session_id: 12,
            oldest_available_id: Some(1),
            events: vec![BusEvent {
                id: 1,
                payload: BusEventPayload::Notification(Notification {
                    code: NotificationCode::ProfileSwitched {
                        profile: "Gaming".into(),
                    },
                    show_native: true,
                    timestamp_ms: 1,
                }),
            }],
        };
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "bus_event_replay",
            "data": replay,
        }))
        .unwrap();

        handle_json(&payload, &tx, &|| {});

        assert_eq!(tx.event_session_id.load(Ordering::Acquire), 12);
        assert_eq!(tx.last_event_id.load(Ordering::Acquire), 1);
        let notifications = tx.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].show_native);
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
        assert!(!handle_json(payload.as_bytes(), &tx, &|| {}).relist_lcd_images);
        let got = assets_r
            .borrow_and_update()
            .get("acme/logo.png")
            .cloned()
            .expect("asset routed");
        assert_eq!(got, b"PNGDATA");
    }

    #[test]
    fn udev_status_frame_lands_on_the_watch_channel() {
        let (tx, _) = test_tx();
        let mut rx = tx.udev_rules.subscribe();
        let payload = br#"{"type":"udev_rules_status","data":{"supported":true,"current":false,"installed_path":"/usr/lib/udev/rules.d/60-halod.rules","generated_rule_count":43}}"#;
        assert!(!handle_json(payload, &tx, &|| {}).relist_lcd_images);
        let got = rx.borrow_and_update().clone().unwrap();
        assert!(got.supported);
        assert!(!got.current);
        assert_eq!(got.generated_rule_count, 43);
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
            assert!(!handle_json(raw, &tx, &|| {}).relist_lcd_images);
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
        assert!(handle_json(br#"{"type":"image_uploaded"}"#, &tx, &|| {}).relist_lcd_images);
        assert!(rx.borrow().is_none(), "stale progress cleared");
    }

    #[test]
    fn handled_command_error_does_not_create_a_duplicate_generic_toast() {
        let (tx, _rx) = test_tx();
        assert!(
            !handle_json(
                br#"{"type":"error","message":"raw stack trace","handled":true}"#,
                &tx,
                &|| {}
            )
            .relist_lcd_images
        );
        assert!(tx.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn catalog_errors_use_localized_messages_instead_of_backend_text() {
        let (tx, _rx) = test_tx();
        assert!(!handle_json(
            br#"{"type":"error","code":"signature_mismatch","message":"catalog signature does not match its contents"}"#,
            &tx,
            &|| {}
        ).relist_lcd_images);
        let notification = tx.notifications.lock().unwrap().pop().unwrap();
        assert_eq!(
            notification.code,
            NotificationCode::Generic {
                message: t!("plugins.catalog_signature_mismatch").into_owned()
            }
        );
    }

    fn sample_canvas_frame() -> CanvasFrame {
        CanvasFrame {
            frame_id: 1,
            timestamp_ms: 0,
            canvas_srgb_b64: String::new(),
            canvas_w: 1,
            canvas_h: 1,
            led_colors: Vec::new(),
        }
    }

    #[test]
    fn hidden_drops_canvas_frames_before_decode() {
        let (tx, _u) = test_tx();
        let mut canvas_r = tx.canvas_frame.subscribe();
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "canvas_frame",
            "data": sample_canvas_frame(),
        }))
        .unwrap();

        handle_json(&payload, &tx, &|| {});
        assert!(canvas_r.borrow_and_update().is_some());

        tx.canvas_frame.send_replace(None);
        let _ = canvas_r.borrow_and_update();
        tx.hidden.store(true, Ordering::Relaxed);
        handle_json(&payload, &tx, &|| {});
        assert!(canvas_r.borrow().is_none());
    }

    #[test]
    fn frame_sinks_release_retained_buffers() {
        let (tx, mut rx, sinks) = channels(Arc::new(AtomicBool::new(false)));
        tx.plugin_assets
            .send_replace(HashMap::from([("k".to_string(), vec![0u8; 32])]));
        tx.canvas_frame.send_replace(Some(sample_canvas_frame()));
        assert!(rx.plugin_assets.poll() && !rx.plugin_assets.get().is_empty());

        sinks.clear();
        assert!(rx.plugin_assets.poll() && rx.plugin_assets.get().is_empty());
        assert!(rx.canvas_frame.poll() && rx.canvas_frame.get().is_none());
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
    fn synced_adopts_each_change_once_and_keeps_it() {
        let (tx, rx) = watch::channel(0u32);
        let mut synced = Synced::new(rx, "test");
        assert!(!synced.poll());
        tx.send_replace(7);
        assert!(synced.poll());
        assert!(!synced.poll());
        assert_eq!(*synced.get(), 7);
        synced.set(0);
        assert!(!synced.poll());
        assert_eq!(*synced.get(), 0);
    }
}

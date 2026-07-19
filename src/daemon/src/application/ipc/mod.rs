// SPDX-License-Identifier: GPL-3.0-or-later
//! Local IPC application adapter and bounded client delivery.
pub mod router;

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Duration;

use halod_shared::bus::{BusEvent, BusEventReplay, BusSubscribe, BusTransaction};
use halod_shared::frames::{
    decode_binary_payload, decode_header, encode_json_frame, payload_exceeds_max, FRAME_BINARY,
    FRAME_JSON,
};

use crate::application::state::AppState;

/// Bound on a client's outgoing frame queue. When incremental state cannot fit,
/// the bus relay waits for capacity and replaces it with a fresh snapshot.
const CLIENT_QUEUE_CAPACITY: usize = 256;

static NEXT_CLIENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_client_id() -> u64 {
    NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The LCD-preview keepalive lease; `Active` expires to `Expired` once
/// `last_keepalive` is more than `LCD_PREVIEW_LEASE_SECS` old.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeaseState {
    Unsubscribed,
    Active {
        last_keepalive: tokio::time::Instant,
    },
    Expired,
}

/// Which engine topics a client has subscribed to; carried by `ClientState::Subscribed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubscribedTopics {
    pub canvas: bool,
    pub lcd: bool,
}

/// A client's IPC connection lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientState {
    Connected,
    Subscribed(SubscribedTopics),
    Closing,
    Closed,
}

#[derive(Debug, Clone, Copy)]
pub enum SubscriptionTopic {
    Canvas,
    Lcd,
}

struct SubscriptionState {
    client: ClientState,
    canvas_lease: LeaseState,
    lcd_lease: LeaseState,
    canvas_task: bool,
    lcd_task: bool,
}

impl Default for SubscriptionState {
    fn default() -> Self {
        Self {
            client: ClientState::Connected,
            canvas_lease: LeaseState::Unsubscribed,
            lcd_lease: LeaseState::Unsubscribed,
            canvas_task: false,
            lcd_task: false,
        }
    }
}

/// Per-client one-shot subscription flags, shared across clones of a handle so a
/// re-subscribe to the same engine topic doesn't spawn a duplicate forwarder.
pub struct Subscriptions {
    lifecycle: std::sync::Mutex<SubscriptionState>,
    lcd_lease: tokio::sync::watch::Sender<LeaseState>,
    lcd_preview: tokio::sync::watch::Sender<Option<Arc<Vec<u8>>>>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for Subscriptions {
    fn default() -> Self {
        Self {
            lifecycle: std::sync::Mutex::new(SubscriptionState::default()),
            lcd_lease: tokio::sync::watch::channel(LeaseState::Unsubscribed).0,
            lcd_preview: tokio::sync::watch::channel(None).0,
            tasks: std::sync::Mutex::new(Vec::new()),
        }
    }
}

/// Handle through which the daemon sends frames to a connected client.
#[derive(Clone)]
pub struct ClientHandle {
    pub id: u64,
    pub tx: mpsc::Sender<Arc<Vec<u8>>>,
    pub subs: Arc<Subscriptions>,
}

impl ClientHandle {
    pub fn send_json(&self, msg: &Value) {
        let _ = self.try_send_json(msg);
    }

    fn try_send_json(&self, msg: &Value) -> bool {
        let Some(frame) = encode_json_frame(msg) else {
            return false;
        };
        self.try_send_frame(Arc::new(frame))
    }

    fn try_send_frame(&self, frame: Arc<Vec<u8>>) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::debug!("IPC: client {} queue full, dropping frame", self.id);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::debug!("IPC: client {} channel closed, dropping frame", self.id);
                false
            }
        }
    }

    async fn send_json_when_ready(&self, msg: &Value) -> bool {
        let Some(frame) = encode_json_frame(msg) else {
            return false;
        };
        matches!(
            tokio::time::timeout(Duration::from_secs(10), self.tx.send(Arc::new(frame))).await,
            Ok(Ok(()))
        )
    }

    /// Publish an LCD preview frame into the latest-wins slot, replacing any
    /// frame the writer hasn't sent yet.
    pub fn send_lcd_preview(&self, frame: Arc<Vec<u8>>) {
        self.subs.lcd_preview.send_replace(Some(frame));
    }

    /// Claim the canvas subscription; returns `true` only on the first call.
    pub fn try_subscribe_canvas(&self) -> bool {
        let mut lifecycle = self.subs.lifecycle.lock().unwrap();
        if lifecycle.canvas_task
            || matches!(lifecycle.client, ClientState::Closing | ClientState::Closed)
        {
            return false;
        }
        lifecycle.canvas_task = true;
        lifecycle.canvas_lease = LeaseState::Active {
            last_keepalive: tokio::time::Instant::now(),
        };
        true
    }

    /// Claim the LCD-engine subscription; returns `true` only on the first call.
    pub fn try_subscribe_lcd(&self) -> bool {
        let mut lifecycle = self.subs.lifecycle.lock().unwrap();
        if lifecycle.lcd_task
            || matches!(lifecycle.client, ClientState::Closing | ClientState::Closed)
        {
            return false;
        }
        lifecycle.lcd_task = true;
        true
    }

    /// Renew the LCD-preview lease (called on every `LcdEngineSubscribe` receipt).
    pub fn touch_lcd_preview(&self) {
        let active = LeaseState::Active {
            last_keepalive: tokio::time::Instant::now(),
        };
        let mut lifecycle = self.subs.lifecycle.lock().unwrap();
        if matches!(lifecycle.client, ClientState::Closing | ClientState::Closed) {
            return;
        }
        lifecycle.lcd_lease = active;
        drop(lifecycle);
        self.subs.lcd_lease.send_replace(active);
    }

    /// Subscribe to this client's LCD-preview lease state changes.
    pub fn lcd_lease_rx(&self) -> tokio::sync::watch::Receiver<LeaseState> {
        self.subs.lcd_lease.subscribe()
    }

    /// Transition an `Active` lease to `Expired`; a no-op if already expired/unsubscribed or renewed since.
    pub(crate) fn expire_lcd_lease(&self) {
        let mut lifecycle = self.subs.lifecycle.lock().unwrap();
        if matches!(lifecycle.lcd_lease, LeaseState::Active { .. }) {
            lifecycle.lcd_lease = LeaseState::Expired;
        }
        drop(lifecycle);
        let _ = self.subs.lcd_lease.send_if_modified(|s| {
            if matches!(s, LeaseState::Active { .. }) {
                *s = LeaseState::Expired;
                true
            } else {
                false
            }
        });
    }

    /// Register a spawned engine-subscription forwarder as owned by this client, advancing its state to `Subscribed`.
    pub fn track_subscription(&self, topic: SubscriptionTopic, task: tokio::task::JoinHandle<()>) {
        let mut lifecycle = self.subs.lifecycle.lock().unwrap();
        if matches!(lifecycle.client, ClientState::Closing | ClientState::Closed) {
            task.abort();
            return;
        }
        let mut topics = match lifecycle.client {
            ClientState::Subscribed(t) => t,
            _ => SubscribedTopics::default(),
        };
        match topic {
            SubscriptionTopic::Canvas => topics.canvas = true,
            SubscriptionTopic::Lcd => topics.lcd = true,
        }
        lifecycle.client = ClientState::Subscribed(topics);
        self.subs.tasks.lock().unwrap().push(task);
    }

    fn track_task(&self, task: tokio::task::JoinHandle<()>) {
        self.subs.tasks.lock().unwrap().push(task);
    }

    #[cfg(test)]
    pub fn state(&self) -> ClientState {
        self.subs.lifecycle.lock().unwrap().client.clone()
    }

    /// Abort every tracked subscription task and transition to `Closed`; called once at client teardown.
    pub async fn close(&self) {
        {
            let mut lifecycle = self.subs.lifecycle.lock().unwrap();
            lifecycle.client = ClientState::Closing;
            lifecycle.canvas_lease = LeaseState::Expired;
            lifecycle.lcd_lease = LeaseState::Expired;
        }
        self.subs.lcd_lease.send_replace(LeaseState::Expired);
        let tasks: Vec<_> = self.subs.tasks.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        self.subs.lifecycle.lock().unwrap().client = ClientState::Closed;
    }
}

fn deregister(clients: &mut Vec<ClientHandle>, id: u64) {
    clients.retain(|c| c.id != id);
}

/// Security descriptor for the Windows IPC named pipe.
///
/// Grants access to SYSTEM, Administrators, and the current user's SID only
/// (so other logged-in users can't reach the command channel), and sets the
/// pipe's integrity label to Medium so the non-elevated UI can connect to an
/// elevated daemon.
#[cfg(windows)]
struct PipeSecurity {
    descriptor: windows::Win32::Security::PSECURITY_DESCRIPTOR,
    attributes: windows::Win32::Security::SECURITY_ATTRIBUTES,
}

/// SID of the user this process runs as, in SDDL string form (`S-1-5-21-…`).
/// Used to scope the pipe DACL to the current user. The daemon and the UI run
/// as the same interactive user, so this principal covers both.
#[cfg(windows)]
fn current_user_sid() -> Result<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard Win32 token-query sequence; the token handle and the
    // LocalAlloc'd SID string are both released before returning.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| anyhow::anyhow!("OpenProcessToken failed: {e}"))?;

        // First call sizes the buffer — it fails with ERROR_INSUFFICIENT_BUFFER
        // while writing the required length into `needed`.
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        let info = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        info.map_err(|e| anyhow::anyhow!("GetTokenInformation(TokenUser) failed: {e}"))?;

        // `buf` holds a TOKEN_USER whose `User.Sid` points back into `buf`.
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str)
            .map_err(|e| anyhow::anyhow!("ConvertSidToStringSidW failed: {e}"))?;
        let result = sid_str.to_string();
        let _ = LocalFree(Some(HLOCAL(sid_str.0 as *mut std::ffi::c_void)));
        result.map_err(|e| anyhow::anyhow!("SID string not valid UTF-16: {e}"))
    }
}

// SAFETY: after construction the descriptor is immutable — it is only ever read
// (by pointer) when the kernel copies it during pipe creation — so moving the
// owning `PipeSecurity` between threads is sound.
#[cfg(windows)]
unsafe impl Send for PipeSecurity {}

/// Builds the SDDL string granting pipe access to SYSTEM, Administrators, and
/// `user_sid` only, with a Medium integrity label. Pure string formatting —
/// kept separate from `PipeSecurity::new` so the ACL text is testable without
/// the Windows security APIs.
#[cfg(any(windows, test))]
fn pipe_security_sddl(user_sid: &str) -> String {
    format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})S:(ML;;NW;;;ME)")
}

#[cfg(windows)]
impl PipeSecurity {
    fn new() -> Result<Self> {
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        let user_sid = current_user_sid()?;
        let sddl = crate::infrastructure::platform::win32::wide(&pipe_security_sddl(&user_sid));

        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` receives a freshly
        // allocated security descriptor that `Drop` releases via `LocalFree`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| anyhow::anyhow!("failed to build pipe security descriptor: {e}"))?;

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }

    /// Raw pointer for `ServerOptions::create_with_security_attributes_raw`.
    fn as_raw(&self) -> *mut std::ffi::c_void {
        &self.attributes as *const _ as *mut std::ffi::c_void
    }
}

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        if !self.descriptor.0.is_null() {
            // SAFETY: `descriptor` was allocated by the Convert* call in `new`.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
            }
        }
    }
}

/// True if a daemon is already accepting connections on `path`.
/// `ECONNREFUSED` (a stale socket file left by a crashed instance) or a missing path means we may safely (re)bind.
#[cfg(unix)]
fn daemon_already_listening(path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Refuse to start when a live daemon already owns the socket.
#[cfg(unix)]
pub fn ensure_single_instance() -> Result<()> {
    let path = socket_path();
    if daemon_already_listening(&path) {
        anyhow::bail!(
            "another halod instance is already running (socket {}); refusing to start",
            path.display()
        );
    }
    Ok(())
}

/// Refuse to start when another daemon already holds the machine-global
/// single-instance mutex.
///
/// The named-pipe `first_pipe_instance` flag in [`serve`] is only observed after
/// device discovery has already opened the HID handles, so a second daemon would
/// fight the first over the hardware for its whole startup before noticing. This
/// runs *before* any hardware access and bails immediately. The mutex is
/// `Global\` so a session-0 service and a user-session (or dev) daemon collide,
/// and is held for the entire process — the OS releases it only on exit, after
/// the first daemon has finished closing its devices.
#[cfg(windows)]
pub fn ensure_single_instance() -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let name = crate::infrastructure::platform::win32::wide(r"Global\HalodDaemonSingleInstance");
    // SAFETY: `name` is NUL-terminated. On success the handle is intentionally
    // never closed so the mutex stays held for the process lifetime (the OS
    // releases it on exit); on the already-exists path it is closed before bailing.
    unsafe {
        let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))
            .map_err(|e| anyhow::anyhow!("CreateMutexW failed: {e}"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            anyhow::bail!("another halod instance is already running; refusing to start");
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn serve(app: Arc<AppState>) -> tokio::task::JoinHandle<Result<()>> {
    let (dir, is_fallback) = halod_shared::socket::runtime_dir();
    serve_at(app, dir, is_fallback)
}

#[cfg(unix)]
fn serve_at(
    app: Arc<AppState>,
    dir: std::path::PathBuf,
    is_fallback: bool,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        use halod_shared::socket::{current_uid, socket_path_in};
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use tokio::net::UnixListener;

        if is_fallback {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
        }

        let path = socket_path_in(&dir);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // Belt-and-suspenders against umask/TOCTOU: even inside a private dir,
        // make the socket node itself owner-only.
        std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        log::info!("Listening on {}", path.display());

        let our_uid = current_uid();
        loop {
            let (stream, _) = listener.accept().await?;
            // Reject non-owner peers
            match stream.peer_cred() {
                Ok(cred) if is_owning_peer(cred.uid(), our_uid) => {}
                Ok(cred) => {
                    log::warn!(
                        "IPC: rejecting connection from uid {} (expected {our_uid})",
                        cred.uid()
                    );
                    continue;
                }
                Err(e) => {
                    log::warn!("IPC: could not read peer credentials, rejecting: {e}");
                    continue;
                }
            }
            spawn_client(stream, app.clone());
        }
    })
}

/// True if a connecting peer's UID matches the daemon's own UID.
#[cfg(unix)]
fn is_owning_peer(peer_uid: u32, our_uid: u32) -> bool {
    peer_uid == our_uid
}

#[cfg(windows)]
pub fn serve(app: Arc<AppState>) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        use tokio::net::windows::named_pipe::ServerOptions;
        let path = socket_path();

        // Explicit security descriptor so the non-elevated UI can connect even
        // if process integrity differs (see `PipeSecurity`). Built once and
        // kept alive for the whole accept loop.
        let security = PipeSecurity::new()?;

        // `first_pipe_instance` makes this fail if another server already
        // owns the pipe name, catching a duplicate daemon.
        // SAFETY: `security` outlives every `create_*` call in this loop.
        let mut daemon = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(&path, security.as_raw())?
        };
        log::info!("Listening on {path}");
        loop {
            // Block until a client opens the current pipe instance.
            daemon.connect().await?;
            let connected = daemon;
            // Pre-create the next instance so a further client can connect
            // while this one is being served.
            // SAFETY: `security` outlives this call.
            daemon = unsafe {
                ServerOptions::new()
                    .create_with_security_attributes_raw(&path, security.as_raw())?
            };
            spawn_client(connected, app.clone());
        }
    })
}

/// Spawn the per-client handler, logging a clean disconnect. Shared tail of both
/// platform `serve` loops so the accept→handle wiring lives in one place.
fn spawn_client<S>(stream: S, app: Arc<AppState>)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = handle_client(stream, app).await {
            log::debug!("Client disconnected: {e}");
        }
    });
}

async fn handle_client<S>(mut stream: S, app: Arc<AppState>) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
{
    let subscription = read_bus_subscription(&mut stream).await?;
    let (write_tx, mut write_rx) = mpsc::channel::<Arc<Vec<u8>>>(CLIENT_QUEUE_CAPACITY);
    let handle = ClientHandle {
        id: next_client_id(),
        tx: write_tx,
        subs: Arc::default(),
    };

    // Subscribe before taking the initial snapshots. Transactions committed in
    // between are either represented by the snapshot or queued on the receiver;
    // the GUI's revision/event cursors make the overlap idempotent.
    let transactions = app.data_bus.subscribe_transactions();
    let events = app.data_bus.subscribe_events();
    send_bus_snapshot(&app, &handle, &subscription.prefixes);
    send_event_replay(
        &app,
        &handle,
        subscription.last_event_id,
        subscription.event_session_id,
    );
    let bus_task = tokio::spawn(bus_forward_loop(
        app.clone(),
        handle.clone(),
        subscription.prefixes,
        transactions,
        events,
    ));
    handle.track_task(bus_task);

    {
        let mut clients = app.clients.lock().await;
        clients.push(handle.clone());
    }

    // tokio::io::split so a stalled writer can't block reads
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut preview_rx = handle.subs.lcd_preview.subscribe();
    let writer = tokio::spawn(async move {
        loop {
            // `biased` keeps command replies and state ahead of previews; the
            // latest-wins preview slot absorbs whatever the writer can't keep
            // up with.
            let frame = tokio::select! {
                biased;
                f = write_rx.recv() => match f {
                    Some(f) => f,
                    None => break,
                },
                c = preview_rx.changed() => match c {
                    Ok(()) => match preview_rx.borrow_and_update().clone() {
                        Some(f) => f,
                        None => continue,
                    },
                    Err(_) => break,
                },
            };
            if write_half.write_all(&frame).await.is_err() || write_half.flush().await.is_err() {
                break;
            }
        }
    });

    let result = run_client_loop(read_half, &handle, app.clone()).await;

    {
        let mut clients = app.clients.lock().await;
        deregister(&mut clients, handle.id);
    }
    handle.close().await;
    writer.abort();

    result
}

async fn read_bus_subscription<S>(stream: &mut S) -> Result<BusSubscribe>
where
    S: AsyncReadExt + Unpin,
{
    let mut header = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut header))
        .await
        .map_err(|_| anyhow::anyhow!("client did not subscribe to the state bus"))??;
    let (frame_type, payload_len) = decode_header(&header);
    anyhow::ensure!(frame_type == FRAME_JSON, "bus subscription must be JSON");
    anyhow::ensure!(
        !payload_exceeds_max(payload_len),
        "oversized bus subscription"
    );
    let mut payload = vec![0u8; payload_len as usize];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    let value: Value = serde_json::from_slice(&payload)?;
    anyhow::ensure!(
        value.get("type").and_then(Value::as_str) == Some("bus_subscribe"),
        "first client frame must be a bus subscription"
    );
    Ok(serde_json::from_value(value)?)
}

/// Pairs JSON command frames with their accompanying binary payload frames for
/// commands that carry a binary body (currently `set_screen_image`).
///
/// The GUI sends two frames per such command: a JSON frame with metadata and a
/// binary frame with the raw image bytes. Either may arrive first; the assembler
/// buffers whichever arrives first until its counterpart appears, then returns
/// the fully-assembled message with the binary data base64-encoded into
/// `data_b64`.
pub(crate) struct ClientFrameAssembler {
    pending_commands: std::collections::HashMap<String, Value>,
    pending_binary: std::collections::HashMap<String, Vec<u8>>,
}

impl ClientFrameAssembler {
    const MAX_PENDING: usize = 8;

    pub(crate) fn new() -> Self {
        Self {
            pending_commands: std::collections::HashMap::new(),
            pending_binary: std::collections::HashMap::new(),
        }
    }

    /// Feed a raw frame (identified by `frame_type`) to the assembler.
    ///
    /// Returns `Some(msg)` when a complete, ready-to-dispatch message is
    /// available:
    /// - For ordinary JSON frames (not `set_screen_image`): the parsed `Value`
    ///   is returned immediately.
    /// - For `set_screen_image` JSON + binary pairs: the message is returned
    ///   only once both halves have arrived, with `data_b64` injected.
    ///
    /// Returns `None` when the frame was buffered awaiting its partner, when it
    /// was dropped due to overflow, or when parsing failed.
    pub(crate) fn process_frame(
        &mut self,
        frame_type: u8,
        payload: &[u8],
        client_id: u64,
    ) -> Option<Value> {
        use base64::Engine as _;

        if frame_type == FRAME_JSON {
            match serde_json::from_slice::<Value>(payload) {
                Ok(msg) => {
                    let cmd = msg["type"].as_str().unwrap_or("");
                    let req_id = msg["request_id"].as_str().unwrap_or("").to_string();
                    if cmd == "set_screen_image" && !req_id.is_empty() {
                        if let Some(data) = self.pending_binary.remove(&req_id) {
                            let mut msg = msg;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            msg["data_b64"] = json!(b64);
                            Some(msg)
                        } else if self.pending_commands.len() < Self::MAX_PENDING {
                            if self.pending_commands.contains_key(&req_id) {
                                log::warn!(
                                    "IPC: duplicate req_id {req_id} for set_screen_image, dropping earlier command"
                                );
                            }
                            self.pending_commands.insert(req_id, msg);
                            None
                        } else {
                            log::warn!(
                                "IPC: too many unpaired set_screen_image commands, dropping"
                            );
                            None
                        }
                    } else {
                        Some(msg)
                    }
                }
                Err(_) => {
                    log::warn!(
                        "IPC: client {client_id} sent malformed JSON frame ({} bytes), dropping",
                        payload.len()
                    );
                    None
                }
            }
        } else if frame_type == FRAME_BINARY {
            if let Some((req_id, _content_type, data)) = decode_binary_payload(payload) {
                if let Some(mut pending_msg) = self.pending_commands.remove(&req_id) {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                    pending_msg["data_b64"] = json!(b64);
                    Some(pending_msg)
                } else if self.pending_binary.len() < Self::MAX_PENDING {
                    self.pending_binary.insert(req_id, data);
                    None
                } else {
                    log::warn!("IPC: too many unpaired binary payloads, dropping");
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }
}

async fn run_client_loop<S>(mut stream: S, handle: &ClientHandle, app: Arc<AppState>) -> Result<()>
where
    S: AsyncReadExt + Unpin,
{
    let mut assembler = ClientFrameAssembler::new();
    let mut header = [0u8; 5];

    const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

    loop {
        tokio::time::timeout(READ_IDLE_TIMEOUT, stream.read_exact(&mut header))
            .await
            .map_err(|_| anyhow::anyhow!("client idle timeout"))?
            .map_err(anyhow::Error::from)?;
        let (frame_type, payload_len) = decode_header(&header);
        if payload_exceeds_max(payload_len) {
            return Err(anyhow::anyhow!(
                "client sent oversized frame: {payload_len} bytes"
            ));
        }
        let mut payload = vec![0u8; payload_len as usize];
        if payload_len > 0 {
            tokio::time::timeout(READ_IDLE_TIMEOUT, stream.read_exact(&mut payload))
                .await
                .map_err(|_| anyhow::anyhow!("client idle timeout"))?
                .map_err(anyhow::Error::from)?;
        }
        if let Some(msg) = assembler.process_frame(frame_type, &payload, handle.id) {
            // Device-scoped commands are spawned onto a per-device lock inside
            // `handle_message`, so a slow device never blocks the reader; global
            // commands run inline in read order.
            router::handle_message(msg, handle.clone(), app.clone()).await;
        }
    }
}

fn send_bus_snapshot(app: &Arc<AppState>, handle: &ClientHandle, prefixes: &[String]) {
    let snapshot = app.data_bus.state_snapshot(prefixes);
    handle.send_json(&json!({ "type": "bus_snapshot", "data": snapshot }));
}

fn send_event_replay(
    app: &Arc<AppState>,
    handle: &ClientHandle,
    cursor: Option<u64>,
    client_session_id: Option<u64>,
) {
    let current = app.data_bus.replay_events(None);
    let cursor = (client_session_id == Some(current.session_id))
        .then_some(cursor)
        .flatten();
    let replay: BusEventReplay = app.data_bus.replay_events(cursor);
    handle.send_json(&json!({ "type": "bus_event_replay", "data": replay }));
}

async fn bus_forward_loop(
    app: Arc<AppState>,
    client: ClientHandle,
    prefixes: Vec<String>,
    mut transactions: broadcast::Receiver<BusTransaction>,
    mut events: broadcast::Receiver<BusEvent>,
) {
    loop {
        tokio::select! {
            transaction = transactions.recv() => match transaction {
                Ok(transaction) => {
                    let filtered = filter_transaction(transaction, &prefixes);
                    if !filtered.upserts.is_empty() || !filtered.tombstones.is_empty() {
                        let frame = json!({ "type": "bus_transaction", "data": filtered });
                        if !client.try_send_json(&frame)
                            && !send_bus_snapshot_when_ready(&app, &client, &prefixes).await
                        {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_bus_snapshot_when_ready(&app, &client, &prefixes).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    let frame = json!({ "type": "bus_event", "data": event });
                    if !client.try_send_json(&frame)
                        && !send_event_replay_when_ready(&app, &client).await
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_event_replay_when_ready(&app, &client).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
        if client.tx.is_closed() {
            return;
        }
    }
}

async fn send_bus_snapshot_when_ready(
    app: &Arc<AppState>,
    client: &ClientHandle,
    prefixes: &[String],
) -> bool {
    let snapshot = app.data_bus.state_snapshot(prefixes);
    client
        .send_json_when_ready(&json!({ "type": "bus_snapshot", "data": snapshot }))
        .await
}

async fn send_event_replay_when_ready(app: &Arc<AppState>, client: &ClientHandle) -> bool {
    let replay = app.data_bus.replay_events(None);
    client
        .send_json_when_ready(&json!({ "type": "bus_event_replay", "data": replay }))
        .await
}

fn filter_transaction(mut transaction: BusTransaction, prefixes: &[String]) -> BusTransaction {
    transaction
        .upserts
        .retain(|record| halod_shared::bus::matches_prefixes(&record.key, prefixes));
    transaction
        .tombstones
        .retain(|key| halod_shared::bus::matches_prefixes(key, prefixes));
    transaction
}

/// Path to the IPC command channel. Shared with the UI via `halod-shared`
/// so the two can never drift on the path scheme.
pub use halod_shared::socket::socket_path;

#[cfg(unix)]
#[cfg(test)]
mod peer_uid_tests {
    use super::is_owning_peer;

    #[test]
    fn rejects_different_uid() {
        assert!(!is_owning_peer(1001, 1000));
    }

    #[test]
    fn accepts_matching_uid() {
        assert!(is_owning_peer(1000, 1000));
    }
}

#[cfg(test)]
mod pipe_security_tests {
    use super::pipe_security_sddl;

    /// Only SYSTEM, Administrators, and the given SID get access; a different
    /// user's SID must not appear in another user's ACL string.
    #[test]
    fn sddl_scopes_access_to_requested_sid_only() {
        let sddl_a = pipe_security_sddl("S-1-5-21-1-1-1-1001");
        let sddl_b = pipe_security_sddl("S-1-5-21-2-2-2-1002");

        assert!(sddl_a.contains("S-1-5-21-1-1-1-1001"));
        assert!(!sddl_a.contains("S-1-5-21-2-2-2-1002"));
        assert!(sddl_b.contains("S-1-5-21-2-2-2-1002"));
        assert!(!sddl_b.contains("S-1-5-21-1-1-1-1001"));

        // SYSTEM/Administrators grants are present regardless of the user SID.
        assert!(sddl_a.contains("(A;;GA;;;SY)"));
        assert!(sddl_a.contains("(A;;GA;;;BA)"));
        // Medium integrity label, so the non-elevated UI can connect.
        assert!(sddl_a.contains("S:(ML;;NW;;;ME)"));
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;

    struct SlowSnapshotDevice {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::infrastructure::drivers::Device for SlowSnapshotDevice {
        fn id(&self) -> &str {
            "slow-snapshot"
        }

        fn name(&self) -> &str {
            "Slow snapshot"
        }

        fn vendor(&self) -> &str {
            "test"
        }

        fn model(&self) -> &str {
            "test"
        }

        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn close(&self) {}

        async fn serialize(&self) -> halod_shared::types::WireDevice {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            halod_shared::types::WireDevice {
                id: self.id().to_owned(),
                name: self.name().to_owned(),
                vendor: self.vendor().to_owned(),
                model: self.model().to_owned(),
                connected: true,
                ..Default::default()
            }
        }

        fn capabilities(&self) -> Vec<crate::infrastructure::drivers::CapabilityRef<'_>> {
            Vec::new()
        }
    }

    #[test]
    fn try_subscribe_canvas_first_call_returns_true() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        assert!(handle.try_subscribe_canvas());
    }

    #[test]
    fn try_subscribe_canvas_second_call_returns_false() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        handle.try_subscribe_canvas();
        assert!(!handle.try_subscribe_canvas());
    }

    #[test]
    fn try_subscribe_lcd_first_call_returns_true() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        assert!(handle.try_subscribe_lcd());
    }

    #[test]
    fn try_subscribe_lcd_second_call_returns_false() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        handle.try_subscribe_lcd();
        assert!(!handle.try_subscribe_lcd());
    }

    #[test]
    fn canvas_and_lcd_subscription_flags_are_independent() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        assert!(handle.try_subscribe_canvas());
        assert!(handle.try_subscribe_lcd(), "lcd unaffected by canvas claim");
        assert!(!handle.try_subscribe_canvas());
        assert!(!handle.try_subscribe_lcd());
    }

    #[tokio::test]
    async fn deregister_removes_client_even_with_live_receiver() {
        // Receivers kept alive => `tx.is_closed()` is false; identity removal
        // must still drop the right client (the old `retain(!is_closed)` did not).
        let (tx_a, _rx_a) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let (tx_b, _rx_b) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let mut clients = vec![
            ClientHandle {
                id: 1,
                tx: tx_a,
                subs: Arc::default(),
            },
            ClientHandle {
                id: 2,
                tx: tx_b,
                subs: Arc::default(),
            },
        ];
        deregister(&mut clients, 1);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, 2);
    }

    #[tokio::test]
    async fn run_client_loop_reads_frame_and_queues_reply_to_writer() {
        use crate::application::state::AppState;
        use crate::config::Config;

        // The read loop is now decoupled from the writer: a frame read off the
        // stream is dispatched and its reply is queued on the client's tx (which
        // the writer task drains). Drive a `ping` through and expect a `pong`.
        let (tx, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        let app = Arc::new(AppState::new(Config::default()));

        let (read_half, mut feed) = tokio::io::duplex(1024);
        let ping_frame = encode_json_frame(&json!({"type": "ping"})).unwrap();
        feed.write_all(&ping_frame).await.unwrap();
        drop(feed); // EOF ends the loop after the frame is processed.

        let _ = run_client_loop(read_half, &handle, app).await;

        let frame = rx.try_recv().expect("a reply frame must be queued");
        let header: [u8; 5] = frame[..5].try_into().unwrap();
        let (_ft, len) = decode_header(&header);
        let reply: Value = serde_json::from_slice(&frame[5..5 + len as usize]).unwrap();
        assert_eq!(reply["type"], "pong");
    }

    #[tokio::test]
    async fn bus_subscription_is_the_first_typed_client_frame() {
        let (mut read_half, mut feed) = tokio::io::duplex(1024);
        let frame = encode_json_frame(&json!({
            "type": "bus_subscribe",
            "prefixes": ["runtime.devices."],
            "last_event_id": 41
        }))
        .unwrap();
        feed.write_all(&frame).await.unwrap();

        let subscription = read_bus_subscription(&mut read_half).await.unwrap();
        assert_eq!(subscription.prefixes, vec!["runtime.devices."]);
        assert_eq!(subscription.last_event_id, Some(41));
    }

    #[tokio::test]
    async fn ordinary_command_cannot_bypass_bus_subscription() {
        let (mut read_half, mut feed) = tokio::io::duplex(1024);
        let frame = encode_json_frame(&json!({"type": "ping"})).unwrap();
        feed.write_all(&frame).await.unwrap();

        let error = read_bus_subscription(&mut read_half).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("first client frame must be a bus subscription"));
    }

    #[tokio::test]
    async fn lcd_preview_slot_keeps_only_the_newest_frame() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        let mut rx = handle.subs.lcd_preview.subscribe();
        handle.send_lcd_preview(Arc::new(vec![1]));
        handle.send_lcd_preview(Arc::new(vec![2]));
        handle.send_lcd_preview(Arc::new(vec![3]));
        assert_eq!(
            *rx.borrow_and_update().clone().expect("frame present"),
            vec![3],
            "a stalled writer must see only the newest preview frame"
        );
    }

    #[tokio::test]
    async fn send_json_drops_when_queue_full_instead_of_growing() {
        let (tx, rx) = mpsc::channel::<Arc<Vec<u8>>>(1);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        handle.send_json(&json!({"n": 1}));
        handle.send_json(&json!({"n": 2}));
        handle.send_json(&json!({"n": 3}));
        assert_eq!(
            rx.len(),
            1,
            "a stalled client's queue must not grow past capacity"
        );
    }

    #[tokio::test]
    async fn full_client_queue_replaces_incremental_state_with_snapshot() {
        use crate::application::state::AppState;
        use crate::config::Config;

        let app = Arc::new(AppState::new(Config::default()));
        let transactions = app.data_bus.subscribe_transactions();
        let events = app.data_bus.subscribe_events();
        let (tx, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(1);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        client.send_json(&json!({"type": "already_queued"}));

        let relay = tokio::spawn(bus_forward_loop(
            app.clone(),
            client,
            Vec::new(),
            transactions,
            events,
        ));
        crate::application::usecases::registry::runtime::gui_changed(&app).await;
        tokio::task::yield_now().await;

        let queued = rx.recv().await.unwrap();
        let queued: Value = serde_json::from_slice(&queued[5..]).unwrap();
        assert_eq!(queued["type"], "already_queued");

        let recovered = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let recovered: Value = serde_json::from_slice(&recovered[5..]).unwrap();
        assert_eq!(recovered["type"], "bus_snapshot");
        assert!(recovered["data"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["key"] == halod_shared::bus::topic::GUI));
        relay.abort();
    }

    #[tokio::test]
    async fn concurrent_topic_commits_do_not_overlap_device_passes() {
        use crate::application::state::AppState;
        use crate::config::Config;

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(Arc::new(SlowSnapshotDevice {
                entered: entered.clone(),
                release: release.clone(),
                calls: calls.clone(),
            }));

        let first = tokio::spawn({
            let app = app.clone();
            async move { crate::application::usecases::registry::runtime::topology_changed(&app).await }
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("first snapshot did not start");

        let second = tokio::spawn({
            let app = app.clone();
            async move { crate::application::usecases::registry::runtime::topology_changed(&app).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second pass must wait behind the first, never overlap on the device"
        );

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("second snapshot did not start after the first released");
        release.notify_one();
        for task in [first, second] {
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("snapshot did not finish")
                .expect("snapshot task panicked");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn client_state_starts_connected() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        assert_eq!(handle.state(), ClientState::Connected);
    }

    #[tokio::test]
    async fn tracking_a_subscription_advances_to_subscribed_with_that_topic() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        handle.track_subscription(SubscriptionTopic::Canvas, tokio::spawn(async {}));
        assert_eq!(
            handle.state(),
            ClientState::Subscribed(SubscribedTopics {
                canvas: true,
                lcd: false
            })
        );
        handle.track_subscription(SubscriptionTopic::Lcd, tokio::spawn(async {}));
        assert_eq!(
            handle.state(),
            ClientState::Subscribed(SubscribedTopics {
                canvas: true,
                lcd: true
            })
        );
    }

    #[tokio::test]
    async fn close_aborts_tracked_subscription_tasks_and_ends_closed() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        handle.track_subscription(SubscriptionTopic::Canvas, task);

        handle.close().await;

        assert_eq!(handle.state(), ClientState::Closed);
    }

    #[test]
    fn touch_lcd_preview_sets_an_active_lease() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        assert_eq!(*handle.lcd_lease_rx().borrow(), LeaseState::Unsubscribed);
        handle.touch_lcd_preview();
        assert!(matches!(
            *handle.lcd_lease_rx().borrow(),
            LeaseState::Active { .. }
        ));
    }

    #[test]
    fn expire_lcd_lease_is_a_noop_when_not_active() {
        let (tx, _rx) = mpsc::channel::<Arc<Vec<u8>>>(4);
        let handle = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        handle.expire_lcd_lease();
        assert_eq!(*handle.lcd_lease_rx().borrow(), LeaseState::Unsubscribed);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use crate::application::state::AppState;
    use crate::config::Config;
    use std::os::unix::fs::PermissionsExt;

    /// A live listener on the socket path must be detected (so a second daemon
    /// refuses to start), while a missing or stale path must read as free.
    #[tokio::test]
    async fn detects_live_socket_owner_but_not_stale_or_missing() {
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("halod.sock");
        // Missing path: free.
        assert!(!daemon_already_listening(&path));

        // Live owner: detected.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(daemon_already_listening(&path));

        // Socket node left behind with no listener (crashed instance): the file
        // still exists but `connect` is refused, so it reads as free.
        drop(listener);
        assert!(path.exists(), "std UnixListener leaves the node on drop");
        assert!(!daemon_already_listening(&path));
    }

    /// Binding in a fallback location (`XDG_RUNTIME_DIR` unset) must create a
    /// private `0700` directory and a `0600` socket, so other local users
    /// cannot reach the command channel.
    #[tokio::test]
    async fn serve_locks_down_fallback_dir_and_socket() {
        let tmp = tempfile::tempdir().unwrap();

        let dir = tmp.path().join("halod-test");
        let path = halod_shared::socket::socket_path_in(&dir);

        let app = Arc::new(AppState::new(Config::default()));
        let handle = serve_at(app, dir.clone(), true);

        // serve() binds inside its spawned task; wait for the socket to appear.
        let mut waited = 0;
        while !std::path::Path::new(&path).exists() && waited < 200 {
            if handle.is_finished() {
                panic!("IPC server exited before binding: {:?}", handle.await);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert!(std::path::Path::new(&path).exists(), "socket not created");

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        handle.abort();

        assert_eq!(dir_mode, 0o700, "fallback dir must be private");
        assert_eq!(sock_mode, 0o600, "socket must be owner-only");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_well_formed() {
        let sid = current_user_sid().expect("current user SID");
        assert!(sid.starts_with("S-1-"), "unexpected SID form: {sid}");
    }

    #[test]
    fn pipe_security_builds_a_descriptor() {
        // The SDDL (with the interpolated user SID) must parse and yield a
        // non-null descriptor and a usable raw security-attributes pointer.
        let security = PipeSecurity::new().expect("pipe security descriptor");
        assert!(!security.descriptor.0.is_null());
        assert!(!security.as_raw().is_null());
    }
}

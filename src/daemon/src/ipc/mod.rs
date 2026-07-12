// SPDX-License-Identifier: GPL-3.0-or-later
pub mod router;
pub mod serializer;

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

use halod_shared::frames::{
    decode_binary_payload, decode_header, encode_json_frame, payload_exceeds_max, FRAME_BINARY,
    FRAME_JSON,
};

use crate::state::AppState;

/// Bound on a client's outgoing frame queue. A client that stalls (socket
/// buffer full) drops frames past this point rather than growing unbounded;
/// state is idempotent, so the next broadcast supersedes any dropped frame.
const CLIENT_QUEUE_CAPACITY: usize = 256;

static NEXT_CLIENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_client_id() -> u64 {
    NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Per-client one-shot subscription flags, shared across clones of a handle so a
/// re-subscribe to the same engine topic doesn't spawn a duplicate forwarder.
pub struct Subscriptions {
    canvas: std::sync::atomic::AtomicBool,
    lcd: std::sync::atomic::AtomicBool,
    lcd_keepalive: tokio::sync::watch::Sender<tokio::time::Instant>,
    lcd_preview: tokio::sync::watch::Sender<Option<Arc<Vec<u8>>>>,
}

impl Default for Subscriptions {
    fn default() -> Self {
        Self {
            canvas: std::sync::atomic::AtomicBool::new(false),
            lcd: std::sync::atomic::AtomicBool::new(false),
            lcd_keepalive: tokio::sync::watch::channel(tokio::time::Instant::now()).0,
            lcd_preview: tokio::sync::watch::channel(None).0,
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
        let Some(frame) = encode_json_frame(msg) else {
            return;
        };
        self.send_frame(Arc::new(frame));
    }

    /// Queue an already-encoded wire frame, shared so broadcast fan-out costs
    /// one `Arc` clone per client instead of a re-serialization.
    pub fn send_frame(&self, frame: Arc<Vec<u8>>) {
        match self.tx.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::debug!("IPC: client {} queue full, dropping frame", self.id);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::debug!("IPC: client {} channel closed, dropping frame", self.id);
            }
        }
    }

    /// Publish an LCD preview frame into the latest-wins slot, replacing any
    /// frame the writer hasn't sent yet.
    pub fn send_lcd_preview(&self, frame: Arc<Vec<u8>>) {
        self.subs.lcd_preview.send_replace(Some(frame));
    }

    /// Claim the canvas subscription; returns `true` only on the first call.
    pub fn try_subscribe_canvas(&self) -> bool {
        !self
            .subs
            .canvas
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Claim the LCD-engine subscription; returns `true` only on the first call.
    pub fn try_subscribe_lcd(&self) -> bool {
        !self
            .subs
            .lcd
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Renew the LCD-preview lease (called on every `LcdEngineSubscribe` receipt).
    pub fn touch_lcd_preview(&self) {
        self.subs
            .lcd_keepalive
            .send_replace(tokio::time::Instant::now());
    }

    /// Subscribe to this client's LCD-preview lease renewals.
    pub fn lcd_keepalive_rx(&self) -> tokio::sync::watch::Receiver<tokio::time::Instant> {
        self.subs.lcd_keepalive.subscribe()
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
        let _ = LocalFree(HLOCAL(sid_str.0 as *mut std::ffi::c_void));
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
        let sddl = crate::platform::win32::wide(&pipe_security_sddl(&user_sid));

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
                let _ = LocalFree(HLOCAL(self.descriptor.0));
            }
        }
    }
}

/// True if a daemon is already accepting connections on `path`.
/// `ECONNREFUSED` (a stale socket file left by a crashed instance) or a missing path means we may safely (re)bind.
#[cfg(unix)]
fn daemon_already_listening(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Refuse to start when a live daemon already owns the socket.
#[cfg(unix)]
pub fn ensure_single_instance() -> Result<()> {
    let path = socket_path();
    if daemon_already_listening(&path) {
        anyhow::bail!(
            "another halod instance is already running (socket {path}); refusing to start"
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

    let name = crate::platform::win32::wide(r"Global\HalodDaemonSingleInstance");
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
    tokio::spawn(async move {
        use halod_shared::socket::{current_uid, runtime_dir};
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use tokio::net::UnixListener;

        let (dir, is_fallback) = runtime_dir();
        if is_fallback {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
        }

        let path = socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // Belt-and-suspenders against umask/TOCTOU: even inside a private dir,
        // make the socket node itself owner-only.
        std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        log::info!("Listening on {path}");

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
        // when the daemon runs elevated (see `PipeSecurity`). Built once and
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

async fn handle_client<S>(stream: S, app: Arc<AppState>) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
{
    let (write_tx, mut write_rx) = mpsc::channel::<Arc<Vec<u8>>>(CLIENT_QUEUE_CAPACITY);
    let handle = ClientHandle {
        id: next_client_id(),
        tx: write_tx,
        subs: Arc::default(),
    };

    let state_msg = build_state_msg(&app).await;
    handle.send_json(&state_msg);

    let plugin_updates = app.plugin_update_status.lock().await.clone();
    if !plugin_updates.is_empty() {
        handle.send_json(&serde_json::json!({
            "type": "plugin_updates",
            "plugins": plugin_updates,
        }));
    }

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
    writer.abort();

    result
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

/// Periodic broadcast of device state to all clients.
pub fn broadcast_loop(app: Arc<AppState>) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            broadcast_state(&app).await;
        }
    })
}

pub async fn broadcast_state(app: &Arc<AppState>) {
    let msg = build_state_msg(app).await;
    broadcast_json(app, &msg).await;
}

/// Fan an arbitrary JSON frame out to every connected client (encoded once).
pub async fn broadcast_json(app: &Arc<AppState>, msg: &Value) {
    let Some(frame) = encode_json_frame(msg) else {
        return;
    };
    let frame = Arc::new(frame);
    let clients = app.clients.lock().await;
    for client in clients.iter() {
        client.send_frame(frame.clone());
    }
}

pub async fn build_state_msg(app: &Arc<AppState>) -> Value {
    let cfg = app.config.read().await.clone();
    let mut names: Vec<String> = cfg
        .app_rules
        .iter()
        .flat_map(|r| r.process_names.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    let process_icons = crate::profiles::running_apps::resolve_process_icons(&names);
    let data = serializer::serialize_state(app, cfg, process_icons).await;
    json!({ "type": "state", "data": data })
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
        use crate::config::Config;
        use crate::state::AppState;

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
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use std::os::unix::fs::PermissionsExt;

    /// A live listener on the socket path must be detected (so a second daemon
    /// refuses to start), while a missing or stale path must read as free.
    #[tokio::test]
    async fn detects_live_socket_owner_but_not_stale_or_missing() {
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("halod.sock");
        let path_str = path.to_str().unwrap();

        // Missing path: free.
        assert!(!daemon_already_listening(path_str));

        // Live owner: detected.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(daemon_already_listening(path_str));

        // Socket node left behind with no listener (crashed instance): the file
        // still exists but `connect` is refused, so it reads as free.
        drop(listener);
        assert!(path.exists(), "std UnixListener leaves the node on drop");
        assert!(!daemon_already_listening(path_str));
    }

    /// Binding in a fallback location (`XDG_RUNTIME_DIR` unset) must create a
    /// private `0700` directory and a `0600` socket, so other local users
    /// cannot reach the command channel.
    #[tokio::test]
    async fn serve_locks_down_fallback_dir_and_socket() {
        let tmp = tempfile::tempdir().unwrap();

        // Drive `runtime_dir()` down the fallback branch (XDG unset) while
        // redirecting the temp base via TMPDIR. This is the only test in this
        // crate that touches these env vars; it restores them before returning.
        let prev_xdg = std::env::var_os("XDG_RUNTIME_DIR");
        let prev_tmp = std::env::var_os("TMPDIR");
        // SAFETY: single-threaded test setup, restored below.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("TMPDIR", tmp.path());
        }

        let (dir, is_fallback) = halod_shared::socket::runtime_dir();
        assert!(is_fallback, "expected fallback dir under {:?}", tmp.path());
        let path = socket_path();

        let app = Arc::new(AppState::new(Config::default()));
        let handle = serve(app);

        // serve() binds inside its spawned task; wait for the socket to appear.
        let mut waited = 0;
        while !std::path::Path::new(&path).exists() && waited < 200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert!(std::path::Path::new(&path).exists(), "socket not created");

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        // SAFETY: restore env before asserting so a failure does not leak it.
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prev_tmp {
                Some(v) => std::env::set_var("TMPDIR", v),
                None => std::env::remove_var("TMPDIR"),
            }
        }

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

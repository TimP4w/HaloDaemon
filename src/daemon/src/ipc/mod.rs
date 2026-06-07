pub mod router;
pub mod serializer;

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

use halod_protocol::frames::{
    decode_binary_payload, decode_header, encode_json_frame, FRAME_BINARY, FRAME_JSON,
};

use crate::state::AppState;

/// Handle through which the daemon sends frames to a connected client.
#[derive(Clone)]
pub struct ClientHandle {
    pub tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ClientHandle {
    pub fn send_json(&self, msg: &Value) {
        let frame = encode_json_frame(msg);
        let _ = self.tx.send(frame);
    }
}

/// Security descriptor for the Windows IPC named pipe.
///
/// The daemon may run elevated (high integrity) so it can reach the chipset
/// SMBus via PawnIO. A pipe created by a high-integrity process gets a High
/// mandatory label by default, which blocks the non-elevated UI from
/// connecting. This descriptor grants access to SYSTEM, Administrators and the
/// **current user** (the daemon and UI run as the same interactive user), and
/// sets the pipe's integrity label to Medium so the (medium-integrity) UI can
/// connect regardless of the daemon's elevation state. Medium — rather than
/// Low — is deliberate: it is the lowest label that still admits the UI, while
/// keeping low-integrity (sandboxed) processes such as browser renderers out of
/// the elevated daemon's command channel.
///
/// The DACL is scoped to the current user's SID rather than Authenticated
/// Users so that, on a multi-user host, another logged-in user cannot connect
/// to the command channel and drive the hardware.
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

#[cfg(windows)]
impl PipeSecurity {
    fn new() -> Result<Self> {
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        // D: DACL — generic-all for SYSTEM (SY), Administrators (BA) and the
        //    current user's SID; the latter covers both the elevated daemon
        //    and the non-elevated UI, which run as the same interactive user,
        //    without exposing the channel to other users on the machine.
        // S: SACL — mandatory label at Medium integrity (ME): the UI runs at
        //    Medium, so this is the lowest label that still lets it connect
        //    while blocking lower-integrity (sandboxed) processes.
        let user_sid = current_user_sid()?;
        let sddl: Vec<u16> =
            format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})S:(ML;;NW;;;ME)")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

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

#[cfg(unix)]
pub fn serve(app: Arc<AppState>) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        use tokio::net::UnixListener;
        let path = socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        log::info!("Listening on {path}");
        loop {
            let (stream, _) = listener.accept().await?;
            let app2 = app.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_client(stream, app2).await {
                    log::debug!("Client disconnected: {e}");
                }
            });
        }
    })
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
            let app2 = app.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_client(connected, app2).await {
                    log::debug!("Client disconnected: {e}");
                }
            });
        }
    })
}

async fn handle_client<S>(mut stream: S, app: Arc<AppState>) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
{
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let handle = ClientHandle { tx: write_tx };

    // Send initial full state.
    let state_msg = build_state_msg(app.clone()).await;
    handle.send_json(&state_msg);

    // Register client.
    {
        let mut clients = app.clients.lock().await;
        clients.push(handle.clone());
    }

    let result = run_client_loop(&mut stream, &handle, app.clone(), &mut write_rx).await;

    // Deregister.
    {
        let mut clients = app.clients.lock().await;
        clients.retain(|c| !c.tx.is_closed());
    }

    result
}

async fn run_client_loop<S>(
    stream: &mut S,
    handle: &ClientHandle,
    app: Arc<AppState>,
    write_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use base64::Engine as _;
    use std::collections::HashMap;

    const MAX_PENDING: usize = 8;
    let mut header = [0u8; 5];
    let mut pending_commands: HashMap<String, Value> = HashMap::new();
    let mut pending_binary: HashMap<String, Vec<u8>> = HashMap::new();

    loop {
        tokio::select! {
            // Flush outgoing frames.
            Some(frame) = write_rx.recv() => {
                stream.write_all(&frame).await?;
                stream.flush().await?;
            }
            // Read incoming frame.
            result = stream.read_exact(&mut header) => {
                result?;
                let (frame_type, payload_len) = decode_header(&header);
                let mut payload = vec![0u8; payload_len as usize];
                if payload_len > 0 {
                    stream.read_exact(&mut payload).await?;
                }
                if frame_type == FRAME_JSON {
                    if let Ok(mut msg) = serde_json::from_slice::<Value>(&payload) {
                        let cmd = msg["type"].as_str().unwrap_or("").to_string();
                        let req_id = msg["request_id"].as_str().unwrap_or("").to_string();
                        if cmd == "set_screen_image" && !req_id.is_empty() {
                            if let Some(data) = pending_binary.remove(&req_id) {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                                msg["data_b64"] = serde_json::json!(b64);
                                // Image upload is long (compress + USB pipeline). Spawn it so
                                // the IPC loop stays readable/writable during the operation.
                                let h = handle.clone();
                                let a = app.clone();
                                tokio::spawn(async move {
                                    router::handle_message(msg, h, a).await;
                                });
                            } else if pending_commands.len() < MAX_PENDING {
                                pending_commands.insert(req_id, msg);
                            } else {
                                log::warn!("IPC: too many unpaired set_screen_image commands, dropping");
                            }
                        } else {
                            router::handle_message(msg, handle.clone(), app.clone()).await;
                        }
                    }
                } else if frame_type == FRAME_BINARY {
                    if let Some((req_id, _content_type, data)) = decode_binary_payload(&payload) {
                        if let Some(mut pending_msg) = pending_commands.remove(&req_id) {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            pending_msg["data_b64"] = serde_json::json!(b64);
                            let h = handle.clone();
                            let a = app.clone();
                            tokio::spawn(async move {
                                router::handle_message(pending_msg, h, a).await;
                            });
                        } else if pending_binary.len() < MAX_PENDING {
                            pending_binary.insert(req_id, data);
                        } else {
                            log::warn!("IPC: too many unpaired binary payloads, dropping");
                        }
                    }
                }
            }
        }
    }
}

/// Periodic broadcast of device state to all clients.
pub fn broadcast_loop(app: Arc<AppState>) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            broadcast_state(app.clone()).await;
        }
    })
}

pub async fn broadcast_state(app: Arc<AppState>) {
    let msg = build_state_msg(app.clone()).await;
    let clients = app.clients.lock().await;
    for client in clients.iter() {
        client.send_json(&msg);
    }
}

pub async fn build_state_msg(app: Arc<AppState>) -> Value {
    let data = serializer::serialize_state(app).await;
    json!({ "type": "state", "data": data })
}

#[cfg(unix)]
pub fn socket_path() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    format!("{}/halod.sock", runtime_dir)
}

#[cfg(windows)]
pub fn socket_path() -> String {
    r"\\.\pipe\halod".to_string()
}

#[cfg(not(any(unix, windows)))]
pub fn socket_path() -> String {
    String::new()
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

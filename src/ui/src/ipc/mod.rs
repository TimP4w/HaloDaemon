pub mod client;

use tokio::sync::mpsc;

/// A command sent from the UI to the IPC writer task. Supports both JSON commands
/// and binary payloads (used for LCD image upload).
pub enum IpcCmd {
    Json(serde_json::Value),
    Binary {
        req_id: String,
        content_type: String,
        data: Vec<u8>,
    },
}

#[derive(Clone)]
pub struct IpcSender {
    tx: mpsc::UnboundedSender<IpcCmd>,
}

impl IpcSender {
    pub fn new(tx: mpsc::UnboundedSender<IpcCmd>) -> Self {
        Self { tx }
    }

    pub fn send(&self, cmd: serde_json::Value) {
        let _ = self.tx.send(IpcCmd::Json(cmd));
    }

    pub fn send_binary(&self, req_id: impl Into<String>, content_type: impl Into<String>, data: Vec<u8>) {
        let _ = self.tx.send(IpcCmd::Binary {
            req_id: req_id.into(),
            content_type: content_type.into(),
            data,
        });
    }
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

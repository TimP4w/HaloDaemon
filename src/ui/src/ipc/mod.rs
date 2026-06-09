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

    pub fn send_binary(
        &self,
        req_id: impl Into<String>,
        content_type: impl Into<String>,
        data: Vec<u8>,
    ) {
        let _ = self.tx.send(IpcCmd::Binary {
            req_id: req_id.into(),
            content_type: content_type.into(),
            data,
        });
    }
}

/// Path to the daemon's IPC command channel
pub use halod_protocol::socket::socket_path;

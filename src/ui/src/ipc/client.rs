use std::sync::Arc;
use std::time::Duration;

use async_channel::Sender;
use halod_protocol::debug_info::DebugInfo;
use halod_protocol::types::RunningApp;
use halod_protocol::frames::{
    decode_header, encode_binary_frame, encode_binary_payload,
    encode_json_frame, FRAME_BINARY, FRAME_JSON,
};
use halod_protocol::types::{CanvasFrame, LcdEngineFrame, Notification};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::IpcCmd;

#[derive(Debug)]
pub enum DaemonMsg {
    State(serde_json::Value),
    LcdImages(Vec<serde_json::Value>),
    ImageUploaded { request_id: String },
    DebugInfo(DebugInfo),
    Connected,
    Disconnected,
    /// Per-client reply to a failed command, sent by the IPC router.
    Error(String),
    /// Broadcast notification (engine exit, device init failure, …).
    Notification(Notification),
    RunningApps(Vec<RunningApp>),
}

pub async fn run(
    mut cmd_rx: mpsc::UnboundedReceiver<IpcCmd>,
    event_tx: Sender<DaemonMsg>,
    frame_tx: async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: async_channel::Sender<Arc<LcdEngineFrame>>,
) {
    loop {
        match run_connection(&mut cmd_rx, &event_tx, &frame_tx, &lcd_frame_tx).await {
            Ok(()) => log::info!("IPC connection closed"),
            Err(e) => log::warn!("IPC connection error: {e}"),
        }
        let _ = event_tx.send(DaemonMsg::Disconnected).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(unix)]
async fn run_connection(
    cmd_rx: &mut mpsc::UnboundedReceiver<IpcCmd>,
    event_tx: &Sender<DaemonMsg>,
    frame_tx: &async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: &async_channel::Sender<Arc<LcdEngineFrame>>,
) -> anyhow::Result<()> {
    use tokio::net::UnixStream;
    let path = super::socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .map_err(|e| anyhow::anyhow!("Connect {path}: {e}"))?;
    run_stream(&mut stream, cmd_rx, event_tx, frame_tx, lcd_frame_tx).await
}

#[cfg(windows)]
async fn run_connection(
    cmd_rx: &mut mpsc::UnboundedReceiver<IpcCmd>,
    event_tx: &Sender<DaemonMsg>,
    frame_tx: &async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: &async_channel::Sender<Arc<LcdEngineFrame>>,
) -> anyhow::Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // Returned when every pipe instance is currently busy; retry briefly.
    const ERROR_PIPE_BUSY: i32 = 231;
    let path = super::socket_path();
    let mut client = loop {
        match ClientOptions::new().open(&path) {
            Ok(client) => break client,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(anyhow::anyhow!("Connect {path}: {e}")),
        }
    };
    run_stream(&mut client, cmd_rx, event_tx, frame_tx, lcd_frame_tx).await
}

async fn run_stream<S>(
    stream: &mut S,
    cmd_rx: &mut mpsc::UnboundedReceiver<IpcCmd>,
    event_tx: &Sender<DaemonMsg>,
    frame_tx: &async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: &async_channel::Sender<Arc<LcdEngineFrame>>,
) -> anyhow::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let _ = event_tx.send(DaemonMsg::Connected).await;

    let mut header = [0u8; 5];
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    IpcCmd::Json(msg) => {
                        stream.write_all(&encode_json_frame(&msg)).await?;
                    }
                    IpcCmd::Binary { req_id, content_type, data } => {
                        let payload = encode_binary_payload(&req_id, &content_type, &data);
                        stream.write_all(&encode_binary_frame(&payload)).await?;
                    }
                }
                stream.flush().await?;
            }
            result = stream.read_exact(&mut header) => {
                result?;
                let (frame_type, payload_len) = decode_header(&header);
                const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
                if payload_len > MAX_PAYLOAD {
                    return Err(anyhow::anyhow!("daemon sent oversized frame: {payload_len} bytes"));
                }
                let mut payload = vec![0u8; payload_len as usize];
                if payload_len > 0 {
                    stream.read_exact(&mut payload).await?;
                }
                handle_incoming(frame_type, payload, event_tx, frame_tx, lcd_frame_tx).await;
            }
        }
    }
    Ok(())
}

async fn handle_incoming(
    frame_type: u8,
    payload: Vec<u8>,
    event_tx: &Sender<DaemonMsg>,
    frame_tx: &async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: &async_channel::Sender<Arc<LcdEngineFrame>>,
) {
    match frame_type {
        FRAME_JSON => match serde_json::from_slice::<serde_json::Value>(&payload) {
            Ok(msg) => dispatch_json(msg, event_tx, frame_tx, lcd_frame_tx).await,
            Err(e) => log::warn!("JSON parse error: {e}"),
        },
        FRAME_BINARY => {}
        _ => {}
    }
}

async fn dispatch_json(
    msg: serde_json::Value,
    event_tx: &Sender<DaemonMsg>,
    frame_tx: &async_channel::Sender<Arc<CanvasFrame>>,
    lcd_frame_tx: &async_channel::Sender<Arc<LcdEngineFrame>>,
) {
    match msg["type"].as_str().unwrap_or("") {
        "state" => {
            let data = msg.get("data").cloned().unwrap_or(msg);
            let _ = event_tx.send(DaemonMsg::State(data)).await;
        }
        "canvas_frame" => {
            if let Ok(frame) = serde_json::from_value::<CanvasFrame>(msg["data"].clone()) {
                // Non-blocking: if the consumer is behind, drop the oldest frame (capacity=2).
                let _ = frame_tx.try_send(Arc::new(frame));
            }
        }
        "lcd_engine_frame" => {
            if let Ok(frame) = serde_json::from_value::<LcdEngineFrame>(msg["data"].clone()) {
                let _ = lcd_frame_tx.try_send(Arc::new(frame));
            }
        }
        "lcd_images" => {
            let files = msg["files"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let _ = event_tx.send(DaemonMsg::LcdImages(files)).await;
        }
        "image_uploaded" => {
            let req_id = msg["request_id"].as_str().unwrap_or("").to_string();
            let _ = event_tx.send(DaemonMsg::ImageUploaded { request_id: req_id }).await;
        }
        "debug_info" => {
            match serde_json::from_value::<DebugInfo>(msg["data"].clone()) {
                Ok(info) => {
                    let _ = event_tx.send(DaemonMsg::DebugInfo(info)).await;
                }
                Err(e) => log::warn!("debug_info parse: {e}"),
            }
        }
        "error" => {
            let text = msg["message"]
                .as_str()
                .unwrap_or("Unknown error")
                .to_string();
            let _ = event_tx.send(DaemonMsg::Error(text)).await;
        }
        "notification" => {
            match serde_json::from_value::<Notification>(msg["data"].clone()) {
                Ok(n) => {
                    let _ = event_tx.send(DaemonMsg::Notification(n)).await;
                }
                Err(e) => log::warn!("notification parse: {e}"),
            }
        }
        "running_apps_list" => {
            match serde_json::from_value::<Vec<RunningApp>>(msg["apps"].clone()) {
                Ok(apps) => { let _ = event_tx.send(DaemonMsg::RunningApps(apps)).await; }
                Err(e) => log::warn!("running_apps_list parse: {e}"),
            }
        }
        _ => {}
    }
}

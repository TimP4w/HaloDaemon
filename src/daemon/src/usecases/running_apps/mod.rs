use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::ipc::ClientHandle;
use crate::state::AppState;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

pub async fn list(_msg: Value, client: ClientHandle, _app: Arc<AppState>) -> Result<()> {
    #[cfg(target_os = "linux")]
    let apps = linux::build_apps();
    #[cfg(target_os = "windows")]
    let apps = windows::build_apps();
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let apps: Vec<halod_protocol::types::RunningApp> = vec![];
    client.send_json(&serde_json::json!({
        "type": "running_apps_list",
        "apps": apps,
    }));
    Ok(())
}

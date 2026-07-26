// SPDX-License-Identifier: GPL-3.0-or-later
use crate::application::ipc::ClientHandle;
use anyhow::Result;
use std::collections::HashMap;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Resolve `process_name -> icon` for the given process names so the UI can
/// show app icons on rule badges. On Linux this reads the installed `.desktop`
/// catalog (works for any installed app); on Windows it reads a persistent cache
/// populated whenever running apps are enumerated (so an app must have been seen
/// running once). Unknown processes are simply omitted.
///
pub fn resolve_process_icons(process_names: &[String]) -> HashMap<String, String> {
    if process_names.is_empty() {
        return HashMap::new();
    }
    resolve_process_icons_uncached(process_names)
}

fn resolve_process_icons_uncached(process_names: &[String]) -> HashMap<String, String> {
    #[cfg(target_os = "linux")]
    {
        linux::resolve_icons(process_names)
    }
    #[cfg(target_os = "windows")]
    {
        windows::resolve_icons(process_names)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = process_names;
        HashMap::new()
    }
}

pub async fn list(client: ClientHandle) -> Result<()> {
    let apps = tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "linux")]
        return linux::build_apps();
        #[cfg(target_os = "windows")]
        return windows::build_apps();
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        Vec::<halod_shared::types::RunningApp>::new()
    })
    .await
    .map_err(|error| anyhow::anyhow!("running-app scan panicked: {error}"))?;
    client.send_json(&serde_json::json!({
        "type": "running_apps_list",
        "apps": apps,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_process_icons_empty_input_returns_empty_map() {
        let result = resolve_process_icons(&[]);
        assert!(result.is_empty());
    }
}

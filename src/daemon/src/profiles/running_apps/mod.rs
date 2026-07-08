use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::ipc::ClientHandle;

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
/// Called on every broadcast, so the result is memoized keyed on the name list
/// and only recomputed when it changes.
pub fn resolve_process_icons(process_names: &[String]) -> HashMap<String, String> {
    if process_names.is_empty() {
        return HashMap::new();
    }
    static CACHE: Mutex<Option<(Vec<String>, HashMap<String, String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if let Some((names, icons)) = cache.as_ref() {
        if names.as_slice() == process_names {
            return icons.clone();
        }
    }
    let icons = resolve_process_icons_uncached(process_names);
    *cache = Some((process_names.to_vec(), icons.clone()));
    icons
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
    #[cfg(target_os = "linux")]
    let apps = linux::build_apps();
    #[cfg(target_os = "windows")]
    let apps = windows::build_apps();
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let apps: Vec<halod_shared::types::RunningApp> = vec![];
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

// SPDX-License-Identifier: GPL-3.0-or-later
//! Save/load/delete named LCD templates under `{config_dir}/lcd/<name>.yaml`.
//! Mirrors the LCD image library's validation and IPC push patterns (see `usecases::lcd`).

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::ipc::ClientHandle;
use crate::state::AppState;
use halod_shared::lcd_custom::CustomTemplateDef;

pub fn lcd_templates_dir() -> std::path::PathBuf {
    crate::config::config_dir().join("lcd")
}

/// Validate a user-supplied template name before joining it onto
/// `lcd_templates_dir()`. Allowlist-based, mirroring
/// `image_support::validate_image_filename`: only `[A-Za-z0-9 _-]`, no
/// leading dot, non-empty, bounded length. The `.yaml` extension is appended
/// server-side and never accepted from the client.
pub fn validate_template_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-'));
    if !ok {
        anyhow::bail!("invalid template name");
    }
    Ok(())
}

fn template_path(name: &str) -> std::path::PathBuf {
    lcd_templates_dir().join(format!("{name}.yaml"))
}

/// Sorted list of saved template names, read straight off disk.
pub fn list_templates() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(lcd_templates_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|e| e.to_str()) == Some("yaml"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

const MAX_TEMPLATE_BYTES: u64 = 512 * 1024;
const MAX_TEMPLATE_WIDGETS: usize = 256;

/// Central template complexity guard.
pub fn validate_template(def: &CustomTemplateDef) -> Result<()> {
    anyhow::ensure!(
        def.widgets.len() <= MAX_TEMPLATE_WIDGETS,
        "template has too many widgets ({} > {MAX_TEMPLATE_WIDGETS})",
        def.widgets.len()
    );
    halod_shared::lcd_custom::validate_widgets(def).map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

pub async fn save_template(name: String, def: CustomTemplateDef, app: Arc<AppState>) -> Result<()> {
    validate_template_name(&name)?;
    validate_template(&def)?;
    let yaml = serde_yaml::to_string(&def)?;
    anyhow::ensure!(
        yaml.len() as u64 <= MAX_TEMPLATE_BYTES,
        "template too large"
    );
    let path = template_path(&name);
    tokio::task::spawn_blocking(move || crate::config::atomic_write(&path, &yaml)).await??;
    log::info!("[LCD] saved template '{name}'");
    app.lcd.refresh_templates().await;
    crate::ipc::broadcast_state(&app).await;
    Ok(())
}

pub async fn load_template(name: String, client: ClientHandle) -> Result<()> {
    validate_template_name(&name)?;
    let path = template_path(&name);
    if let Ok(meta) = tokio::fs::metadata(&path).await {
        anyhow::ensure!(meta.len() <= MAX_TEMPLATE_BYTES, "template too large");
    }
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| anyhow!("template not found: {e}"))?;
    anyhow::ensure!(raw.len() as u64 <= MAX_TEMPLATE_BYTES, "template too large");
    let def: CustomTemplateDef = serde_yaml::from_str(&raw)
        .map_err(|e| anyhow!("failed to parse template '{name}': {e}"))?;
    validate_template(&def)?;
    client.send_json(&json!({ "type": "lcd_template", "name": name, "def": def }));
    Ok(())
}

pub async fn delete_template(name: String, app: Arc<AppState>) -> Result<()> {
    validate_template_name(&name)?;
    match tokio::fs::remove_file(template_path(&name)).await {
        Ok(()) => log::info!("[LCD] deleted template '{name}'"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    app.lcd.refresh_templates().await;
    crate::ipc::broadcast_state(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_template_name_accepts_normal_names() {
        assert!(validate_template_name("My Preset").is_ok());
        assert!(validate_template_name("system_stats-v2").is_ok());
    }

    #[test]
    fn validate_template_name_rejects_traversal() {
        assert!(validate_template_name("../escape").is_err());
        assert!(validate_template_name("a/b").is_err());
    }

    #[test]
    fn validate_template_name_rejects_leading_dot() {
        assert!(validate_template_name(".hidden").is_err());
    }

    #[test]
    fn validate_template_name_rejects_empty() {
        assert!(validate_template_name("").is_err());
    }

    #[test]
    fn validate_template_name_rejects_overlong() {
        let name = "a".repeat(65);
        assert!(validate_template_name(&name).is_err());
    }

    proptest::proptest! {
        #[test]
        fn accepted_names_stay_inside_templates_dir(name in "[A-Za-z0-9 _-]{1,64}") {
            // Serialize against the tests that mutate `HALOD_CONFIG_DIR`, so the
            // config dir can't change between the two reads below.
            let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if validate_template_name(&name).is_ok() {
                let path = template_path(&name);
                assert!(path.starts_with(lcd_templates_dir()));
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn save_list_load_delete_cycle() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let def = CustomTemplateDef::default();
        save_template("My Preset".into(), def.clone(), app.clone())
            .await
            .unwrap();

        assert_eq!(list_templates(), vec!["My Preset".to_string()]);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::sync::Arc<Vec<u8>>>(16);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        load_template("My Preset".into(), client).await.unwrap();
        let frame = rx.try_recv().unwrap();
        let payload = &frame[5..];
        let v: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(v["type"], "lcd_template");
        assert_eq!(v["name"], "My Preset");

        delete_template("My Preset".into(), app).await.unwrap();
        assert!(list_templates().is_empty());

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_template_noop_when_missing() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(crate::config::Config::default()));
        delete_template("nonexistent".into(), app).await.unwrap();

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }
}

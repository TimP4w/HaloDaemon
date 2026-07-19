// SPDX-License-Identifier: GPL-3.0-or-later
//! Save/load/delete named custom Designer effects.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use halod_shared::effect_designer::{validate_effect_name, DesignerParams, DESIGNER_EFFECT_ID};
use halod_shared::types::{EffectDef, EffectParamValue};

use crate::application::state::AppState;

pub fn custom_effects_dir() -> std::path::PathBuf {
    crate::config::config_dir().join("effects")
}

fn effect_path(name: &str) -> std::path::PathBuf {
    custom_effects_dir().join(format!("{name}.yaml"))
}

const MAX_CUSTOM_EFFECTS: usize = 1024;

/// Sorted-by-name list of saved custom effects. Each file is size-bounded and
/// re-validated on load (same name allowlist + param normalization as save).
pub fn list_custom_effects() -> Vec<EffectDef> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(custom_effects_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    paths.sort();
    if paths.len() > MAX_CUSTOM_EFFECTS {
        log::warn!(
            "[RGB] {} custom-effect files exceed the {MAX_CUSTOM_EFFECTS} cap; ignoring the rest",
            paths.len()
        );
        paths.truncate(MAX_CUSTOM_EFFECTS);
    }

    let mut defs: Vec<EffectDef> = paths
        .into_iter()
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?;
            if !validate_effect_name(stem) {
                log::warn!("[RGB] ignoring custom effect with invalid name: {stem}");
                return None;
            }
            let raw = crate::config::read_bounded(&path, "custom effect").ok()?;
            let mut def = serde_yaml::from_str::<EffectDef>(&raw).ok()?;
            def.params = DesignerParams::from_params(&def.params).to_params();
            def.effect_id = DESIGNER_EFFECT_ID.to_string();
            Some(def)
        })
        .collect();
    defs.sort_by(|a, b| {
        a.name
            .as_deref()
            .unwrap_or("")
            .cmp(b.name.as_deref().unwrap_or(""))
    });
    defs
}

pub async fn save_custom_effect(
    name: String,
    params: HashMap<String, EffectParamValue>,
    app: Arc<AppState>,
) -> Result<()> {
    if !validate_effect_name(&name) {
        anyhow::bail!("invalid effect name");
    }
    let normalized = DesignerParams::from_params(&params).to_params();
    let def = EffectDef {
        effect_id: DESIGNER_EFFECT_ID.to_string(),
        name: Some(name.clone()),
        params: normalized,
    };
    let path = effect_path(&name);
    let yaml = serde_yaml::to_string(&def)?;
    tokio::task::spawn_blocking(move || crate::config::atomic_write(&path, &yaml)).await??;
    log::info!("[RGB] saved custom effect '{name}'");
    app.lighting.custom_effects.refresh().await;
    app.record_change(crate::application::bus::coordinator::Change::LightingCatalog)
        .await;
    Ok(())
}

pub async fn delete_custom_effect(name: String, app: Arc<AppState>) -> Result<()> {
    if !validate_effect_name(&name) {
        anyhow::bail!("invalid effect name");
    }
    match tokio::fs::remove_file(effect_path(&name)).await {
        Ok(()) => log::info!("[RGB] deleted custom effect '{name}'"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    app.lighting.custom_effects.refresh().await;
    app.record_change(crate::application::bus::coordinator::Change::LightingCatalog)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn save_list_delete_cycle() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(crate::config::Config::default()));
        save_custom_effect("My Effect".into(), HashMap::new(), app.clone())
            .await
            .unwrap();

        let listed = list_custom_effects();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name.as_deref(), Some("My Effect"));
        assert_eq!(listed[0].effect_id, DESIGNER_EFFECT_ID);

        delete_custom_effect("My Effect".into(), app).await.unwrap();
        assert!(list_custom_effects().is_empty());

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_missing_effect_is_a_noop() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(crate::config::Config::default()));
        delete_custom_effect("nonexistent".into(), app)
            .await
            .unwrap();

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn save_clamps_out_of_range_params_before_writing_to_disk() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let mut params = HashMap::new();
        params.insert("speed".to_string(), EffectParamValue::Float(9999.0));
        save_custom_effect("Wild".into(), params, app.clone())
            .await
            .unwrap();

        let listed = list_custom_effects();
        let saved = DesignerParams::from_params(&listed[0].params);
        assert_eq!(saved.speed, 100.0);

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[test]
    fn save_rejects_invalid_name() {
        assert!(!validate_effect_name("../escape"));
    }
}

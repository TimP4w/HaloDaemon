use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use crate::state::AppState;
use halod_protocol::types::AppRule;

fn normalize(name: &str) -> String {
    let s = name.to_lowercase();
    s.strip_suffix(".exe").unwrap_or(&s).to_string()
}

fn parse_rule(msg: &Value) -> Result<AppRule> {
    let process_names: Vec<String> = msg["process_names"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing process_names"))?
        .iter()
        .filter_map(|v| v.as_str().map(normalize))
        .filter(|s| !s.is_empty())
        .collect();
    if process_names.is_empty() {
        anyhow::bail!("process_names must not be empty");
    }
    let profile = msg["profile"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing profile"))?
        .to_string();
    let enabled = msg["enabled"].as_bool().unwrap_or(true);
    Ok(AppRule { process_names, profile, enabled })
}

fn notify_watcher(app: &AppState) {
    use crate::engines::focus_watcher::ControlMsg;
    if let Ok(guard) = app.focus_watcher_tx.try_lock() {
        if let Some(tx) = &*guard {
            let _ = tx.try_send(ControlMsg::RulesUpdated);
        }
    }
}

fn save_config(cfg: &crate::config::Config, app: &crate::state::AppState) {
    #[cfg(not(test))]
    app.request_config_save(cfg.clone());
    #[cfg(test)]
    let _ = (cfg, app);
}

pub async fn add(msg: Value, app: Arc<AppState>) -> Result<()> {
    let rule = parse_rule(&msg)?;
    let snap = {
        let mut cfg = app.config.write().await;
        cfg.app_rules.push(rule);
        cfg.clone()
    };
    save_config(&snap, &app);
    notify_watcher(&app);
    crate::ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn update(msg: Value, app: Arc<AppState>) -> Result<()> {
    let index = msg["index"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing index"))? as usize;
    let rule = parse_rule(&msg)?;
    let snap = {
        let mut cfg = app.config.write().await;
        if index >= cfg.app_rules.len() {
            anyhow::bail!("index {index} out of range");
        }
        cfg.app_rules[index] = rule;
        cfg.clone()
    };
    save_config(&snap, &app);
    notify_watcher(&app);
    crate::ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn remove(msg: Value, app: Arc<AppState>) -> Result<()> {
    let index = msg["index"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing index"))? as usize;
    let snap = {
        let mut cfg = app.config.write().await;
        if index >= cfg.app_rules.len() {
            anyhow::bail!("index {index} out of range");
        }
        cfg.app_rules.remove(index);
        cfg.clone()
    };
    save_config(&snap, &app);
    notify_watcher(&app);
    crate::ipc::broadcast_state(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[tokio::test]
    async fn add_appends_rule() {
        let app = make_app();
        add(json!({"process_names": ["firefox"], "profile": "Web", "enabled": true}), app.clone())
            .await.unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules.len(), 1);
        assert_eq!(cfg.app_rules[0].profile, "Web");
    }

    #[tokio::test]
    async fn add_rejects_empty_process_names() {
        let app = make_app();
        let err = add(json!({"process_names": [], "profile": "Web", "enabled": true}), app)
            .await.unwrap_err();
        assert!(err.to_string().contains("process_names"));
    }

    #[tokio::test]
    async fn add_normalizes_process_names_to_lowercase() {
        let app = make_app();
        add(json!({"process_names": ["Firefox", "Code.exe"], "profile": "Web", "enabled": true}), app.clone())
            .await.unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules[0].process_names, vec!["firefox", "code"]);
    }

    #[tokio::test]
    async fn update_replaces_rule_at_index() {
        let app = make_app();
        add(json!({"process_names": ["firefox"], "profile": "Web", "enabled": true}), app.clone())
            .await.unwrap();
        update(json!({"index": 0, "process_names": ["chrome"], "profile": "Work", "enabled": false}), app.clone())
            .await.unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules[0].process_names, vec!["chrome"]);
        assert_eq!(cfg.app_rules[0].profile, "Work");
        assert!(!cfg.app_rules[0].enabled);
    }

    #[tokio::test]
    async fn update_rejects_out_of_range_index() {
        let app = make_app();
        let err = update(json!({"index": 5, "process_names": ["x"], "profile": "X", "enabled": true}), app)
            .await.unwrap_err();
        assert!(err.to_string().contains("index"));
    }

    #[tokio::test]
    async fn remove_deletes_rule_at_index() {
        let app = make_app();
        add(json!({"process_names": ["firefox"], "profile": "Web", "enabled": true}), app.clone())
            .await.unwrap();
        remove(json!({"index": 0}), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(cfg.app_rules.is_empty());
    }

    #[tokio::test]
    async fn remove_rejects_out_of_range_index() {
        let app = make_app();
        let err = remove(json!({"index": 0}), app).await.unwrap_err();
        assert!(err.to_string().contains("index"));
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
use crate::state::AppState;
use anyhow::Result;
use halod_shared::types::AppRule;
use std::sync::Arc;

fn normalize(name: &str) -> String {
    let s = name.to_lowercase();
    s.strip_suffix(".exe").unwrap_or(&s).to_string()
}

fn to_rule(process_names: Vec<String>, profile: String, enabled: bool) -> Result<AppRule> {
    let process_names: Vec<String> = process_names
        .into_iter()
        .map(|n| normalize(&n))
        .filter(|s| !s.is_empty())
        .collect();
    if process_names.is_empty() {
        anyhow::bail!("process_names must not be empty");
    }
    Ok(AppRule {
        process_names,
        profile,
        enabled,
    })
}

async fn notify_watcher(app: &AppState) {
    use crate::profiles::focus_watcher::ControlMsg;
    app.focus.notify(ControlMsg::RulesUpdated).await;
}

pub async fn add(
    process_names: Vec<String>,
    profile: String,
    enabled: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let rule = to_rule(process_names, profile, enabled)?;
    app.config.write().await.app_rules.push(rule);
    app.request_config_save();
    notify_watcher(&app).await;
    app.record_change(crate::services::effective_state::Change::AppRules)
        .await;
    Ok(())
}

pub async fn update(
    index: usize,
    process_names: Vec<String>,
    profile: String,
    enabled: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let rule = to_rule(process_names, profile, enabled)?;
    {
        let mut cfg = app.config.write().await;
        if index >= cfg.app_rules.len() {
            anyhow::bail!("index {index} out of range");
        }
        cfg.app_rules[index] = rule;
    }
    app.request_config_save();
    notify_watcher(&app).await;
    app.record_change(crate::services::effective_state::Change::AppRules)
        .await;
    Ok(())
}

pub async fn remove(index: usize, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        if index >= cfg.app_rules.len() {
            anyhow::bail!("index {index} out of range");
        }
        cfg.app_rules.remove(index);
    }
    app.request_config_save();
    notify_watcher(&app).await;
    app.record_change(crate::services::effective_state::Change::AppRules)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_app() -> (Arc<AppState>, crate::test_support::TmpConfigDir) {
        let guard = crate::test_support::tmp_config_dir();
        (Arc::new(AppState::new(Config::default())), guard)
    }

    #[tokio::test]
    async fn add_appends_rule() {
        let (app, _cfg) = make_app();
        add(vec!["firefox".into()], "Web".into(), true, app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules.len(), 1);
        assert_eq!(cfg.app_rules[0].profile, "Web");
    }

    #[tokio::test]
    async fn add_rejects_empty_process_names() {
        let (app, _cfg) = make_app();
        let err = add(vec![], "Web".into(), true, app).await.unwrap_err();
        assert!(err.to_string().contains("process_names"));
    }

    #[tokio::test]
    async fn add_normalizes_process_names_to_lowercase() {
        let (app, _cfg) = make_app();
        add(
            vec!["Firefox".into(), "Code.exe".into()],
            "Web".into(),
            true,
            app.clone(),
        )
        .await
        .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules[0].process_names, vec!["firefox", "code"]);
    }

    #[tokio::test]
    async fn update_replaces_rule_at_index() {
        let (app, _cfg) = make_app();
        add(vec!["firefox".into()], "Web".into(), true, app.clone())
            .await
            .unwrap();
        update(0, vec!["chrome".into()], "Work".into(), false, app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.app_rules[0].process_names, vec!["chrome"]);
        assert_eq!(cfg.app_rules[0].profile, "Work");
        assert!(!cfg.app_rules[0].enabled);
    }

    #[tokio::test]
    async fn update_rejects_out_of_range_index() {
        let (app, _cfg) = make_app();
        let err = update(5, vec!["x".into()], "X".into(), true, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("index"));
    }

    #[tokio::test]
    async fn remove_deletes_rule_at_index() {
        let (app, _cfg) = make_app();
        add(vec!["firefox".into()], "Web".into(), true, app.clone())
            .await
            .unwrap();
        remove(0, app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(cfg.app_rules.is_empty());
    }

    #[tokio::test]
    async fn remove_rejects_out_of_range_index() {
        let (app, _cfg) = make_app();
        let err = remove(0, app).await.unwrap_err();
        assert!(err.to_string().contains("index"));
    }
}

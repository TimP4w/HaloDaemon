// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

use crate::profiles::usecases::profiles::switch_profile_direct;
use crate::state::{AppState, EngineRunConfig};

#[cfg(target_os = "linux")]
pub mod gnome_shell;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod wayland;
pub mod windows;

#[derive(Debug, Clone)]
pub enum FocusEvent {
    AppFocused { process_name: String },
    NoApp,
}

#[derive(Debug, Clone)]
pub enum ControlMsg {
    ManualSwitch { profile: String },
    RulesUpdated,
    Shutdown,
}

pub struct FocusWatcherEngine {
    app: Arc<AppState>,
}

impl FocusWatcherEngine {
    pub fn new(app: Arc<AppState>) -> Arc<Self> {
        Arc::new(Self { app })
    }

    pub async fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
        ctrl_rx: mpsc::Receiver<ControlMsg>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            match spawn_platform_backend().await {
                Ok(focus_rx) => {
                    self.app.focus.set_supported(true);
                    if let Some(ctrl_rx) =
                        run_event_loop(self.app.clone(), cfg_rx, ctrl_rx, focus_rx).await
                    {
                        // Backend stream closed — clear supported flag and stay alive.
                        self.app.focus.set_supported(false);
                        run_idle_loop(ctrl_rx).await;
                    }
                }
                Err(e) => {
                    log::info!("[FocusWatcher] No platform backend available: {e}");
                    run_idle_loop(ctrl_rx).await;
                }
            }
        })
    }
}

pub fn normalize_name(s: &str) -> String {
    let lower = s.to_lowercase();
    #[cfg(windows)]
    let lower = lower
        .strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(lower);
    // Nix wraps binaries as `.name-wrapped`; unwrap to match desktop Exec basenames.
    if let Some(inner) = lower
        .strip_prefix('.')
        .and_then(|s| s.strip_suffix("-wrapped"))
    {
        return inner.to_owned();
    }
    lower
}

async fn apply_focus(
    app: &Arc<AppState>,
    process_name: Option<&str>,
    baseline: &str,
    last_active_rule: &mut Option<String>,
) {
    match process_name {
        Some(proc) => {
            let (rules, profiles_exist, current_profile) = {
                let cfg = app.config.read().await;
                let rules = cfg.app_rules.clone();
                (rules, cfg.profiles.clone(), cfg.active_profile.clone())
            };

            let matched = rules
                .iter()
                .filter(|r| r.enabled)
                .find(|r| r.process_names.iter().any(|n| n == proc));

            if let Some(rule) = matched {
                if !profiles_exist.contains_key(&rule.profile) {
                    log::warn!(
                        "[FocusWatcher] Rule profile '{}' not found, skipping",
                        rule.profile
                    );
                    if last_active_rule.take().is_some() {
                        switch_profile_direct(baseline.to_string(), app.clone()).await;
                    }
                    return;
                }
                if rule.profile != current_profile {
                    switch_profile_direct(rule.profile.clone(), app.clone()).await;
                }
                *last_active_rule = Some(rule.profile.clone());
            } else if last_active_rule.take().is_some() {
                switch_profile_direct(baseline.to_string(), app.clone()).await;
            }
        }
        None => {
            if last_active_rule.take().is_some() {
                switch_profile_direct(baseline.to_string(), app.clone()).await;
            }
        }
    }
}

/// Returns `Some(ctrl_rx)` when the focus backend stream closes (backend died);
/// the caller can continue with `run_idle_loop`. Returns `None` on Shutdown.
pub async fn run_event_loop(
    app: Arc<AppState>,
    mut cfg_rx: watch::Receiver<EngineRunConfig>,
    mut ctrl_rx: mpsc::Receiver<ControlMsg>,
    mut focus_rx: mpsc::Receiver<FocusEvent>,
) -> Option<mpsc::Receiver<ControlMsg>> {
    let mut baseline = app.config.read().await.active_profile.clone();
    let mut last_active_rule: Option<String> = None;
    let mut current_foreground: Option<String> = None;
    let mut enabled = cfg_rx.borrow_and_update().enabled;

    loop {
        if !enabled {
            tokio::select! {
                _ = cfg_rx.changed() => {
                    enabled = cfg_rx.borrow_and_update().enabled;
                }
                msg = ctrl_rx.recv() => {
                    if matches!(msg, None | Some(ControlMsg::Shutdown)) { return None; }
                }
            }
            continue;
        }

        tokio::select! {
            biased;
            event = focus_rx.recv() => {
                match event {
                    // Backend stream ended — return ctrl_rx so caller can idle.
                    None => return Some(ctrl_rx),
                    Some(FocusEvent::AppFocused { process_name }) => {
                        current_foreground = Some(process_name.clone());
                        apply_focus(&app, Some(&process_name), &baseline, &mut last_active_rule).await;
                    }
                    Some(FocusEvent::NoApp) => {
                        current_foreground = None;
                        apply_focus(&app, None, &baseline, &mut last_active_rule).await;
                    }
                }
            }
            msg = ctrl_rx.recv() => {
                match msg {
                    None | Some(ControlMsg::Shutdown) => return None,
                    Some(ControlMsg::ManualSwitch { profile }) => {
                        baseline = profile.clone();
                        switch_profile_direct(profile, app.clone()).await;
                    }
                    Some(ControlMsg::RulesUpdated) => {
                        apply_focus(&app, current_foreground.as_deref(), &baseline, &mut last_active_rule).await;
                    }
                }
            }
            _ = cfg_rx.changed() => {
                enabled = cfg_rx.borrow_and_update().enabled;
            }
        }
    }
}

/// Keeps the engine alive without a backend: only responds to Shutdown.
async fn run_idle_loop(mut ctrl_rx: mpsc::Receiver<ControlMsg>) {
    while let Some(msg) = ctrl_rx.recv().await {
        if matches!(msg, ControlMsg::Shutdown) {
            return;
        }
    }
}

async fn spawn_platform_backend() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    #[cfg(target_os = "linux")]
    return linux::spawn().await;

    #[cfg(target_os = "windows")]
    return windows::spawn().await;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    anyhow::bail!("unsupported platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use crate::state::EngineRunConfig;
    use halod_shared::types::AppRule;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};

    fn enabled_cfg() -> EngineRunConfig {
        EngineRunConfig {
            enabled: true,
            tick_ms: 0,
        }
    }

    fn make_app_with_rule(
        process: &str,
        profile: &str,
    ) -> (Arc<AppState>, crate::test_support::TmpConfigDir) {
        let guard = crate::test_support::tmp_config_dir();
        let mut cfg = Config::default();
        cfg.profiles.insert(profile.to_string(), Default::default());
        cfg.app_rules.push(AppRule {
            process_names: vec![process.to_string()],
            profile: profile.to_string(),
            enabled: true,
        });
        (Arc::new(AppState::new(cfg)), guard)
    }

    #[tokio::test]
    async fn rule_match_switches_profile() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();
        drop(focus_tx);

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;

        assert_eq!(app.config.read().await.active_profile, "Web");
    }

    #[tokio::test]
    async fn no_rule_match_leaves_baseline() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "terminal".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;

        assert_eq!(app.config.read().await.active_profile, "default");
    }

    #[tokio::test]
    async fn restore_baseline_when_focus_leaves_rule_app() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        focus_tx.send(FocusEvent::NoApp).await.unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;

        assert_eq!(app.config.read().await.active_profile, "default");
    }

    #[tokio::test]
    async fn manual_switch_updates_baseline() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        app.config
            .write()
            .await
            .profiles
            .insert("Gaming".into(), Default::default());

        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        ctrl_tx
            .send(ControlMsg::ManualSwitch {
                profile: "Gaming".into(),
            })
            .await
            .unwrap();
        focus_tx.send(FocusEvent::NoApp).await.unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;

        assert_eq!(app.config.read().await.active_profile, "Gaming");
    }

    #[tokio::test]
    async fn disabled_rule_does_not_trigger() {
        let mut cfg = Config::default();
        cfg.profiles.insert("Web".into(), Default::default());
        cfg.app_rules.push(AppRule {
            process_names: vec!["firefox".into()],
            profile: "Web".into(),
            enabled: false,
        });
        let _cfg = crate::test_support::tmp_config_dir();
        let app = Arc::new(AppState::new(cfg));
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;

        assert_eq!(app.config.read().await.active_profile, "default");
    }

    #[tokio::test]
    async fn missing_profile_rule_skips_without_panic() {
        let (app, _cfg) = make_app_with_rule("firefox", "Deleted");
        app.config.write().await.profiles.remove("Deleted");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;
        assert_eq!(app.config.read().await.active_profile, "default");
    }

    #[tokio::test]
    async fn rules_updated_with_active_matching_foreground_switches_profile() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::RulesUpdated).await.unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;
        assert_eq!(app.config.read().await.active_profile, "Web");
    }

    #[tokio::test]
    async fn rules_updated_with_non_matching_foreground_leaves_profile_unchanged() {
        let (app, _cfg) = make_app_with_rule("firefox", "Web");
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "terminal".into(),
            })
            .await
            .unwrap();
        ctrl_tx.send(ControlMsg::RulesUpdated).await.unwrap();
        ctrl_tx.send(ControlMsg::Shutdown).await.unwrap();

        run_event_loop(app.clone(), cfg_rx, ctrl_rx, focus_rx).await;
        assert_eq!(app.config.read().await.active_profile, "default");
    }

    // ── normalize_name tests ────────────────────────────────────────────────

    #[test]
    fn normalize_nix_wrapped_name() {
        assert_eq!(normalize_name(".firefox-wrapped"), "firefox");
        assert_eq!(normalize_name(".code-wrapped"), "code");
        assert_eq!(normalize_name(".gnome-terminal-wrapped"), "gnome-terminal");
    }

    #[test]
    fn normalize_no_stripping_needed() {
        assert_eq!(normalize_name("firefox"), "firefox");
        assert_eq!(normalize_name("Code"), "code");
        assert_eq!(normalize_name("ALACRITTY"), "alacritty");
    }

    #[test]
    fn normalize_dot_only_no_inner_name() {
        // A name like ".wrapped" has no inner name; must not produce empty string.
        assert_eq!(normalize_name(".wrapped"), ".wrapped");
    }

    #[cfg(windows)]
    #[test]
    fn normalize_strips_exe_suffix() {
        assert_eq!(normalize_name("Firefox.exe"), "firefox");
    }
}

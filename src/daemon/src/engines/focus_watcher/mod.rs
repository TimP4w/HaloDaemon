use std::sync::Arc;
use tokio::sync::{mpsc, watch};

use crate::state::{AppState, EngineRunConfig};
use crate::usecases::profiles::switch_profile_direct;

#[cfg(target_os = "linux")]
mod gnome_shell;
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
                    self.app.set_focus_watcher_supported(true);
                    if let Some(ctrl_rx) =
                        run_event_loop(self.app.clone(), cfg_rx, ctrl_rx, focus_rx).await
                    {
                        // Backend stream closed — clear supported flag and stay alive.
                        self.app.set_focus_watcher_supported(false);
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
    if let Some(inner) = lower.strip_prefix('.').and_then(|s| s.strip_suffix("-wrapped")) {
        return inner.to_owned();
    }
    lower
}

async fn apply_focus(
    app: &Arc<AppState>,
    process_name: Option<&str>,
    baseline: &mut String,
    last_active_rule: &mut Option<String>,
) {
    match process_name {
        Some(proc) => {
            let (rules, profiles_exist) = {
                let cfg = app.config.read().await;
                let rules = cfg.app_rules.clone();
                (rules, cfg.profiles.clone())
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
                        switch_profile_direct(baseline.clone(), app.clone()).await;
                    }
                    return;
                }
                let current = app.config.read().await.active_profile.clone();
                if rule.profile != current {
                    switch_profile_direct(rule.profile.clone(), app.clone()).await;
                }
                *last_active_rule = Some(rule.profile.clone());
            } else if last_active_rule.take().is_some() {
                switch_profile_direct(baseline.clone(), app.clone()).await;
            }
        }
        None => {
            if last_active_rule.take().is_some() {
                switch_profile_direct(baseline.clone(), app.clone()).await;
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
                        apply_focus(&app, Some(&process_name), &mut baseline, &mut last_active_rule).await;
                    }
                    Some(FocusEvent::NoApp) => {
                        current_foreground = None;
                        apply_focus(&app, None, &mut baseline, &mut last_active_rule).await;
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
                        apply_focus(&app, current_foreground.as_deref(), &mut baseline, &mut last_active_rule).await;
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
    use halod_protocol::types::AppRule;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};

    fn enabled_cfg() -> EngineRunConfig {
        EngineRunConfig {
            enabled: true,
            tick_ms: 0,
            failsafe_duty: 0,
        }
    }

    fn make_app_with_rule(process: &str, profile: &str) -> Arc<AppState> {
        let mut cfg = Config::default();
        cfg.profiles.insert(profile.to_string(), Default::default());
        cfg.app_rules.push(AppRule {
            process_names: vec![process.to_string()],
            profile: profile.to_string(),
            enabled: true,
        });
        Arc::new(AppState::new(cfg))
    }

    #[tokio::test]
    async fn rule_match_switches_profile() {
        let app = make_app_with_rule("firefox", "Web");
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
        let app = make_app_with_rule("firefox", "Web");
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
        let app = make_app_with_rule("firefox", "Web");
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
        let app = make_app_with_rule("firefox", "Web");
        // Add another profile "Gaming" for baseline update
        app.config
            .write()
            .await
            .profiles
            .insert("Gaming".into(), Default::default());

        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (focus_tx, focus_rx) = mpsc::channel(8);
        let (_cfg_tx, cfg_rx) = watch::channel(enabled_cfg());

        // Focus firefox → switches to Web
        focus_tx
            .send(FocusEvent::AppFocused {
                process_name: "firefox".into(),
            })
            .await
            .unwrap();
        // User manually switches to Gaming → baseline becomes Gaming
        ctrl_tx
            .send(ControlMsg::ManualSwitch {
                profile: "Gaming".into(),
            })
            .await
            .unwrap();
        // Focus leaves → should restore Gaming, not default
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
        let app = make_app_with_rule("firefox", "Deleted");
        // Remove "Deleted" so the profile referenced by the rule no longer exists
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
        // Must not switch away from default
        assert_eq!(app.config.read().await.active_profile, "default");
    }
}

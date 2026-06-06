mod audio;
mod config;
mod discovery;
mod drivers;
mod elevation;
mod engines;
mod ipc;
mod logger;
mod notify;
#[cfg(windows)]
mod service;
mod state;
#[cfg(test)]
mod test_support;
mod usecases;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;

use crate::drivers::transports::hid::HidTransport;
use crate::state::{EngineRunConfig, Engines};
use crate::usecases::initialize_app_state;

/// How this process was invoked, decided purely from argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessRole {
    /// `--install-service`: register the Windows service, then exit.
    InstallService,
    /// `--uninstall-service`: remove the Windows service, then exit.
    UninstallService,
    /// `--service`: run the SCM supervisor.
    Supervisor,
    /// Run the device server. `worker` is true when the supervisor relaunched
    /// this exe into the user session (`--worker`): it is already elevated, so
    /// the UAC self-elevation must be skipped.
    Server { worker: bool },
}

/// Map argv (without argv[0]) to a [`ProcessRole`]. The service-control flags
/// take precedence; `--worker` only refines a plain server run.
fn process_role(args: &[String]) -> ProcessRole {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--install-service") {
        ProcessRole::InstallService
    } else if has("--uninstall-service") {
        ProcessRole::UninstallService
    } else if has("--service") {
        ProcessRole::Supervisor
    } else {
        ProcessRole::Server {
            worker: has("--worker"),
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = process_role(&args);

    // Windows service control entry points, dispatched before any runtime is
    // created. These are invoked by the installer and the SCM.
    #[cfg(windows)]
    match role {
        ProcessRole::InstallService => return service::install(),
        ProcessRole::UninstallService => return service::uninstall(),
        // Launched by the Service Control Manager: run the supervisor, which
        // relaunches this exe as `--worker` in the user session.
        ProcessRole::Supervisor => return service::run_supervisor(),
        ProcessRole::Server { .. } => {}
    }

    // A `--worker` run is already elevated (the supervisor passed it an
    // elevated token), so it must not prompt for UAC; a plain/dev run does.
    let is_worker = matches!(role, ProcessRole::Server { worker: true });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_daemon(is_worker))
}

/// The actual device server. Runs for a dev/plain launch and for the service
/// `--worker` process alike.
async fn run_daemon(is_worker: bool) -> Result<()> {
    let cfg = config::load()?;

    // Set up the log level from config before anything else logs.
    let initial_level: log::LevelFilter = cfg
        .global
        .log_level
        .parse()
        .unwrap_or(log::LevelFilter::Info);

    let env_logger = env_logger::Builder::new()
        .filter_level(initial_level)
        .parse_default_env()
        .build();

    let app = Arc::new(state::AppState::new(cfg).with_service_worker(is_worker));
    let _save_worker = crate::state::start_config_save_worker(app.config_save_tx.subscribe());

    let buffering_logger =
        logger::BufferingLogger::new(env_logger, Arc::clone(&app.log_buffer));
    log::set_boxed_logger(Box::new(buffering_logger)).expect("logger already set");
    log::set_max_level(initial_level);

    // Chipset SMBus access (DRAM / GPU RGB via PawnIO) needs Administrator
    // rights. A dev/plain run prompts for it via UAC; declining is non-fatal.
    // The service worker is already launched with an elevated token by the
    // supervisor, so it must NOT prompt — skip elevation entirely there.
    if !is_worker {
        elevation::ensure_elevated();
    }

    log::info!("Daemon is running. Press Ctrl+C to shut down.");

    let ipc_handle = ipc::serve(app.clone());
    let broadcast_handle = ipc::broadcast_loop(app.clone());

    log::info!("Discovering devices...");
    initialize_app_state(app.clone()).await;
    log::info!("Device discovery complete");
    tokio::spawn(HidTransport::hotplug_monitor(app.clone()));

    // Build initial EngineRunConfig values from loaded global config.
    let global = {
        let cfg = app.config.read().await;
        cfg.global.clone()
    };

    let (fan_curve_cfg_tx, fan_curve_cfg_rx) = watch::channel(EngineRunConfig {
        enabled: global.engine_fan_curve_enabled,
        tick_ms: global.engine_fan_curve_tick_ms,
        failsafe_duty: global.fan_failsafe_duty,
    });
    let canvas_tick_ms = 1000 / global.engine_canvas_fps.max(1) as u64;
    let (canvas_cfg_tx, canvas_cfg_rx) = watch::channel(EngineRunConfig {
        enabled: global.engine_canvas_enabled,
        tick_ms: canvas_tick_ms,
        failsafe_duty: 0,
    });
    let lcd_tick_ms = 1000 / global.engine_lcd_fps.max(1) as u64;
    let (lcd_cfg_tx, lcd_cfg_rx) = watch::channel(EngineRunConfig {
        enabled: global.engine_lcd_enabled,
        tick_ms: lcd_tick_ms,
        failsafe_duty: 0,
    });

    let fan_curve = engines::fan_curve::FanCurveEngine::new(app.clone());
    let canvas = engines::canvas::CanvasEngine::new(app.clone()).await;
    let lcd = engines::lcd::LcdEngine::new(app.clone());

    // FocusWatcher: create control channel, store tx in AppState
    let (focus_ctrl_tx, focus_ctrl_rx) = tokio::sync::mpsc::channel::<engines::focus_watcher::ControlMsg>(32);
    {
        let mut guard = app.focus_watcher_tx.lock().await;
        *guard = Some(focus_ctrl_tx);
    }

    let (focus_watcher_cfg_tx, focus_watcher_cfg_rx) = watch::channel(EngineRunConfig {
        enabled: true,
        tick_ms: 0,
        failsafe_duty: 0,
    });

    let focus_watcher = engines::focus_watcher::FocusWatcherEngine::new(app.clone());

    app.engines
        .set(Engines {
            canvas,
            fan_curve,
            lcd,
            fan_curve_cfg_tx,
            canvas_cfg_tx,
            lcd_cfg_tx,
            focus_watcher: focus_watcher.clone(),
            focus_watcher_cfg_tx,
        })
        .ok()
        .expect("engines already set");

    let fan_curve_handle = app.engines.get().unwrap().fan_curve.clone().start(fan_curve_cfg_rx);
    let canvas_handle = app.engines.get().unwrap().canvas.clone().start(canvas_cfg_rx).await;
    let lcd_handle = app.engines.get().unwrap().lcd.clone().start(lcd_cfg_rx).await;
    let focus_watcher_handle = focus_watcher.start(focus_watcher_cfg_rx, focus_ctrl_rx).await;

    // Start the key remap engine (action executor may fail to init on headless Linux).
    match engines::action_executor::ActionExecutor::new() {
        Ok(executor) => {
            let executor = Arc::new(executor);
            let remap_engine = Arc::new(engines::key_remap::KeyRemapEngine::new(executor, app.clone()));
            remap_engine.start();
        }
        Err(e) => {
            notify::warn(
                &app,
                "Key remapping unavailable",
                format!("Could not initialize action executor: {e}"),
            )
            .await;
        }
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Shutting down");
        }
        _ = app.shutdown.notified() => {
            log::info!("Shutdown requested via IPC");
        }
        r = ipc_handle => { if let Err(e) = r { log::error!("IPC: {e}"); } }
        r = broadcast_handle => { if let Err(e) = r { log::error!("Broadcast: {e}"); } }
        _ = fan_curve_handle => {
            notify::error(&app, "Engine stopped", "FanCurve engine exited unexpectedly — fan control will no longer respond to sensors.").await;
        }
        _ = canvas_handle => {
            notify::error(&app, "Engine stopped", "Canvas engine exited unexpectedly — RGB animations will stop.").await;
        }
        _ = lcd_handle => {
            notify::error(&app, "Engine stopped", "LCD engine exited unexpectedly — device LCDs will stop updating.").await;
        }
        _ = focus_watcher_handle => {
            log::info!("FocusWatcher engine exited");
        }
    }

    state::shutdown(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_runs_a_plain_server() {
        assert_eq!(
            process_role(&argv(&[])),
            ProcessRole::Server { worker: false }
        );
    }

    #[test]
    fn worker_flag_marks_a_worker_server() {
        assert_eq!(
            process_role(&argv(&["--worker"])),
            ProcessRole::Server { worker: true }
        );
    }

    #[test]
    fn service_control_flags_select_their_roles() {
        assert_eq!(
            process_role(&argv(&["--install-service"])),
            ProcessRole::InstallService
        );
        assert_eq!(
            process_role(&argv(&["--uninstall-service"])),
            ProcessRole::UninstallService
        );
        assert_eq!(process_role(&argv(&["--service"])), ProcessRole::Supervisor);
    }

    #[test]
    fn service_control_flags_take_precedence_over_worker() {
        // The installer / SCM flags win even when other arguments are present.
        assert_eq!(
            process_role(&argv(&["--worker", "--install-service"])),
            ProcessRole::InstallService
        );
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        assert_eq!(
            process_role(&argv(&["--frobnicate", "foo"])),
            ProcessRole::Server { worker: false }
        );
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
mod config;
mod constants;
mod cooling;
mod drivers;
mod input;
mod ipc;
mod lcd;
mod lifecycle;
mod lighting;
mod logger;
mod platform;
mod profiles;
mod registry;
mod run_loop;
mod secrets;
mod services;
mod state;
#[cfg(test)]
mod test_support;
mod util;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;

use crate::drivers::transports::hid;
use crate::registry::initialize_app_state;
use crate::state::EngineRunConfig;

/// How this process was invoked, decided purely from argv.
///
/// The daemon is always a plain user process now — Windows service registration
/// and the elevated register-bus broker live in `halod-broker.exe`, and the GUI
/// launches this daemon directly. The only knob is `--headless`, which opts out
/// of idle-shutdown (see [`crate::lifecycle`]) for a frontend-less deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessRole {
    Server { headless: bool },
}

fn process_role(args: &[String]) -> ProcessRole {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    ProcessRole::Server {
        headless: has(halod_shared::lifecycle::HEADLESS_ARG),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = process_role(&args);

    let headless = matches!(role, ProcessRole::Server { headless: true });

    // Before the runtime starts (still single-threaded, no env-reader race).
    #[cfg(target_os = "linux")]
    {
        let new_path = platform::env::augmented_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME"),
            std::env::var_os("USER"),
        );
        std::env::set_var("PATH", new_path);
    }

    let cfg = config::load()?;

    // Prevent glibc from creating one 64MB malloc arena per CPU core: the
    // daemon is I/O-bound and fragmentation across many arenas makes
    // `malloc_trim(0)` useless. Two arenas (main + 1 spare) is enough.
    // Must be `mallopt`, not the MALLOC_ARENA_MAX env var: glibc reads the
    // tunable before `main()` runs, so `set_var` here would be a no-op.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    runtime.block_on(run_daemon(headless, cfg))
}

/// Watches one engine's join handle and notifies the user if it exits other
/// than by a deliberate abort, without affecting any other engine.
fn spawn_engine_supervisor(
    app: Arc<state::AppState>,
    handle: tokio::task::JoinHandle<()>,
    title: &'static str,
    body: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = handle.await;
        // Aborted tasks are expected during clean shutdown; suppress notifications.
        if result.as_ref().is_err_and(|e| e.is_cancelled()) {
            return;
        }
        let panicked = result.is_err_and(|e| e.is_panic());
        if body.is_empty() {
            if panicked {
                log::error!("{title} (panicked)");
            } else {
                log::info!("{title}");
            }
        } else {
            platform::notify::send(
                &app,
                halod_shared::types::NotificationCode::EngineStopped {
                    detail: body.to_string(),
                },
            )
            .await;
        }
    })
}

/// The actual device server — always a plain, unprivileged user process.
/// On Windows, register-bus access is delegated to the elevated `halod-broker`
/// on demand (see `drivers::transports::register_ops`); nothing here elevates.
async fn run_daemon(headless: bool, cfg: crate::config::Config) -> Result<()> {
    let initial_level: log::LevelFilter = cfg
        .global
        .log_level
        .parse()
        .unwrap_or(log::LevelFilter::Info);

    let env_logger = env_logger::Builder::new()
        .filter_level(initial_level)
        .parse_default_env()
        .build();

    let app = Arc::new(state::AppState::new(cfg).with_secret_store(secrets::open_secret_store()));
    let _save_worker = crate::state::start_config_save_worker(app.clone());
    let _persist_worker = crate::state::start_persist_worker(app.clone());

    let buffering_logger = logger::BufferingLogger::new(env_logger, Arc::clone(&app.log_buffer));
    if let Err(e) = log::set_boxed_logger(Box::new(buffering_logger)) {
        log::warn!("logger already installed: {e}");
    }
    log::set_max_level(initial_level);

    log::info!(
        "Starting halod v{} (build {})",
        env!("CARGO_PKG_VERSION"),
        env!("HALOD_BUILD_HASH")
    );
    log::info!("Daemon is running. Press Ctrl+C to shut down.");

    // Must run before device discovery opens any HID handles: a second daemon
    // has to exit *before* it can fight the first over the hardware.
    ipc::ensure_single_instance()?;

    let ipc_handle = ipc::serve(app.clone());
    let broadcast_handle = ipc::broadcast_loop(app.clone());

    // Reclaim sinks leaked by a previous daemon; safe once single-instance owns.
    services::audio::sink::cleanup_orphaned_sinks().await;

    log::info!("Discovering devices...");
    initialize_app_state(app.clone()).await;
    log::info!("Device discovery complete");

    // Started only once startup is done — the grace clock must not elapse
    // before a client has had any chance to connect.
    let _idle_watcher = lifecycle::spawn_idle_watcher(app.clone(), headless);
    {
        let app2 = app.clone();
        let hotplug_handle = tokio::spawn(hid::hotplug_monitor(app2));
        tokio::spawn(async move {
            if let Err(e) = hotplug_handle.await {
                if !e.is_cancelled() {
                    log::warn!("hotplug monitor exited unexpectedly: {e}");
                }
            }
        });
    }

    let global = {
        let cfg = app.config.read().await;
        cfg.global.clone()
    };

    let (fan_curve_cfg_tx, fan_curve_cfg_rx) = watch::channel(EngineRunConfig::fan_curve(&global));
    let (failsafe_duty_tx, failsafe_duty_rx) = watch::channel(global.fan_failsafe_duty);
    let (rgb_cfg_tx, rgb_cfg_rx) = watch::channel(EngineRunConfig::canvas(&global));
    let (lcd_cfg_tx, lcd_cfg_rx) = watch::channel(EngineRunConfig::lcd(&global));

    let fan_curve = cooling::fan_curve::FanCurveEngine::new(app.clone());
    let rgb = lighting::rgb_engine::RgbEngine::new(app.clone()).await;
    let lcd = lcd::engine::LcdEngine::new(app.clone());
    let video = lcd::engine::video::VideoEngine::new(app.clone(), lcd.frame_sender());

    let (focus_ctrl_tx, focus_ctrl_rx) =
        tokio::sync::mpsc::channel::<profiles::focus_watcher::ControlMsg>(32);
    app.focus.set_ctrl_tx(focus_ctrl_tx).await;

    let (focus_watcher_cfg_tx, focus_watcher_cfg_rx) =
        watch::channel(EngineRunConfig::focus_watcher(&global));

    let focus_watcher = profiles::focus_watcher::FocusWatcherEngine::new(app.clone());

    app.lighting.set_engine(rgb.clone(), rgb_cfg_tx);
    app.cooling
        .set_engine(fan_curve.clone(), fan_curve_cfg_tx, failsafe_duty_tx);
    app.lcd.set_engine(lcd.clone(), video, lcd_cfg_tx);
    app.focus
        .set_engine(focus_watcher.clone(), focus_watcher_cfg_tx);
    // Receivers are subscribed lazily; SendError just means no live receivers yet,
    // but watch still stores the value so future subscribers see `true`.
    let _ = app.engines_ready.send(true);

    let fan_curve_handle = fan_curve.start(fan_curve_cfg_rx, failsafe_duty_rx);
    let rgb_handle = rgb.start(rgb_cfg_rx).await;
    let lcd_handle = lcd.start(lcd_cfg_rx).await;
    let focus_watcher_handle = focus_watcher
        .start(focus_watcher_cfg_rx, focus_ctrl_rx)
        .await;

    // Engines are supervised independently so one exiting can't take the others down.
    let engine_aborts = [
        fan_curve_handle.abort_handle(),
        rgb_handle.abort_handle(),
        lcd_handle.abort_handle(),
        focus_watcher_handle.abort_handle(),
    ];
    let supervise = |handle, title, body| spawn_engine_supervisor(app.clone(), handle, title, body);
    supervise(
        fan_curve_handle,
        "FanCurve engine exited",
        "FanCurve engine exited unexpectedly, fan control will no longer respond to sensors.",
    );
    supervise(
        rgb_handle,
        "RGB engine exited",
        "RGB engine exited unexpectedly, RGB animations will stop.",
    );
    supervise(
        lcd_handle,
        "LCD engine exited",
        "LCD engine exited unexpectedly, device LCDs will stop updating.",
    );
    supervise(focus_watcher_handle, "FocusWatcher engine exited", "");

    // Action executor may fail to init on headless Linux.
    match input::action_executor::ActionExecutor::new() {
        Ok(executor) => {
            let executor = Arc::new(executor);
            app.input.set_executor(Arc::clone(&executor));
            let remap_engine =
                Arc::new(input::key_remap::KeyRemapEngine::new(executor, app.clone()));
            let handle = remap_engine.start();
            // KeyRemapEngine has no config channel; supervise its handle.
            tokio::spawn(async move {
                if let Err(e) = handle.await {
                    if !e.is_cancelled() {
                        log::warn!("KeyRemapEngine exited unexpectedly: {e}");
                    }
                }
            });
        }
        Err(e) => {
            platform::notify::send(
                &app,
                halod_shared::types::NotificationCode::KeyRemapUnavailable {
                    detail: e.to_string(),
                },
            )
            .await;
        }
    }

    // Handle SIGTERM so state::shutdown runs before exit (resource cleanup)
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let sigterm_fut = async {
        #[cfg(unix)]
        {
            sigterm.recv().await;
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(sigterm_fut);

    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            if let Err(e) = r { log::warn!("ctrl_c signal error: {e}"); }
            log::info!("Shutting down (SIGINT)");
        }
        _ = &mut sigterm_fut => {
            log::info!("Shutting down (SIGTERM)");
        }
        _ = app.shutdown.notified() => {
            log::info!("Shutdown requested via IPC");
        }
        r = ipc_handle => { if let Err(e) = r { log::error!("IPC: {e}"); } }
        r = broadcast_handle => { if let Err(e) = r { log::error!("Broadcast: {e}"); } }
    }

    // Stop engines before closing devices so no tick writes mid-close.
    for abort in &engine_aborts {
        abort.abort();
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
            ProcessRole::Server { headless: false }
        );
    }

    #[test]
    fn headless_flag_marks_a_headless_server() {
        assert_eq!(
            process_role(&argv(&["--headless"])),
            ProcessRole::Server { headless: true }
        );
    }

    #[test]
    fn former_service_and_worker_flags_are_now_ignored() {
        // Service registration + the elevated broker moved to halod-broker.exe;
        // these flags no longer mean anything to the daemon.
        for flag in [
            "--service",
            "--install-service",
            "--uninstall-service",
            "--worker",
        ] {
            assert_eq!(
                process_role(&argv(&[flag])),
                ProcessRole::Server { headless: false },
                "{flag} should be ignored",
            );
        }
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        assert_eq!(
            process_role(&argv(&["--frobnicate", "foo"])),
            ProcessRole::Server { headless: false }
        );
    }

    #[tokio::test]
    async fn aborting_one_supervised_engine_does_not_affect_another() {
        let app = Arc::new(state::AppState::new(crate::config::Config::default()));

        let never_ending = tokio::spawn(std::future::pending::<()>());
        let survivor_handle = tokio::spawn(std::future::pending::<()>());

        let aborted_sup = spawn_engine_supervisor(app.clone(), never_ending, "aborted", "");
        // Give the supervised task a moment to start awaiting the join handle.
        tokio::task::yield_now().await;

        aborted_sup.abort();
        let _ = aborted_sup.await;

        // The other engine's supervisor is untouched and the task keeps running.
        assert!(!survivor_handle.is_finished());
        survivor_handle.abort();
    }
}

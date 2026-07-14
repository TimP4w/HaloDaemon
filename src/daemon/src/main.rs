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
mod task_supervisor;
#[cfg(test)]
mod test_support;
mod util;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;

use crate::drivers::transports::hid;
use crate::registry::initialize_app_state_with_dev_repo;
use crate::state::EngineRunConfig;

/// How this process was invoked, decided purely from argv.
///
/// The daemon is always a plain user process now — Windows service registration
/// and the elevated register-bus broker live in `halod-broker.exe`, and the GUI
/// launches this daemon directly. `--headless` opts out of idle-shutdown (see
/// [`crate::lifecycle`]); `--dev-plugin-repo <DIR>` replaces the official
/// plugin checkout with a directly loaded working tree for this process.
///
/// `plugin-test <package-dir>` (behind the `plugin-test` cargo feature) is a
/// separate mode entirely: it drives one plugin package's `test.lua` against
/// a recording mock transport (see `drivers::plugins::plugin_test`) and never
/// touches config, device discovery, or the engines — the official plugin
/// repo's CI runs it once per package, not the daemon proper.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessRole {
    Server {
        headless: bool,
        dev_plugin_repo: Option<std::path::PathBuf>,
    },
    #[cfg(feature = "plugin-test")]
    PluginTest {
        package: std::path::PathBuf,
    },
}

fn process_role(args: &[String]) -> ProcessRole {
    #[cfg(feature = "plugin-test")]
    if args.first().map(String::as_str) == Some("plugin-test") {
        let package = args
            .get(1)
            .unwrap_or_else(|| {
                eprintln!("usage: halod plugin-test <package-dir>");
                std::process::exit(2);
            })
            .into();
        return ProcessRole::PluginTest { package };
    }
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let mut dev_plugin_repo = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dev-plugin-repo" {
            let Some(path) = args.get(i + 1) else {
                eprintln!("usage: halod [--headless] [--dev-plugin-repo <DIR>]");
                std::process::exit(2);
            };
            if dev_plugin_repo.replace(std::path::PathBuf::from(path)).is_some() {
                eprintln!("--dev-plugin-repo may only be provided once");
                std::process::exit(2);
            }
            i += 1;
        }
        i += 1;
    }
    ProcessRole::Server {
        headless: has(halod_shared::lifecycle::HEADLESS_ARG),
        dev_plugin_repo,
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = process_role(&args);

    #[cfg(feature = "plugin-test")]
    if let ProcessRole::PluginTest { package } = &role {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        let exit_code = drivers::plugins::plugin_test::run(handle, package)?;
        std::process::exit(exit_code);
    }

    let (headless, dev_plugin_repo) = match role {
        ProcessRole::Server {
            headless,
            dev_plugin_repo,
        } => (headless, dev_plugin_repo),
        #[cfg(feature = "plugin-test")]
        ProcessRole::PluginTest { .. } => unreachable!("handled above"),
    };

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
    runtime.block_on(run_daemon(headless, dev_plugin_repo, cfg))
}

/// The actual device server — always a plain, unprivileged user process.
/// On Windows, register-bus access is delegated to the elevated `halod-broker`
/// on demand (see `drivers::transports::register_ops`); nothing here elevates.
async fn run_daemon(
    headless: bool,
    dev_plugin_repo: Option<std::path::PathBuf>,
    cfg: crate::config::Config,
) -> Result<()> {
    let initial_level: log::LevelFilter =
        cfg.gui.log_level.parse().unwrap_or(log::LevelFilter::Info);

    let env_logger = env_logger::Builder::new()
        .filter_level(initial_level)
        .parse_default_env()
        .build();

    let app = Arc::new(state::AppState::new(cfg).with_secret_store(secrets::open_secret_store()));
    let save_worker = crate::state::start_config_save_worker(app.clone());
    let persist_worker = crate::state::start_persist_worker(app.clone());

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

    #[cfg(unix)]
    platform::elevation::warn_if_elevated();

    // Must run before device discovery opens any HID handles: a second daemon
    // has to exit *before* it can fight the first over the hardware.
    ipc::ensure_single_instance()?;

    let ipc_handle = ipc::serve(app.clone());
    let broadcast_handle = ipc::broadcast_loop(app.clone());
    let mut supervisor = task_supervisor::TaskSupervisor::new(Arc::clone(&app));

    // Reclaim sinks leaked by a previous daemon; safe once single-instance owns.
    services::audio::sink::cleanup_orphaned_sinks().await;

    log::info!("Discovering devices...");
    initialize_app_state_with_dev_repo(app.clone(), dev_plugin_repo).await;
    log::info!("Device discovery complete");

    // Started only once startup is done — the grace clock must not elapse
    // before a client has had any chance to connect.
    let idle_app = Arc::clone(&app);
    supervisor.register(
        "Idle watcher",
        "",
        move || {
            let app = Arc::clone(&idle_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(lifecycle::spawn_idle_watcher(app, headless))
            })
        },
        || {},
    );
    let hotplug_app = Arc::clone(&app);
    supervisor.register(
        "HID hotplug monitor",
        "Device hotplug monitoring stopped unexpectedly.",
        move || {
            let app = Arc::clone(&hotplug_app);
            Box::pin(
                async move { task_supervisor::TaskHandle(tokio::spawn(hid::hotplug_monitor(app))) },
            )
        },
        || {},
    );

    // Reconnect/liveness watcher for config-instantiated integration plugins
    // (offline-at-startup, mid-run drops, controller add/remove). Runs beside
    // the HID hotplug monitor.
    let integration_app = Arc::clone(&app);
    supervisor.register(
        "Plugin integration monitor",
        "Plugin integration monitoring stopped unexpectedly.",
        move || {
            let app = Arc::clone(&integration_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(
                    drivers::plugins::integration_monitor::integration_monitor(app),
                ))
            })
        },
        || {},
    );

    let (cooling_cfg, rgb_cfg, lcd_cfg) = {
        let cfg = app.config.read().await;
        (cfg.cooling.clone(), cfg.rgb.clone(), cfg.lcd.clone())
    };

    let (fan_curve_cfg_tx, fan_curve_cfg_rx) =
        watch::channel(EngineRunConfig::fan_curve(&cooling_cfg));
    let (failsafe_duty_tx, failsafe_duty_rx) = watch::channel(cooling_cfg.fan_failsafe_duty);
    let (rgb_cfg_tx, rgb_cfg_rx) = watch::channel(EngineRunConfig::canvas(&rgb_cfg));
    let (lcd_cfg_tx, lcd_cfg_rx) = watch::channel(EngineRunConfig::lcd(&lcd_cfg));

    let fan_curve = cooling::fan_curve::FanCurveEngine::new(app.clone());
    let rgb = lighting::rgb_engine::RgbEngine::new(app.clone()).await;
    let lcd = lcd::engine::LcdEngine::new(app.clone());
    let video = lcd::engine::video::VideoEngine::new(app.clone(), lcd.frame_sender());

    let focus_watcher = profiles::focus_watcher::FocusWatcherEngine::new(app.clone());

    app.lighting.set_engine(rgb.clone(), rgb_cfg_tx.clone());
    app.cooling
        .set_engine(fan_curve_cfg_tx.clone(), failsafe_duty_tx.clone());
    app.lcd.set_engine(lcd.clone(), video, lcd_cfg_tx.clone());
    // Receivers are subscribed lazily; SendError just means no live receivers yet,
    // but watch still stores the value so future subscribers see `true`.
    let _ = app.engines_ready.send(true);

    drop((fan_curve_cfg_rx, failsafe_duty_rx, rgb_cfg_rx, lcd_cfg_rx));
    let fan_engine = Arc::clone(&fan_curve);
    let fan_cfg = fan_curve_cfg_tx.clone();
    let fan_failsafe = failsafe_duty_tx.clone();
    supervisor.register(
        "FanCurve engine",
        "FanCurve engine exited unexpectedly; fan control will no longer respond to sensors.",
        move || {
            let engine = Arc::clone(&fan_engine);
            let cfg = fan_cfg.subscribe();
            let failsafe = fan_failsafe.subscribe();
            Box::pin(async move { task_supervisor::TaskHandle(engine.start(cfg, failsafe)) })
        },
        || {},
    );
    let rgb_engine = Arc::clone(&rgb);
    let rgb_config = rgb_cfg_tx.clone();
    supervisor.register(
        "RGB engine",
        "RGB engine exited unexpectedly; RGB animations will stop.",
        move || {
            let engine = Arc::clone(&rgb_engine);
            let cfg = rgb_config.subscribe();
            Box::pin(async move { task_supervisor::TaskHandle(engine.start(cfg).await) })
        },
        || {},
    );
    let lcd_engine = Arc::clone(&lcd);
    let lcd_config = lcd_cfg_tx.clone();
    supervisor.register(
        "LCD engine",
        "LCD engine exited unexpectedly; device LCDs will stop updating.",
        move || {
            let engine = Arc::clone(&lcd_engine);
            let cfg = lcd_config.subscribe();
            Box::pin(async move { task_supervisor::TaskHandle(engine.start(cfg).await) })
        },
        || {},
    );
    let focus_engine = Arc::clone(&focus_watcher);
    let focus_app = Arc::clone(&app);
    supervisor.register(
        "Focus watcher",
        "",
        move || {
            let engine = Arc::clone(&focus_engine);
            let app = Arc::clone(&focus_app);
            Box::pin(async move {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<profiles::focus_watcher::ControlMsg>(32);
                app.focus.set_ctrl_tx(tx).await;
                task_supervisor::TaskHandle(engine.start(rx).await)
            })
        },
        || {},
    );

    // Action executor may fail to init on headless Linux.
    match input::action_executor::ActionExecutor::new() {
        Ok(executor) => {
            let executor = Arc::new(executor);
            app.input.set_executor(Arc::clone(&executor));
            let remap_engine =
                Arc::new(input::key_remap::KeyRemapEngine::new(executor, app.clone()));
            let remap_start = Arc::clone(&remap_engine);
            let remap_app = Arc::clone(&app);
            supervisor.register(
                "Key remap engine",
                "Key remapping stopped unexpectedly.",
                move || {
                    let engine = Arc::clone(&remap_start);
                    Box::pin(async move { task_supervisor::TaskHandle(engine.start()) })
                },
                move || remap_app.input.shutdown(),
            );
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

    // Stop every supervised task before closing devices so no engine writes mid-close.
    supervisor.shutdown().await;
    if let Some(executor) = app.input.executor() {
        executor.shutdown().await;
    }

    // Persistence owns its shutdown transition and performs one final flush of
    // the newest version before it exits. The device-state worker has no disk
    // queue of its own and can be cancelled after engines stop producing work.
    app.persistence.shutdown_tx.send_replace(true);
    if tokio::time::timeout(std::time::Duration::from_secs(5), save_worker)
        .await
        .is_err()
    {
        log::error!("Config persistence worker did not finish its shutdown flush");
    }
    persist_worker.abort();
    let _ = persist_worker.await;

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
            ProcessRole::Server { headless: false, dev_plugin_repo: None }
        );
    }

    #[test]
    fn headless_flag_marks_a_headless_server() {
        assert_eq!(
            process_role(&argv(&["--headless"])),
            ProcessRole::Server { headless: true, dev_plugin_repo: None }
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
                ProcessRole::Server { headless: false, dev_plugin_repo: None },
                "{flag} should be ignored",
            );
        }
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        assert_eq!(
            process_role(&argv(&["--frobnicate", "foo"])),
            ProcessRole::Server { headless: false, dev_plugin_repo: None }
        );
    }

    #[test]
    fn development_plugin_repository_is_parsed() {
        assert_eq!(
            process_role(&argv(&["--dev-plugin-repo", "C:/plugins"])),
            ProcessRole::Server {
                headless: false,
                dev_plugin_repo: Some("C:/plugins".into()),
            }
        );
    }
}

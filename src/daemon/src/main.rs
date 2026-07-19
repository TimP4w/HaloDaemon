// SPDX-License-Identifier: GPL-3.0-or-later
mod application;
mod config;
mod constants;
mod domain;
mod embedded_plugins {
    include!(concat!(env!("OUT_DIR"), "/embedded_plugin_bundle.rs"));
}
mod infrastructure;
mod logger;
#[cfg(test)]
mod test_support;
mod util;

use anyhow::Result;
use std::sync::Arc;

use crate::application::run_loop::{EngineConfigReceiver, EngineConfigTopic};
use crate::application::state;
use crate::application::{ipc, lifecycle, task_supervisor};
use crate::domain::{cooling, device, input, lcd, lighting, plugin, profiles};
use crate::infrastructure::{platform, secrets};

/// How this process was invoked, decided purely from argv.
///
/// The daemon is always a plain user process now — Windows service registration
/// and the elevated register-bus broker live in `halod-broker.exe`, and the GUI
/// launches this daemon directly. `--headless` opts out of idle-shutdown (see
/// [`crate::application::lifecycle`]); the development-only `--dev-plugin-repo <DIR>` flag
/// loads a directly supplied working tree as an extra plugin source alongside
/// the official, local, and configured repos, winning any id collisions.
///
/// `plugin-test <package-dir>` (behind the `plugin-test` cargo feature) is a
/// separate mode entirely: it drives one plugin package's `test.lua` against
/// a recording mock transport (see `plugin::plugin_test`) and never
/// touches config, device discovery, or the engines — the official plugin
/// repo's CI runs it once per package, not the daemon proper.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessRole {
    Server {
        headless: bool,
        #[cfg(feature = "dev-plugin-repo")]
        dev_plugin_repo: Option<std::path::PathBuf>,
    },
    #[cfg(feature = "plugin-test")]
    PluginTest {
        package: std::path::PathBuf,
    },
    UdevRules {
        embedded: bool,
    },
}

#[cfg(feature = "dev-plugin-repo")]
const USAGE: &str = "usage: halod [--headless] [--dev-plugin-repo <DIR>] | halod udev-rules";
#[cfg(not(feature = "dev-plugin-repo"))]
const USAGE: &str = "usage: halod [--headless] | halod udev-rules";

fn process_role(args: &[String]) -> std::result::Result<ProcessRole, String> {
    if args.first().map(String::as_str) == Some("udev-rules") {
        let embedded = args.get(1).is_some_and(|arg| arg == "--embedded");
        if args.len() != 1 + usize::from(embedded) {
            return Err("usage: halod udev-rules [--embedded]".to_owned());
        }
        return Ok(ProcessRole::UdevRules { embedded });
    }
    #[cfg(feature = "plugin-test")]
    if args.first().map(String::as_str) == Some("plugin-test") {
        let package = args
            .get(1)
            .ok_or_else(|| "usage: halod plugin-test <package-dir>".to_owned())?
            .into();
        if args.len() != 2 {
            return Err("usage: halod plugin-test <package-dir>".to_owned());
        }
        return Ok(ProcessRole::PluginTest { package });
    }
    let mut headless = false;
    #[cfg(feature = "dev-plugin-repo")]
    let mut dev_plugin_repo = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => {
                if headless {
                    return Err("--headless may only be provided once".to_owned());
                }
                headless = true;
            }
            #[cfg(feature = "dev-plugin-repo")]
            "--dev-plugin-repo" => {
                let Some(path) = args.get(i + 1) else {
                    return Err("--dev-plugin-repo requires a directory".to_owned());
                };
                if path.starts_with('-')
                    || dev_plugin_repo
                        .replace(std::path::PathBuf::from(path))
                        .is_some()
                {
                    return Err(
                        "--dev-plugin-repo requires one directory and may only be provided once"
                            .to_owned(),
                    );
                }
                i += 1;
            }
            _ => {
                return Err(format!("unknown argument '{}'", args[i]));
            }
        }
        i += 1;
    }
    Ok(ProcessRole::Server {
        headless,
        #[cfg(feature = "dev-plugin-repo")]
        dev_plugin_repo,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = process_role(&args).unwrap_or_else(|error| {
        eprintln!("{error}\n{USAGE}");
        std::process::exit(2);
    });

    #[cfg(feature = "plugin-test")]
    if let ProcessRole::PluginTest { package } = &role {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        let exit_code = plugin::plugin_test::run(handle, package)?;
        std::process::exit(exit_code);
    }

    if let ProcessRole::UdevRules { embedded } = role {
        let rules = if embedded {
            let bytes = embedded_plugins::BUNDLE
                .ok_or_else(|| anyhow::anyhow!("this build has no embedded plugin bundle"))?;
            plugin::udev_rules_from_bundle(bytes)?
        } else {
            let cfg = config::load()?;
            let registry = plugin::Registry::default();
            registry.load_all_with_priority_repo(
                &config::plugins_dir(),
                None,
                &plugin::repo_plugin_sources(&cfg.plugins.repos),
            );
            registry.udev_rules()
        };
        print!("{rules}");
        return Ok(());
    }

    let headless = match &role {
        ProcessRole::Server { headless, .. } => *headless,
        ProcessRole::UdevRules { .. } => unreachable!("handled above"),
        #[cfg(feature = "plugin-test")]
        ProcessRole::PluginTest { .. } => unreachable!("handled above"),
    };
    #[cfg(feature = "dev-plugin-repo")]
    let dev_plugin_repo = match role {
        ProcessRole::Server {
            dev_plugin_repo, ..
        } => dev_plugin_repo,
        ProcessRole::UdevRules { .. } => unreachable!("handled above"),
        #[cfg(feature = "plugin-test")]
        ProcessRole::PluginTest { .. } => unreachable!("handled above"),
    };
    #[cfg(feature = "dev-plugin-repo")]
    let dev_plugin_repo = dev_plugin_repo.map(|path| {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|e| {
            eprintln!("invalid --dev-plugin-repo '{}': {e}", path.display());
            std::process::exit(2);
        });
        canonical
    });

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
    runtime.block_on(run_daemon(
        headless,
        #[cfg(feature = "dev-plugin-repo")]
        dev_plugin_repo,
        cfg,
    ))
}

/// The actual device server — always a plain, unprivileged user process.
/// On Windows, register-bus access is delegated to the elevated `halod-broker`
/// on demand (see `drivers::transports::register_ops`); nothing here elevates.
async fn run_daemon(
    headless: bool,
    #[cfg(feature = "dev-plugin-repo")] dev_plugin_repo: Option<std::path::PathBuf>,
    cfg: crate::config::Config,
) -> Result<()> {
    let initial_level: log::LevelFilter =
        cfg.gui.log_level.parse().unwrap_or(log::LevelFilter::Info);

    let app = Arc::new(
        application::state::AppState::new(cfg).with_secret_store(secrets::open_secret_store()),
    );
    let save_worker = crate::application::state::start_config_save_worker(app.clone());
    let persist_worker = crate::application::state::start_persist_worker(app.clone());

    logger::init(crate::config::config_dir().join("halod.log"), initial_level);

    log::info!(
        "Starting halod v{} (build {})",
        env!("CARGO_PKG_VERSION"),
        env!("HALOD_BUILD_HASH")
    );
    log::info!("Daemon is running. Press Ctrl+C to shut down.");

    #[cfg(unix)]
    platform::elevation::refuse_if_elevated()?;

    // Must run before device discovery opens any HID handles: a second daemon
    // has to exit *before* it can fight the first over the hardware.
    ipc::ensure_single_instance()?;

    let ipc_handle = ipc::serve(app.clone());
    let mut supervisor = task_supervisor::TaskSupervisor::new(Arc::clone(&app));

    // Reclaim sinks leaked by a previous daemon; safe once single-instance owns.
    infrastructure::audio::sink::cleanup_orphaned_sinks().await;

    log::info!("Discovering devices...");
    crate::domain::registry::initialize_app_state(
        app.clone(),
        #[cfg(feature = "dev-plugin-repo")]
        dev_plugin_repo,
    )
    .await;
    log::info!("Device discovery complete");

    // Prime the requirement cache so the first UI poll doesn't trigger a probe
    // per plugin; refreshed thereafter only at reconcile (no continuous polling).
    crate::domain::plugin::usecases::plugins::refresh_requirements(&app).await?;

    // Compute passive HID/USB/SMBus recommendations once at startup. The same
    // snapshot is refreshed when repository manifests change.
    crate::domain::plugin::usecases::plugins::refresh_recommendations(&app).await;
    crate::domain::registry::usecases::runtime::bootstrap(&app).await;

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
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(
                    application::observers::hid::hotplug_monitor(app),
                ))
            })
        },
        || {},
    );

    // Liveness watcher for plugin devices whose primary transport is a plain USB
    // device (e.g. the Philips Evnia). HID hotplug does not cover them, so a
    // disconnect would otherwise strand a stale handle forever.
    let usb_hotplug_app = Arc::clone(&app);
    supervisor.register(
        "USB hotplug monitor",
        "USB device hotplug monitoring stopped unexpectedly.",
        move || {
            let app = Arc::clone(&usb_hotplug_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(
                    crate::application::observers::usb_hotplug::run(app),
                ))
            })
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
                    plugin::observers::integration_monitor::integration_monitor(app),
                ))
            })
        },
        || {},
    );

    let sensor_app = Arc::clone(&app);
    supervisor.register(
        "Sensor data producer",
        "Host sensor snapshots stopped updating unexpectedly.",
        move || {
            let app = Arc::clone(&sensor_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(device::usecases::telemetry::run(app)))
            })
        },
        || {},
    );

    let plugin_data_app = Arc::clone(&app);
    supervisor.register(
        "Plugin data status producer",
        "Plugin data status projection stopped unexpectedly.",
        move || {
            let app = Arc::clone(&plugin_data_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(plugin::observers::data_status::run(app)))
            })
        },
        || {},
    );

    let battery_app = Arc::clone(&app);
    supervisor.register(
        "Low battery watcher",
        "Low battery monitoring stopped unexpectedly.",
        move || {
            let app = Arc::clone(&battery_app);
            Box::pin(async move {
                task_supervisor::TaskHandle(tokio::spawn(device::policies::low_battery::watcher(
                    app,
                )))
            })
        },
        || {},
    );

    let fan_curve = cooling::engine::fan_curve::FanCurveEngine::new(app.clone());
    let rgb = lighting::engine::RgbEngine::new(app.clone()).await;
    let lcd = lcd::engine::LcdEngine::new(app.clone());
    let video = lcd::engine::video::VideoEngine::new(app.clone(), lcd.frame_sender());

    let focus_watcher = profiles::observers::active_window::FocusWatcherEngine::new(app.clone());

    app.lighting.set_engine(rgb.clone());
    app.cooling.set_engine();
    app.lcd.set_engine(lcd.clone(), video);
    // Receivers are subscribed lazily; SendError just means no live receivers yet,
    // but watch still stores the value so future subscribers see `true`.
    let _ = app.engines_ready.send(true);

    let fan_engine = Arc::clone(&fan_curve);
    let fan_bus = app.data_bus.clone();
    supervisor.register(
        "FanCurve engine",
        "FanCurve engine exited unexpectedly; fan control will no longer respond to sensors.",
        move || {
            let engine = Arc::clone(&fan_engine);
            let cfg = EngineConfigReceiver::new(fan_bus.clone(), EngineConfigTopic::Cooling);
            Box::pin(async move { task_supervisor::TaskHandle(engine.start(cfg)) })
        },
        || {},
    );
    let lighting_engine = Arc::clone(&rgb);
    let rgb_bus = app.data_bus.clone();
    supervisor.register(
        "RGB engine",
        "RGB engine exited unexpectedly; RGB animations will stop.",
        move || {
            let engine = Arc::clone(&lighting_engine);
            let cfg = EngineConfigReceiver::new(rgb_bus.clone(), EngineConfigTopic::Lighting);
            Box::pin(async move { task_supervisor::TaskHandle(engine.start(cfg).await) })
        },
        || {},
    );
    let lcd_engine = Arc::clone(&lcd);
    let lcd_bus = app.data_bus.clone();
    supervisor.register(
        "LCD engine",
        "LCD engine exited unexpectedly; device LCDs will stop updating.",
        move || {
            let engine = Arc::clone(&lcd_engine);
            let cfg = EngineConfigReceiver::new(lcd_bus.clone(), EngineConfigTopic::Lcd);
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
                let (tx, rx) = tokio::sync::mpsc::channel::<
                    profiles::observers::active_window::ControlMsg,
                >(32);
                app.focus.set_ctrl_tx(tx).await;
                task_supervisor::TaskHandle(engine.start(rx).await)
            })
        },
        || {},
    );

    // Action executor may fail to init on headless Linux.
    match input::engine::action_executor::ActionExecutor::new() {
        Ok(executor) => {
            let executor = Arc::new(executor);
            app.input.set_executor(Arc::clone(&executor));
            let remap_engine = Arc::new(input::engine::key_remap::KeyRemapEngine::new(
                executor,
                app.clone(),
            ));
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
            application::notifications::send(
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
    }

    // Stop every supervised task before closing devices so no engine writes mid-close.
    supervisor.shutdown().await;
    if let Some(executor) = app.input.executor() {
        executor.shutdown().await;
    }

    // Persistence owns its shutdown transition and performs one final flush of
    // the newest version before it exits. The device-state worker has no disk
    // queue of its own and can be cancelled after engines stop producing work.
    app.config.persistence().shutdown_tx.send_replace(true);
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
            process_role(&argv(&[])).unwrap(),
            ProcessRole::Server {
                headless: false,
                #[cfg(feature = "dev-plugin-repo")]
                dev_plugin_repo: None
            }
        );
    }

    #[test]
    fn headless_flag_marks_a_headless_server() {
        assert_eq!(
            process_role(&argv(&["--headless"])).unwrap(),
            ProcessRole::Server {
                headless: true,
                #[cfg(feature = "dev-plugin-repo")]
                dev_plugin_repo: None
            }
        );
    }

    #[test]
    fn udev_rules_is_a_standalone_runtime_command() {
        assert_eq!(
            process_role(&argv(&["udev-rules"])).unwrap(),
            ProcessRole::UdevRules { embedded: false }
        );
        assert_eq!(
            process_role(&argv(&["udev-rules", "--embedded"])).unwrap(),
            ProcessRole::UdevRules { embedded: true }
        );
        assert!(process_role(&argv(&["udev-rules", "extra"])).is_err());
    }

    #[cfg(feature = "dev-plugin-repo")]
    #[test]
    fn development_plugin_repository_is_parsed() {
        assert_eq!(
            process_role(&argv(&["--dev-plugin-repo", "C:/plugins"])).unwrap(),
            ProcessRole::Server {
                headless: false,
                dev_plugin_repo: Some("C:/plugins".into()),
            }
        );
    }

    #[test]
    fn invalid_server_arguments_return_usage_errors() {
        let mut cases = vec![vec!["--unknown"], vec!["--headless", "--headless"]];
        #[cfg(feature = "dev-plugin-repo")]
        cases.extend([
            vec!["--dev-plugin-repo"],
            vec!["--dev-plugin-repo", "--headless"],
            vec!["--dev-plugin-repo", "one", "--dev-plugin-repo", "two"],
        ]);
        #[cfg(not(feature = "dev-plugin-repo"))]
        cases.push(vec!["--dev-plugin-repo", "C:/plugins"]);
        for args in cases {
            assert!(process_role(&argv(&args)).is_err(), "{args:?}");
        }
    }
}

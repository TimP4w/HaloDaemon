// SPDX-License-Identifier: GPL-3.0-or-later
mod application;
mod config;
mod constants;
mod domain;
mod infrastructure;
mod logger;
#[cfg(test)]
mod test_support;
mod util;

use anyhow::Result;
use std::sync::Arc;

use crate::application::run_loop::EngineConfigReceiver;
use crate::application::state;
use crate::application::{ipc, lifecycle, task_supervisor};
use crate::domain::{cooling, device, input, lcd, lighting, plugin, profiles};
#[cfg(unix)]
use crate::infrastructure::platform;
use crate::infrastructure::secrets;
use crate::util::task::TaskHandle;

/// How this process was invoked, decided purely from argv.
///
/// The daemon is always a plain user process now — Windows service registration
/// and the elevated register-bus broker live in `halod-broker.exe`, and the GUI
/// launches this daemon directly. `--headless` opts out of idle-shutdown (see
/// [`crate::application::lifecycle`]). `--dev-plugins <dir>` (or the
/// `HALOD_DEV_PLUGINS` environment variable) loads plugins only from a local
/// directory; see [`crate::config::enable_dev_plugins`].
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
        dev_plugins: Option<std::path::PathBuf>,
    },
    #[cfg(feature = "plugin-test")]
    PluginTest {
        package: std::path::PathBuf,
    },
    UdevRules {
        catalog: Option<String>,
    },
}

const USAGE: &str =
    "usage: halod [--headless] [--dev-plugins <dir>] | halod udev-rules [--catalog <url>]";

fn process_role(args: &[String]) -> std::result::Result<ProcessRole, String> {
    if args.first().map(String::as_str) == Some("udev-rules") {
        return match &args[1..] {
            [] => Ok(ProcessRole::UdevRules { catalog: None }),
            [flag, url] if flag == "--catalog" => Ok(ProcessRole::UdevRules {
                catalog: Some(url.clone()),
            }),
            _ => Err("usage: halod udev-rules [--catalog <url>]".to_owned()),
        };
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
    let (mut headless, mut dev_plugins) = (false, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--headless" if !headless => headless = true,
            "--dev-plugins" if dev_plugins.is_none() => {
                let dir = rest
                    .next()
                    .ok_or_else(|| "--dev-plugins needs a directory".to_owned())?;
                dev_plugins = Some(dir.into());
            }
            _ => return Err(format!("unknown arguments {args:?}")),
        }
    }
    Ok(ProcessRole::Server {
        headless,
        dev_plugins,
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

    if let ProcessRole::UdevRules { catalog } = role {
        let rules = match catalog {
            Some(url) => plugin::udev_rules_from_catalog(&url)?,
            None => {
                let registry = plugin::Registry::default();
                registry.load_all(&config::load()?.plugins.repos);
                registry.udev_rules()
            }
        };
        print!("{rules}");
        return Ok(());
    }

    let (headless, dev_plugins) = match &role {
        ProcessRole::Server {
            headless,
            dev_plugins,
        } => (
            *headless,
            dev_plugins
                .clone()
                .or_else(|| std::env::var_os(config::DEV_PLUGINS_ENV).map(Into::into)),
        ),
        ProcessRole::UdevRules { .. } => unreachable!("handled above"),
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

    if let Some(dir) = &dev_plugins {
        config::enable_dev_plugins(dir)?;
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

/// The actual device server — always a plain, unprivileged user process.
/// On Windows, register-bus access is delegated to the elevated `halod-broker`
/// on demand (see `drivers::transports::register_ops`); nothing here elevates.
async fn run_daemon(headless: bool, cfg: crate::config::Config) -> Result<()> {
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
    if let Some(dir) = crate::config::dev_plugins_dir() {
        log::warn!(
            "DEV PLUGIN MODE: loading plugins only from {}; plugin repositories are not loaded or fetched",
            dir.display()
        );
    }

    #[cfg(unix)]
    platform::elevation::refuse_if_elevated()?;

    // Must run before device discovery opens any HID handles: a second daemon
    // has to exit *before* it can fight the first over the hardware.
    ipc::ensure_single_instance()?;

    // IPC snapshots must never represent an absent GUI topic as default
    // English. Seed the config loaded above before a client can subscribe.
    crate::application::usecases::registry::runtime::publish_gui(&app).await;
    let ipc_handle = ipc::serve(app.clone());
    let mut supervisor = task_supervisor::TaskSupervisor::new(Arc::clone(&app));

    // Reclaim sinks leaked by a previous daemon; safe once single-instance owns.
    infrastructure::audio::sink::cleanup_orphaned_sinks().await;

    log::info!("Discovering devices...");
    crate::domain::registry::initialize_app_state(app.clone()).await;
    log::info!("Device discovery complete");

    // Prime the requirement cache so the first UI poll doesn't trigger a probe
    // per plugin; refreshed thereafter only at reconcile (no continuous polling).
    crate::application::usecases::plugin::plugins::refresh_requirements(&app).await?;

    // Compute passive HID/USB/SMBus recommendations once at startup. The same
    // snapshot is refreshed when repository manifests change.
    crate::application::usecases::plugin::plugins::refresh_recommendations(&app).await;
    crate::application::usecases::registry::runtime::bootstrap(&app).await;

    // Started only once startup is done — the grace clock must not elapse
    // before a client has had any chance to connect.
    let idle_app = Arc::clone(&app);
    supervisor.register(
        "Idle watcher",
        "",
        move || {
            let app = Arc::clone(&idle_app);
            Box::pin(async move { TaskHandle(lifecycle::spawn_idle_watcher(app, headless)) })
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
                TaskHandle(tokio::spawn(application::observers::hid::hotplug_monitor(
                    app,
                )))
            })
        },
        || {},
    );

    // Liveness watcher for plugin devices whose primary transport is a plain USB
    // device. HID hotplug does not cover them, so a disconnect would otherwise
    // strand a stale handle forever.
    let usb_hotplug_app = Arc::clone(&app);
    supervisor.register(
        "USB hotplug monitor",
        "USB device hotplug monitoring stopped unexpectedly.",
        move || {
            let app = Arc::clone(&usb_hotplug_app);
            Box::pin(async move {
                TaskHandle(tokio::spawn(
                    crate::application::observers::usb_hotplug::run(app),
                ))
            })
        },
        || {},
    );

    let retry_app = Arc::clone(&app);
    supervisor.register(
        "Failed device retry",
        "Retrying devices that failed to open stopped unexpectedly.",
        move || {
            let app = Arc::clone(&retry_app);
            Box::pin(async move {
                TaskHandle(tokio::spawn(
                    plugin::observers::host_scan::retry_failed_opens(app),
                ))
            })
        },
        || {},
    );

    // Devices that stay enumerated across a host suspend are invisible to both
    // hotplug observers, but their firmware still forgets everything.
    let power_app = Arc::clone(&app);
    supervisor.register(
        "Resume watcher",
        "",
        move || {
            let app = Arc::clone(&power_app);
            Box::pin(async move {
                TaskHandle(tokio::spawn(crate::application::observers::power::run(app)))
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
                TaskHandle(tokio::spawn(
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
                TaskHandle(tokio::spawn(
                    crate::application::usecases::device::telemetry::run(app),
                ))
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
            Box::pin(
                async move { TaskHandle(tokio::spawn(plugin::observers::data_status::run(app))) },
            )
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
                TaskHandle(tokio::spawn(device::policies::low_battery::watcher(app)))
            })
        },
        || {},
    );

    let update_app = Arc::clone(&app);
    supervisor.register(
        "Update watcher",
        "",
        move || {
            let app = Arc::clone(&update_app);
            Box::pin(async move {
                TaskHandle(tokio::spawn(
                    application::usecases::registry::update::watcher(app),
                ))
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
            let cfg = EngineConfigReceiver::new(fan_bus.clone());
            Box::pin(async move { TaskHandle(engine.start(cfg)) })
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
            let cfg = EngineConfigReceiver::new(rgb_bus.clone());
            Box::pin(async move { TaskHandle(engine.start(cfg).await) })
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
            let cfg = EngineConfigReceiver::new(lcd_bus.clone());
            Box::pin(async move { TaskHandle(engine.start(cfg).await) })
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
                TaskHandle(engine.start(rx).await)
            })
        },
        || {},
    );

    let remap_app = Arc::clone(&app);
    let remap_shutdown = Arc::clone(&app);
    supervisor.register(
        "Key remap engine",
        "Key remapping stopped unexpectedly.",
        move || {
            let app = Arc::clone(&remap_app);
            Box::pin(async move { TaskHandle(input::engine::startup::start(app)) })
        },
        move || remap_shutdown.input.shutdown(),
    );

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
    fn server_arguments_parse_and_reject_unknown_flags() {
        assert_eq!(
            process_role(&argv(&[])).unwrap(),
            ProcessRole::Server {
                headless: false,
                dev_plugins: None
            }
        );
        assert_eq!(
            process_role(&argv(&["--headless"])).unwrap(),
            ProcessRole::Server {
                headless: true,
                dev_plugins: None
            }
        );
        for args in [
            vec!["--dev-plugins", "C:/plugins", "--headless"],
            vec!["--headless", "--dev-plugins", "C:/plugins"],
        ] {
            assert_eq!(
                process_role(&argv(&args)).unwrap(),
                ProcessRole::Server {
                    headless: true,
                    dev_plugins: Some("C:/plugins".into())
                },
                "{args:?}"
            );
        }
        for args in [
            vec!["--unknown"],
            vec!["--headless", "--headless"],
            vec!["--dev-plugin-repo", "C:/plugins"],
            vec!["--dev-plugins"],
            vec!["--dev-plugins", "a", "--dev-plugins", "b"],
        ] {
            assert!(process_role(&argv(&args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn udev_rules_optionally_takes_a_catalog() {
        assert_eq!(
            process_role(&argv(&["udev-rules"])).unwrap(),
            ProcessRole::UdevRules { catalog: None }
        );
        assert_eq!(
            process_role(&argv(&[
                "udev-rules",
                "--catalog",
                "https://x/catalog.yaml"
            ]))
            .unwrap(),
            ProcessRole::UdevRules {
                catalog: Some("https://x/catalog.yaml".into())
            }
        );
        for args in [vec!["udev-rules", "extra"], vec!["udev-rules", "--catalog"]] {
            assert!(process_role(&argv(&args)).is_err(), "{args:?}");
        }
    }
}

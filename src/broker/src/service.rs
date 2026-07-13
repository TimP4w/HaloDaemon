// SPDX-License-Identifier: GPL-3.0-or-later
//! The broker's Windows service: install/uninstall and the SCM dispatcher.
//!
//! This is a **demand-start** LocalSystem service whose only job is to run the
//! register-bus RPC server (`server::serve_forever`). It does not supervise or
//! launch anything — the unprivileged worker is started by the GUI, and the
//! worker starts *this* service (via a granted `SERVICE_START` right) the first
//! time it needs a register bus. The service reports Running, serves, and stops
//! either on an SCM STOP or once it has been idle (no live client) for a grace
//! period — so the elevated helper never lingers after its worker is gone.

use std::ffi::OsString;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::server;

/// SCM key name, shared with the daemon (which starts the service) via
/// `halod-hwaccess`.
const SERVICE_NAME: &str = halod_hwaccess::proto::BROKER_SERVICE_NAME;
const SERVICE_DISPLAY_NAME: &str = "HaloDaemon Broker";
const SERVICE_DESCRIPTION: &str =
    "Elevated register-bus broker for HaloDaemon: serves SMBus/PawnIO to the unprivileged daemon.";

// Win32 SCM error codes treated as non-fatal during install / uninstall.
const ERROR_SERVICE_EXISTS: i32 = 1073;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

/// Stop serving once idle (no live client) continuously for this long.
const IDLE_GRACE: Duration = Duration::from_secs(30);

/// Register (or reconfigure) `HalodBroker` as a **demand-start** LocalSystem
/// service running `halod-broker.exe --service`. Not started here: the worker
/// starts it on first register-bus access.
pub fn install() -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("opening the service control manager")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().context("locating halod-broker.exe")?,
        // The SCM appends this; `--service` selects the dispatcher role in `main`.
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG) {
        Ok(service) => {
            if let Err(e) = service.set_description(SERVICE_DESCRIPTION) {
                println!("warning: could not set service description: {e}");
            }
            println!("installed service '{SERVICE_NAME}'");
        }
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_EXISTS) =>
        {
            // Reconfigure in place (e.g. an upgrade moved the exe path, or an
            // older install pointed the service at halod.exe --service).
            let service = manager
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .context("opening existing service to reconfigure")?;
            service
                .change_config(&info)
                .context("reconfiguring existing service")?;
            if let Err(e) = service.set_description(SERVICE_DESCRIPTION) {
                println!("warning: could not set service description: {e}");
            }
            println!("service '{SERVICE_NAME}' already installed; config refreshed");
        }
        Err(e) => return Err(anyhow!("creating service '{SERVICE_NAME}': {e}")),
    }

    // Let the non-elevated worker start/stop the service without a UAC prompt.
    grant_user_service_control();
    Ok(())
}

/// Allow interactive (non-elevated) users to start the broker service on demand
/// and query its state. Best-effort.
///
/// The grant is deliberately **start + query only** (no SERVICE_STOP): the
/// broker self-stops once idle, so no unprivileged caller needs to stop it, and
/// withholding STOP keeps one interactive user from stopping another user's
/// in-use broker (a cross-user denial of service). Which user is served is
/// enforced at the pipe, not here: even a user who starts the service cannot
/// connect unless they are the bound coordinator (see `clientauth`).
fn grant_user_service_control() {
    // Interactive Users: SERVICE_START (RP) + query (LC). No SERVICE_STOP (WP).
    const ACE: &str = "(A;;RPLC;;;IU)";

    let shown = Command::new("sc.exe")
        .args(["sdshow", SERVICE_NAME])
        .output();
    let current = match shown {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            println!("could not read the service security descriptor; skipping user grant");
            return;
        }
    };
    let current: String = current.split_whitespace().collect();

    let Some(updated) = insert_dacl_ace(&current, ACE) else {
        return;
    };
    match Command::new("sc.exe")
        .args(["sdset", SERVICE_NAME, &updated])
        .status()
    {
        Ok(s) if s.success() => {
            println!("granted interactive users start/stop control of the broker service")
        }
        _ => println!("could not update the service security descriptor"),
    }
}

/// Insert `ace` at the head of the DACL of a service SDDL string. Returns
/// `None` when the descriptor has no `D:` DACL or already contains `ace`.
fn insert_dacl_ace(sddl: &str, ace: &str) -> Option<String> {
    let sddl = sddl.trim();
    let d_pos = sddl.find("D:")?;
    if sddl.contains(ace) {
        return None;
    }
    Some(format!("D:{ace}{}", &sddl[d_pos + 2..]))
}

/// Stop and remove the `HalodBroker` service. Idempotent.
pub fn uninstall() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service control manager")?;

    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            println!("service '{SERVICE_NAME}' not installed; nothing to do");
            return Ok(());
        }
        Err(e) => return Err(anyhow!("opening service '{SERVICE_NAME}': {e}")),
    };

    if !matches!(
        service.query_status().map(|s| s.current_state),
        Ok(ServiceState::Stopped)
    ) {
        let _ = service.stop();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if matches!(
                service.query_status().map(|s| s.current_state),
                Ok(ServiceState::Stopped)
            ) {
                break;
            }
        }
    }

    service.delete().context("deleting service")?;
    println!("removed service '{SERVICE_NAME}'");
    Ok(())
}

/// Hand control to the SCM dispatcher (`--service`). Blocks until the service
/// stops.
pub fn run() -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting service dispatcher")?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        log::error!("[broker] service fatal: {e:#}");
    }
}

fn run_service() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    // STOP arrives on an SCM thread; forward it (and the idle signal) to the
    // main service thread over one channel.
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handler_tx = stop_tx.clone();
    let event_handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = handler_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("registering service control handler")?;

    let set_state = |state: ServiceState| -> Result<()> {
        status_handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .context("reporting service status")?;
        Ok(())
    };

    set_state(ServiceState::Running)?;
    log::info!("[broker] service running");

    // The RPC accept loop runs on its own thread forever; a fatal pipe error is
    // unrecoverable, so exit the process (SCM restarts on next demand-start).
    std::thread::spawn(|| {
        if let Err(e) = server::serve_forever() {
            log::error!("[broker] serve_forever failed: {e:#}");
            std::process::exit(1);
        }
    });

    // Self-stop once idle so the elevated service doesn't linger after the
    // worker exits (all connections drop).
    let idle_tx = stop_tx;
    std::thread::spawn(move || {
        server::wait_until_idle(IDLE_GRACE);
        log::info!("[broker] idle; requesting service stop");
        let _ = idle_tx.send(());
    });

    // Block until SCM STOP or the idle watcher fires.
    let _ = stop_rx.recv();
    set_state(ServiceState::Stopped)?;
    log::info!("[broker] service stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::insert_dacl_ace;

    #[test]
    fn insert_dacl_ace_prepends_into_the_dacl() {
        let sddl = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCLCSWLOCRRC;;;IU)S:(AU;FA;CCDCLC;;;WD)";
        let out = insert_dacl_ace(sddl, "(A;;RPWPLC;;;IU)").expect("ace inserted");
        assert_eq!(
            out,
            "D:(A;;RPWPLC;;;IU)(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCLCSWLOCRRC;;;IU)S:(AU;FA;CCDCLC;;;WD)"
        );
    }

    #[test]
    fn insert_dacl_ace_is_idempotent() {
        let sddl = "D:(A;;RPWPLC;;;IU)(A;;CCLC;;;SY)";
        assert!(insert_dacl_ace(sddl, "(A;;RPWPLC;;;IU)").is_none());
    }

    #[test]
    fn insert_dacl_ace_rejects_a_descriptor_without_a_dacl() {
        assert!(insert_dacl_ace("S:(AU;FA;CCDCLC;;;WD)", "(A;;RPWPLC;;;IU)").is_none());
    }

    #[test]
    fn insert_dacl_ace_tolerates_surrounding_whitespace() {
        let out = insert_dacl_ace("  D:(A;;CC;;;SY)\n", "(A;;RPWPLC;;;IU)").expect("trimmed");
        assert_eq!(out, "D:(A;;RPWPLC;;;IU)(A;;CC;;;SY)");
    }
}

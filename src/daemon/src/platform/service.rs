// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows service supervisor with a user-session bridge.
//!
//! `halod` has two orthogonal Windows requirements:
//!   * SMBus / PawnIO (DRAM & GPU RGB) needs an **elevated** token, and
//!   * DXGI desktop-duplication screen capture needs the process to run in the
//!     **interactive desktop session** — session 0 has no desktop to duplicate.
//!
//! A plain service runs in session 0 as `LocalSystem`: elevated, but with no
//! desktop. So the installed service (`HalodDaemon`) is only a *supervisor*.
//! It runs as `LocalSystem` in session 0 and relaunches the real daemon
//! (`halod.exe --worker`) into the active console session, using the
//! **elevated linked token** of the logged-on user. The worker then has both a
//! desktop (capture works) and Administrator rights (SMBus works).
//!
//! The supervisor keeps the worker alive with a 2 s poll loop: if the worker
//! exits, or the active session changes (logoff / fast-user-switch / a fresh
//! login after a boot-time start), the worker is (re)launched into whatever
//! session is now at the console. Polling — rather than handling
//! `SERVICE_CONTROL_SESSIONCHANGE` — is deliberate: it is simpler and also
//! self-heals a worker that crashed for any other reason.

use std::ffi::{c_void, OsString};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Name the service is registered under in the SCM. Shared with the GUI so its
/// "Start daemon" button targets the same service.
pub const SERVICE_NAME: &str = halod_shared::app::WINDOWS_SERVICE_NAME;
const SERVICE_DISPLAY_NAME: &str = "HaloDaemon";
const SERVICE_DESCRIPTION: &str =
    "Supervises the HaloDaemon device daemon and keeps it running in the active user session.";

// Win32 SCM error codes we treat as non-fatal during install / uninstall.
const ERROR_SERVICE_EXISTS: i32 = 1073;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

/// Absolute path to `halod.exe`, resolved once.
fn daemon_exe() -> Result<PathBuf> {
    std::env::current_exe().context("locating halod.exe")
}

/// Best-effort diagnostic log next to the executable. The supervisor runs in
/// session 0 with nowhere to print, so a file is the only practical way to see
/// why it failed to start.
fn diag(msg: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(crate::constants::SERVICE_LOG_FILENAME))
    {
        use std::io::Write;
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

// Install / uninstall — invoked by the installer via `--install-service` /
// `--uninstall-service`.

/// Register `HalodDaemon` as a demand-start `LocalSystem` service and start it.
/// Demand-start, not auto-start: the tray brings it up (via the ACL below) and
/// the idle-shutdown watcher ([`crate::lifecycle`]) stops it again once no
/// frontend is connected. Idempotent: re-running over an existing service
/// reconfigures it rather than skipping.
pub fn install() -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
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
        executable_path: daemon_exe()?,
        // The SCM appends these to the ImagePath; `--service` selects the
        // supervisor role in `main`.
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
            // Reconfigure rather than skip: an upgrade may have moved the exe
            // path, or an older install may still be AutoStart from before
            // idle-shutdown existed.
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

    // Let non-elevated interactive users start/stop the service, so the tray
    // can bring it back up without a UAC prompt.
    grant_user_service_control();

    // Start it now so a fresh install is immediately usable.
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .context("opening service to start it")?;
    let state = service.query_status().map(|s| s.current_state);
    if !matches!(
        state,
        Ok(ServiceState::Running) | Ok(ServiceState::StartPending)
    ) {
        service
            .start(&[] as &[&std::ffi::OsStr])
            .context("starting service")?;
        println!("started service '{SERVICE_NAME}'");
    }
    Ok(())
}

/// Allow interactive (non-elevated) users to start and stop the service, so the
/// tray can bring it back up without a UAC prompt.
///
/// Best-effort: it reads the service's security descriptor, splices in an
/// allow-ACE for the Interactive Users group, and writes it back via `sc.exe`.
/// A failure here only means the user would have to start the service elevated.
fn grant_user_service_control() {
    // Interactive Users: SERVICE_START (RP) + SERVICE_STOP (WP) + query (LC).
    const ACE: &str = "(A;;RPWPLC;;;IU)";

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
    // `sc sdshow` prints the SDDL padded with blank lines; the descriptor
    // itself has no internal whitespace, so collapsing it yields the bare string.
    let current: String = current.split_whitespace().collect();

    let Some(updated) = insert_dacl_ace(&current, ACE) else {
        // No DACL to extend, or the ACE is already present — nothing to do.
        return;
    };
    match Command::new("sc.exe")
        .args(["sdset", SERVICE_NAME, &updated])
        .status()
    {
        Ok(s) if s.success() => {
            println!("granted interactive users start/stop control of the service")
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

/// Stop and remove the `HalodDaemon` service. Idempotent: a missing service
/// is treated as success.
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

    // Stop it and wait (up to ~5 s) for the SCM to report it stopped, so the
    // installer can delete the files afterwards without sharing violations.
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

/// Ask the SCM to stop the whole `HalodDaemon` service.
///
/// Called by the **worker** when it receives an IPC `shutdown` command (the
/// tray's "Quit"). The worker carries the elevated token, so it has the rights
/// to control the service; the supervisor's stop handler then terminates the
/// worker. Stopping the service — rather than letting the worker exit — is
/// required, otherwise the supervisor would simply relaunch it.
pub fn request_stop() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service control manager")?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP)
        .context("opening service to stop it")?;
    service.stop().context("sending stop to service")?;
    Ok(())
}

// Supervisor — runs under the SCM (`--service`).

/// Hand control to the SCM dispatcher. Blocks until the service stops.
pub fn run_supervisor() -> Result<()> {
    diag("supervisor: starting SCM dispatcher");
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting service dispatcher")?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        diag(&format!("supervisor: fatal: {e:#}"));
    }
}

fn run_service() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    // The control handler runs on an SCM thread; it just forwards STOP to the
    // poll loop below.
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let event_handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
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
    diag("supervisor: running");

    let mut worker = Worker::default();
    let mut current_session: Option<u32> = None;

    loop {
        // Wake every 2 s, or immediately when a STOP arrives.
        if stop_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
            break;
        }
        match active_console_session() {
            Some(sid) => {
                if current_session != Some(sid) || !worker.is_alive() {
                    worker.kill();
                    match spawn_worker(sid) {
                        Ok(w) => {
                            worker = w;
                            current_session = Some(sid);
                            diag(&format!("supervisor: worker launched in session {sid}"));
                        }
                        Err(e) => {
                            // No user logged on yet, or a transient failure;
                            // retry on the next tick.
                            diag(&format!("supervisor: spawn failed: {e:#}"));
                            current_session = None;
                        }
                    }
                }
            }
            None => {
                // No interactive session (logged off, or locked at boot).
                if worker.is_alive() {
                    worker.kill();
                    diag("supervisor: no console session; worker stopped");
                }
                current_session = None;
            }
        }
    }

    worker.kill();
    diag("supervisor: stopping");
    set_state(ServiceState::Stopped)?;
    Ok(())
}

/// Session id currently attached to the physical console, or `None` when no
/// session is attached.
fn active_console_session() -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == u32::MAX {
        None
    } else {
        Some(id)
    }
}

/// A launched worker process. Not `Send` — it lives entirely on the supervisor
/// thread.
#[derive(Default)]
struct Worker {
    process: Option<windows::Win32::Foundation::HANDLE>,
}

impl Worker {
    fn is_alive(&self) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        match self.process {
            None => false,
            // A signalled process handle means the process has exited.
            Some(h) => unsafe { WaitForSingleObject(h, 0) != WAIT_OBJECT_0 },
        }
    }

    fn kill(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::TerminateProcess;
        if let Some(h) = self.process.take() {
            unsafe {
                let _ = TerminateProcess(h, 0);
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Launch `halod.exe --worker` into `session_id`, carrying the
/// logged-on user's elevated linked token.
fn spawn_worker(session_id: u32) -> Result<Worker> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    let exe = daemon_exe()?;
    let install_dir = exe
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // SAFETY: a standard CreateProcessAsUserW sequence. Every handle obtained
    // here is closed before returning; the worker's process handle is moved
    // into the returned `Worker`.
    unsafe {
        // Primary token of the user currently at the console.
        let mut user_token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut user_token)
            .context("WTSQueryUserToken (no user logged on?)")?;

        // Prefer the elevated *linked* token so the worker can reach SMBus;
        // fall back to the plain user token (capture still works, SMBus does
        // not) for accounts without an admin split token.
        let exec_token = match linked_primary_token(user_token) {
            Some(t) => {
                let _ = CloseHandle(user_token);
                t
            }
            None => user_token,
        };

        // Environment block for the target user (PATH, APPDATA, TEMP, …).
        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, exec_token, BOOL(0)).is_ok();

        let mut cmdline: Vec<u16> = OsString::from(format!("\"{}\" --worker", exe.display()))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut desktop: Vec<u16> = OsString::from("winsta0\\default")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dir_wide: Vec<u16> = install_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        let result = CreateProcessAsUserW(
            exec_token,
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            None,
            None,
            BOOL(0),
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            if have_env { Some(env) } else { None },
            PCWSTR(dir_wide.as_ptr()),
            &si,
            &mut pi,
        );

        if have_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(exec_token);

        result.context("CreateProcessAsUserW for --worker")?;

        let _ = CloseHandle(pi.hThread);
        Ok(Worker {
            process: Some(pi.hProcess),
        })
    }
}

/// The elevated linked token of `token`, duplicated into a *primary* token
/// (`CreateProcessAsUserW` rejects impersonation tokens). Returns `None` when
/// the account has no admin split token.
///
/// # Safety
/// `token` must be a valid, open token handle.
unsafe fn linked_primary_token(
    token: windows::Win32::Foundation::HANDLE,
) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TokenLinkedToken,
        TokenPrimary, TOKEN_ALL_ACCESS, TOKEN_LINKED_TOKEN,
    };

    let mut linked = TOKEN_LINKED_TOKEN::default();
    let mut ret_len = 0u32;
    GetTokenInformation(
        token,
        TokenLinkedToken,
        Some(&mut linked as *mut _ as *mut c_void),
        std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
        &mut ret_len,
    )
    .ok()?;
    if linked.LinkedToken.is_invalid() {
        return None;
    }

    let mut primary = HANDLE::default();
    let dup = DuplicateTokenEx(
        linked.LinkedToken,
        TOKEN_ALL_ACCESS,
        None,
        SecurityImpersonation,
        TokenPrimary,
        &mut primary,
    );
    let _ = CloseHandle(linked.LinkedToken);
    dup.ok()?;
    Some(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Re-running the grant must not stack duplicate ACEs.
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

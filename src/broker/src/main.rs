// SPDX-License-Identifier: GPL-3.0-or-later
//! `halod-broker` — the elevated register-bus broker.
//!
//! The only HaloDaemon process that runs elevated (LocalSystem). It does exactly
//! one thing: serve the register-bus primitives from `halod-hwaccess` (SMBus +
//! PawnIO) over a named pipe locked to the coordinator SIDs. It never launches
//! or supervises anything, and cannot run Lua — it links `halod-hwaccess` +
//! `windows` only.
//!
//! Roles (Windows):
//!   * `--install-service` / `--uninstall-service` — register/remove the
//!     demand-start `HalodBroker` LocalSystem service.
//!   * `--service` — run under the SCM (the installed path). Serves until an SCM
//!     STOP or until idle.
//!   * `--coordinator-sid=<SID>` — run the RPC server directly (the dev/UAC
//!     path: the worker `ShellExecuteExW("runas")`-launches this when no
//!     service is installed).
//!
//! The broker is *not* a security boundary against the daemon or any other
//! code running as the same user (they're indistinguishable at the RPC layer).
//! It is a smaller, auditable elevated surface — see the privilege-separation
//! doc.

#[cfg(target_os = "windows")]
mod clientauth;
#[cfg(target_os = "windows")]
mod hardening;
#[cfg(target_os = "windows")]
mod log_file;
#[cfg(target_os = "windows")]
mod pipe;
#[cfg(target_os = "windows")]
mod server;
#[cfg(target_os = "windows")]
mod service;

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--service") {
        log_file::init(true);
        hardening::apply_process_mitigations()?;
        let removed = hardening::drop_privileges()?;
        log::info!("starting under SCM; removed privileges {removed:?}");
        return service::run();
    }
    log_file::init(false);
    if has("--install-service") {
        return service::install();
    }
    if has("--uninstall-service") {
        return service::uninstall();
    }

    // Dev / UAC path: run the RPC server directly, exiting once idle so a
    // dev-run broker goes away when its daemon does.
    server::configure(server::foreground_config_from_args(&args)?)?;
    log::info!(
        "starting (foreground); serving {}",
        halod_hwaccess::proto::PIPE_NAME
    );
    std::thread::spawn(|| {
        if let Err(e) = server::serve_forever() {
            log::error!("serve_forever failed: {e:#}");
            std::process::exit(1);
        }
    });
    server::wait_until_idle(std::time::Duration::from_millis(
        halod_hwaccess::proto::BROKER_IDLE_GRACE_MS,
    ));
    log::info!("idle; exiting");
    Ok(())
}

// The broker is a Windows-only component (PawnIO/SMBus register access). On
// other platforms the daemon reaches `halod-hwaccess` in-process, so the broker
// binary is inert — it exists only so `cargo build --workspace` succeeds.
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("halod-broker is a Windows-only component; nothing to do on this platform");
}

// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;

use halod_plugin_signing::release;
use halod_shared::app::{
    release_asset_url, INSTALLER_ASSET, OFFICIAL_PUBLIC_KEYS, SUMS_ASSET, SUMS_SIG_ASSET,
};
use halod_shared::lifecycle::UPDATE_FAILED_ARG;
use halod_shared::update::{update_dir, UpdateError, UpdaterEvent};
use halod_shared::win32::{is_declined, run_elevated};
use semver::Version;

use crate::args::Args;
use crate::download;
use crate::verify::hash_file;

const MAX_SUMS_BYTES: u64 = 64 * 1024;
const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;
const PROGRESS_STEP: u64 = 256 * 1024;
const INSTALLER_PARAMETERS: &str = "/SILENT /SUPPRESSMSGBOXES /NORESTART";

pub fn run(args: &Args, emit: &mut impl FnMut(UpdaterEvent)) -> Result<(), UpdateError> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver");
    if args.version <= current {
        return Err(UpdateError::Stale);
    }
    let version = args.version.to_string();
    let agent = download::agent();
    let expected = signed_installer_hash(&agent, &version)?;

    let dir = update_dir().ok_or(UpdateError::Io)?;
    std::fs::create_dir_all(&dir).map_err(io_error)?;
    let installer = dir.join(INSTALLER_ASSET);
    download_installer(&agent, &version, &installer, emit)?;

    emit(UpdaterEvent::Verifying);
    let held = hold_verified(&installer, &expected)?;

    emit(UpdaterEvent::Consent);
    let process = run_elevated(&installer, INSTALLER_PARAMETERS, &dir).map_err(|e| {
        log::warn!("launching the installer: {e}");
        if is_declined(&e) {
            UpdateError::Declined
        } else {
            UpdateError::Installer
        }
    })?;

    emit(UpdaterEvent::Installing);
    let exit_code = wait(&process);
    log::info!("installer exited with {exit_code:?}");
    drop(held);
    let _ = std::fs::remove_file(&installer);
    relaunch(&args.relaunch, exit_code != Some(0));
    Ok(())
}

fn signed_installer_hash(agent: &ureq::Agent, version: &str) -> Result<String, UpdateError> {
    let sums = download::fetch(
        agent,
        &release_asset_url(version, SUMS_ASSET),
        MAX_SUMS_BYTES,
    )?;
    let signature = download::fetch(
        agent,
        &release_asset_url(version, SUMS_SIG_ASSET),
        MAX_SUMS_BYTES,
    )?;
    let signature = String::from_utf8(signature).map_err(|_| UpdateError::Signature)?;
    release::verify(version, &sums, &signature, OFFICIAL_PUBLIC_KEYS).map_err(|e| {
        log::warn!("release signature: {e:#}");
        UpdateError::Signature
    })?;
    let sums = std::str::from_utf8(&sums).map_err(|_| UpdateError::Signature)?;
    release::expected_hash(sums, INSTALLER_ASSET)
        .map(str::to_owned)
        .ok_or(UpdateError::Signature)
}

fn download_installer(
    agent: &ureq::Agent,
    version: &str,
    path: &Path,
    emit: &mut impl FnMut(UpdaterEvent),
) -> Result<(), UpdateError> {
    let download = download::open(agent, &release_asset_url(version, INSTALLER_ASSET))?;
    let total = download.total;
    let mut file = BufWriter::new(File::create(path).map_err(io_error)?);
    let mut reported = 0;
    emit(UpdaterEvent::Progress { done: 0, total });
    download::copy_capped(download.reader, &mut file, MAX_INSTALLER_BYTES, |done| {
        if done - reported >= PROGRESS_STEP || Some(done) == total {
            reported = done;
            emit(UpdaterEvent::Progress { done, total });
        }
    })?;
    file.flush().map_err(io_error)?;
    Ok(())
}

/// Re-hashes the installer through a handle that denies writers, and keeps it
/// open so the file cannot change before the elevated run.
fn hold_verified(path: &Path, expected: &str) -> Result<File, UpdateError> {
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .map_err(io_error)?;
    let actual = hash_file(&file).map_err(|e| {
        log::warn!("hashing the installer: {e:#}");
        UpdateError::Io
    })?;
    if actual != expected {
        log::warn!("installer hash {actual} does not match {expected}");
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(UpdateError::Hash);
    }
    Ok(file)
}

fn wait(process: &OwnedHandle) -> Option<u32> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    let handle = HANDLE(process.as_raw_handle());
    let mut code = 0;
    // SAFETY: `handle` is a live process handle owned by `process`.
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        GetExitCodeProcess(handle, &mut code).ok()?;
    }
    Some(code)
}

fn relaunch(gui: &Path, failed: bool) {
    let mut command = std::process::Command::new(gui);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if failed {
        command.arg(UPDATE_FAILED_ARG);
    }
    if let Err(e) = command.spawn() {
        log::warn!("relaunching {}: {e}", gui.display());
    }
}

fn io_error(e: std::io::Error) -> UpdateError {
    log::warn!("installer file: {e}");
    UpdateError::Io
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared PawnIO kernel-driver bridge.
//!
//! PawnIO (<https://pawnio.eu/>) is loaded once per process; each consumer
//! (the daemon's `lpcio` for SuperIO fan control, the `smbus::windows::chipset`
//! backend for chipset SMBus, `amd_smn` for AMD SMN) opens its own
//! [`PawnioModule`] — a fresh `pawnio_open` handle with one PawnIO `.bin` module
//! loaded into it. This module is used only inside the elevated broker; the
//! daemon sees typed SMBus, AMD SMN, and LPC operations instead.
//!
//! The `PawnIOLib.dll` handle and the on-disk module blobs are both cached
//! process-wide so opening N modules does not re-load the DLL N times or
//! re-read the same `.bin` from disk N times.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Mutex, OnceLock};

// ── FFI ──────────────────────────────────────────────────────────────────────

type PawnioOpenFn = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type PawnioLoadFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type PawnioExecuteFn = unsafe extern "C" fn(
    *mut c_void, // handle
    *const u8,   // null-terminated function name
    *const u64,  // input array
    usize,       // input length
    *mut u64,    // output array
    usize,       // output length
    *mut usize,  // returned element count
) -> i32;
type PawnioCloseFn = unsafe extern "C" fn(*mut c_void) -> i32;

struct PawnioApi {
    // Kept alive so the resolved function pointers stay valid. Never touched
    // after construction.
    _lib: libloading::Library,
    open: PawnioOpenFn,
    load: PawnioLoadFn,
    execute: PawnioExecuteFn,
    close: PawnioCloseFn,
}

// SAFETY: function pointers and the library handle are immutable after init;
// concurrent calls into PawnIO use distinct module handles, which the caller
// is expected to serialise per-handle.
unsafe impl Send for PawnioApi {}
unsafe impl Sync for PawnioApi {}

fn api() -> Result<&'static PawnioApi> {
    static API: OnceLock<Result<PawnioApi, String>> = OnceLock::new();
    API.get_or_init(|| init_api().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

fn init_api() -> Result<PawnioApi> {
    // Only load from trusted, non-user-writable install locations. A bare
    // "PawnIOLib.dll" would trigger the Windows DLL search order (CWD, %PATH%),
    // letting an attacker drop a malicious DLL that this elevated process would
    // load — so it is deliberately excluded.
    let candidates = library_candidates();
    let lib = candidates
        .iter()
        .find_map(|p| unsafe { libloading::Library::new(p).ok() })
        .ok_or_else(|| anyhow!("PawnIO not found — install from https://pawnio.eu/"))?;

    let open: PawnioOpenFn = unsafe {
        *lib.get(b"pawnio_open\0")
            .map_err(|e| anyhow!("pawnio_open: {e}"))?
    };
    let load: PawnioLoadFn = unsafe {
        *lib.get(b"pawnio_load\0")
            .map_err(|e| anyhow!("pawnio_load: {e}"))?
    };
    let execute: PawnioExecuteFn = unsafe {
        *lib.get(b"pawnio_execute\0")
            .map_err(|e| anyhow!("pawnio_execute: {e}"))?
    };
    let close: PawnioCloseFn = unsafe {
        *lib.get(b"pawnio_close\0")
            .map_err(|e| anyhow!("pawnio_close: {e}"))?
    };

    Ok(PawnioApi {
        _lib: lib,
        open,
        load,
        execute,
        close,
    })
}

fn library_candidates() -> Vec<String> {
    let mut candidates = vec![r"C:\Program Files\PawnIO\PawnIOLib.dll".to_string()];
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(format!(r"{pf}\PawnIO\PawnIOLib.dll"));
    }
    if let Ok(pf) = std::env::var("ProgramW6432") {
        candidates.push(format!(r"{pf}\PawnIO\PawnIOLib.dll"));
    }
    candidates
}

/// Whether PawnIO is installed in a trusted system location. This only checks
/// for the DLL and never loads code into the probing process.
pub fn installation_present() -> bool {
    library_candidates()
        .iter()
        .any(|path| std::path::Path::new(path).is_file())
}

// ── Blob loading ─────────────────────────────────────────────────────────────

/// Return the bytes of a PawnIO module blob from disk, cached for the rest of
/// the process so repeated opens (e.g. one LpcIO module per SuperIO slot) hit
/// the disk only on the first call.
fn load_blob(name: &str) -> Option<&'static [u8]> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static [u8]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let g = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = g.get(name) {
            return Some(*b);
        }
    }
    // Read from disk without holding the lock (slow path).
    let data = read_blob_from_disk(name)?;
    // Re-acquire and re-check: a concurrent caller may have raced and already
    // inserted the same blob. Return the winner's slice rather than leaking twice.
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(b) = g.get(name) {
        return Some(*b);
    }
    let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
    g.insert(name.to_string(), leaked);
    Some(leaked)
}

fn read_blob_from_disk(name: &str) -> Option<Vec<u8>> {
    // These blobs are executed by the PawnIO kernel driver, so they must come
    // only from the install directory (next to the elevated executable), never
    // from the current working directory or any user-writable location — that
    // would be a privileged search-path hijack straight into a kernel driver.
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            dirs.push(dir.join("pwnio"));
        }
    }
    for dir in dirs {
        let path = dir.join(name);
        if let Ok(data) = std::fs::read(&path) {
            log::debug!("[PawnIO] using blob {}", path.display());
            return Some(data);
        }
    }
    None
}

// ── Module handle ────────────────────────────────────────────────────────────

/// A PawnIO `.bin` module loaded into its own driver handle. Internal state
/// kept by the module (`select_slot`/BAR registrations, port selection, …)
/// is per-instance, so callers that need isolation should open separate
/// `PawnioModule`s.
pub struct PawnioModule {
    api: &'static PawnioApi,
    /// Internal mutex serialises access to the raw PawnIO handle so the
    /// struct is `Send` + `Sync` without requiring external synchronisation
    /// from every caller.
    handle: Mutex<*mut c_void>,
    blob_name: String,
}

// SAFETY: `handle` is protected by an internal `Mutex`, so all access is
// single-threaded even across thread boundaries.
unsafe impl Send for PawnioModule {}
unsafe impl Sync for PawnioModule {}

impl PawnioModule {
    /// Open a fresh PawnIO handle and load the first available blob from
    /// `blob_names` into it. Each candidate name is tried in order; the first
    /// one that loads cleanly wins, the rest are skipped. The chosen name is
    /// available via [`PawnioModule::blob_name`].
    pub fn open(blob_names: &[&str]) -> Result<Self> {
        let api = api()?;
        let mut handle: *mut c_void = ptr::null_mut();
        let hr = unsafe { (api.open)(&mut handle) };
        validate_open_result(hr, handle)?;

        let mut last_err: Option<anyhow::Error> = None;
        for name in blob_names {
            let Some(blob) = load_blob(name) else {
                last_err = Some(anyhow!(
                    "PawnIO blob {name} not found — place it next to the \
                     executable or in pwnio/"
                ));
                continue;
            };
            let hr = unsafe { (api.load)(handle, blob.as_ptr(), blob.len()) };
            if hr == 0 {
                log::debug!("[PawnIO] loaded module {name}");
                return Ok(Self {
                    api,
                    handle: Mutex::new(handle),
                    blob_name: name.to_string(),
                });
            }
            log::debug!("[PawnIO] rejected {name}: HRESULT=0x{:08x}", hr as u32);
            last_err = Some(anyhow!("pawnio_load({name}): HRESULT=0x{:08x}", hr as u32));
        }

        unsafe { (api.close)(handle) };
        Err(last_err.unwrap_or_else(|| anyhow!("no PawnIO blob candidates supplied")))
    }

    /// Name of the loaded blob — useful for diagnostics.
    pub fn blob_name(&self) -> &str {
        &self.blob_name
    }

    /// Run a PawnIO ioctl. `output` is the caller-owned result buffer; the
    /// returned `usize` is the number of meaningful `u64` words PawnIO wrote
    /// (≤ `output.len()`). Pass an empty slice when the ioctl returns nothing.
    ///
    /// Not metered here: broker RPC capabilities provide request bounds, while
    /// daemon transports apply their hardware-specific write limits.
    pub fn exec(&self, func: &std::ffi::CStr, input: &[u64], output: &mut [u64]) -> Result<usize> {
        let handle = *self.handle.lock().unwrap_or_else(|e| e.into_inner());
        let mut ret = 0usize;
        let hr = unsafe {
            (self.api.execute)(
                handle,
                func.as_ptr() as *const u8,
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut ret,
            )
        };
        validate_execute_result(hr, ret, output.len(), func.to_str().unwrap_or("?"))
    }
}

fn validate_open_result(hr: i32, handle: *mut c_void) -> Result<()> {
    if hr != 0 {
        return Err(anyhow!("pawnio_open: HRESULT=0x{:08x}", hr as u32));
    }
    if handle.is_null() {
        return Err(anyhow!("pawnio_open succeeded but returned a null handle"));
    }
    Ok(())
}

fn validate_execute_result(
    hr: i32,
    returned: usize,
    capacity: usize,
    function: &str,
) -> Result<usize> {
    if hr != 0 {
        return Err(anyhow!(
            "pawnio_execute({function}): HRESULT=0x{:08x}",
            hr as u32
        ));
    }
    if returned > capacity {
        return Err(anyhow!(
            "pawnio_execute({function}) reported {returned} output words for a {capacity}-word buffer"
        ));
    }
    Ok(returned)
}

impl Drop for PawnioModule {
    fn drop(&mut self) {
        let handle = *self.handle.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            (self.api.close)(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_execute_result, validate_open_result};

    #[test]
    fn rejects_successful_open_with_null_handle() {
        assert!(validate_open_result(0, std::ptr::null_mut()).is_err());
    }

    #[test]
    fn accepts_non_null_open_handle() {
        assert!(validate_open_result(0, std::ptr::dangling_mut()).is_ok());
    }

    #[test]
    fn execute_rejects_driver_count_beyond_buffer() {
        assert!(validate_execute_result(0, 5, 4, "ioctl_test").is_err());
    }

    #[test]
    fn execute_propagates_hresult_before_using_count() {
        let error = validate_execute_result(-1, usize::MAX, 4, "ioctl_test").unwrap_err();
        assert!(error.to_string().contains("HRESULT"));
    }
}

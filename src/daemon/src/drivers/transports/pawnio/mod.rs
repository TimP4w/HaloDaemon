#![cfg(target_os = "windows")]

//! Shared PawnIO kernel-driver bridge.
//!
//! PawnIO (<https://pawnio.eu/>) is loaded once per process; each consumer
//! ([`crate::drivers::transports::lpcio`] for SuperIO fan control,
//! [`crate::drivers::transports::smbus::windows::chipset`] for chipset SMBus)
//! opens its own [`PawnioModule`] — a fresh `pawnio_open` handle with one
//! PawnIO `.bin` module loaded into it. Module-internal state (e.g. LpcIO's
//! `select_slot` + `find_bars` BAR registration) therefore stays isolated
//! between consumers and between distinct chips.
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
    let mut candidates = vec![
        "PawnIOLib.dll".to_string(),
        r"C:\Program Files\PawnIO\PawnIOLib.dll".to_string(),
    ];
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(format!(r"{pf}\PawnIO\PawnIOLib.dll"));
    }
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

// ── Blob loading ─────────────────────────────────────────────────────────────

/// Return the bytes of a PawnIO module blob from disk, cached for the rest of
/// the process so repeated opens (e.g. one LpcIO module per SuperIO slot) hit
/// the disk only on the first call.
fn load_blob(name: &str) -> Option<&'static [u8]> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static [u8]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let g = cache.lock().ok()?;
        if let Some(b) = g.get(name) {
            return Some(*b);
        }
    }
    let data = read_blob_from_disk(name)?;
    let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
    cache.lock().ok()?.insert(name.to_string(), leaked);
    Some(leaked)
}

fn read_blob_from_disk(name: &str) -> Option<Vec<u8>> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            dirs.push(dir.join("pwnio"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("pwnio"));
        dirs.push(cwd);
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
    handle: *mut c_void,
    blob_name: String,
}

// SAFETY: the raw PawnIO handle is only touched while the owner holds a
// `Mutex<PawnioModule>` (or guarantees external serialisation), so all access
// is single-threaded even though the pointer is not itself `Send`.
unsafe impl Send for PawnioModule {}

impl PawnioModule {
    /// Open a fresh PawnIO handle and load the first available blob from
    /// `blob_names` into it. Each candidate name is tried in order; the first
    /// one that loads cleanly wins, the rest are skipped. The chosen name is
    /// available via [`PawnioModule::blob_name`].
    pub fn open(blob_names: &[&str]) -> Result<Self> {
        let api = api()?;
        let mut handle: *mut c_void = ptr::null_mut();
        let hr = unsafe { (api.open)(&mut handle) };
        if hr != 0 {
            return Err(anyhow!("pawnio_open: HRESULT=0x{:08x}", hr as u32));
        }

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
                    handle,
                    blob_name: (*name).to_string(),
                });
            }
            log::debug!("[PawnIO] rejected {name}: HRESULT=0x{:08x}", hr as u32);
            last_err = Some(anyhow!(
                "pawnio_load({name}): HRESULT=0x{:08x}",
                hr as u32
            ));
        }

        unsafe { (api.close)(handle) };
        Err(last_err
            .unwrap_or_else(|| anyhow!("no PawnIO blob candidates supplied")))
    }

    /// Name of the loaded blob — useful for diagnostics.
    pub fn blob_name(&self) -> &str {
        &self.blob_name
    }

    /// Run a PawnIO ioctl. `output` is the caller-owned result buffer; the
    /// returned `usize` is the number of meaningful `u64` words PawnIO wrote
    /// (≤ `output.len()`). Pass an empty slice when the ioctl returns nothing.
    pub fn exec(&self, func: &[u8], input: &[u64], output: &mut [u64]) -> Result<usize> {
        let mut ret = 0usize;
        let hr = unsafe {
            (self.api.execute)(
                self.handle,
                func.as_ptr(),
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut ret,
            )
        };
        if hr != 0 {
            return Err(anyhow!(
                "pawnio_execute({}): HRESULT=0x{:08x}",
                std::str::from_utf8(func)
                    .unwrap_or("?")
                    .trim_end_matches('\0'),
                hr as u32
            ));
        }
        Ok(ret)
    }
}

impl Drop for PawnioModule {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn load_blob_returns_none_for_nonexistent() {
        assert!(super::load_blob("nonexistent_xyz.bin").is_none());
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! Win32 named mutexes shared with other hardware-monitoring software.

use anyhow::{anyhow, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, MUTEX_MODIFY_STATE,
    SYNCHRONIZATION_SYNCHRONIZE,
};

/// Held by LibreHardwareMonitor, HWiNFO and friends around SMBus transfers.
pub const SMBUS: &str = r"Global\Access_SMBUS.HTP.Method";
/// Held by the same tools around Super-I/O config and HWM index/data access.
pub const ISA_BUS: &str = r"Global\Access_ISABUS.HTP.Method";

pub struct NamedMutex {
    handle: HANDLE,
    name: &'static str,
}

// SAFETY: the handle refers to a kernel object whose ownership the kernel
// tracks per thread; it is only closed on drop.
unsafe impl Send for NamedMutex {}
unsafe impl Sync for NamedMutex {}

impl NamedMutex {
    /// Create the mutex, or open it when an elevated process already owns the
    /// `Global\` name and denies creation to this one.
    pub fn open(name: &'static str) -> Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let lpname = PCWSTR(wide.as_ptr());
        let handle = unsafe { CreateMutexW(None, false, lpname) }
            .or_else(|error| {
                if error.code() != ERROR_ACCESS_DENIED.to_hresult() {
                    return Err(error);
                }
                unsafe {
                    OpenMutexW(
                        SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
                        false,
                        lpname,
                    )
                }
            })
            .map_err(|error| anyhow!("opening mutex {name}: {error}"))?;
        Ok(Self { handle, name })
    }

    pub fn lock(&self, timeout_ms: u32) -> Result<MutexGuard<'_>> {
        let wait = unsafe { WaitForSingleObject(self.handle, timeout_ms) };
        // WAIT_ABANDONED: the previous owner died holding it; the lock is ours.
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            return Err(anyhow!("{} acquire failed or timed out", self.name));
        }
        Ok(MutexGuard(self))
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Releases the mutex on drop, including unwinds.
pub struct MutexGuard<'a>(&'a NamedMutex);

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NamedMutex;

    const NAME: &str = r"Local\halod_named_mutex_test";

    fn try_lock_from_other_thread() -> bool {
        std::thread::spawn(|| NamedMutex::open(NAME).unwrap().lock(50).is_ok())
            .join()
            .unwrap()
    }

    #[test]
    fn lock_excludes_other_threads_until_released() {
        let mutex = NamedMutex::open(NAME).unwrap();
        let guard = mutex.lock(1000).unwrap();
        assert!(!try_lock_from_other_thread());
        drop(guard);
        assert!(try_lock_from_other_thread());
    }
}

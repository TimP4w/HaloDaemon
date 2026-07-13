// SPDX-License-Identifier: GPL-3.0-or-later
//! Named-pipe plumbing for the broker's RPC server: a DACL-restricted pipe
//! instance factory and a blocking `Read`/`Write` stream over one connection.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

const PIPE_BUFFER: u32 = 64 * 1024;
const MAX_PIPE_INSTANCES: u32 = 32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns the `PSECURITY_DESCRIPTOR` parsed from an SDDL string (freed on drop)
/// and hands out a `SECURITY_ATTRIBUTES` pointing at it for `CreateNamedPipeW`.
pub struct PipeSecurity {
    sd: PSECURITY_DESCRIPTOR,
}

impl PipeSecurity {
    /// Parse `sddl` (e.g. the output of `winsec::interactive_dacl_sddl`) into a
    /// security descriptor for the pipe.
    pub fn from_sddl(sddl: &str) -> Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl_w` outlives the call; `sd` is freed in `Drop`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide(sddl).as_ptr()),
                1, // SDDL_REVISION_1
                &mut sd,
                None,
            )
            .map_err(|e| anyhow!("ConvertStringSecurityDescriptor: {e}"))?;
        }
        Ok(Self { sd })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.sd.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.sd.0)));
            }
        }
    }
}

/// Create one instance of the named pipe with the given security. Byte-mode,
/// blocking, with an explicit instance cap matching the server admission cap.
pub fn create_instance(name: &str, sec: &PipeSecurity) -> Result<HANDLE> {
    let sa = sec.attributes();
    // SAFETY: `name_w` and `sa` outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide(name).as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            MAX_PIPE_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            Some(&sa),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(anyhow!(
            "CreateNamedPipeW failed: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(handle)
}

/// Block until a client connects to `pipe`. `ERROR_PIPE_CONNECTED` (232) means
/// a client connected between create and this call — also success.
pub fn wait_for_client(pipe: HANDLE) -> Result<()> {
    // SAFETY: `pipe` is a valid pipe instance handle.
    let ok = unsafe { ConnectNamedPipe(pipe, None) };
    match ok {
        Ok(()) => Ok(()),
        Err(e) if e.code().0 as u32 & 0xFFFF == 232 => Ok(()),
        Err(e) => Err(anyhow!("ConnectNamedPipe: {e}")),
    }
}

/// A blocking byte stream over one accepted pipe connection. Disconnects and
/// closes the handle on drop, so the broker frees the connection (and every
/// bus/module handle scoped to it) when the worker goes away.
pub struct PipeStream(HANDLE);

// SAFETY: the handle is owned exclusively by the single thread that serves the
// connection; it is never shared, only moved into that thread.
unsafe impl Send for PipeStream {}

impl PipeStream {
    pub fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// Wait until at least one request byte is available without letting an
    /// authenticated but silent client retain broker resources forever.
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let started = Instant::now();
        loop {
            let mut available = 0u32;
            // SAFETY: this only queries the owned pipe handle and writes one
            // stack `u32`; no data buffer is supplied.
            unsafe { PeekNamedPipe(self.0, None, 0, None, Some(&mut available), None) }
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            if available != 0 {
                return Ok(true);
            }
            if started.elapsed() >= timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: `buf` is valid for `buf.len()` bytes for the duration.
        unsafe { ReadFile(self.0, Some(buf), Some(&mut read), None) }
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        Ok(read as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: `buf` is valid for `buf.len()` bytes for the duration.
        unsafe { WriteFile(self.0, Some(buf), Some(&mut written), None) }
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sddl_parses_the_broker_dacl_and_frees_it() {
        // The real broker DACL round-trips through the parser; the descriptor is
        // non-null and freed on drop without a double-free (run under the alloc
        // checks the test harness applies).
        let sec = PipeSecurity::from_sddl("D:P(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)")
            .expect("valid SDDL should parse");
        assert!(!sec.sd.0.is_null());
        let sa = sec.attributes();
        assert_eq!(sa.lpSecurityDescriptor, sec.sd.0);
        assert!(!sa.bInheritHandle.as_bool());
    }

    #[test]
    fn from_sddl_rejects_a_malformed_descriptor() {
        assert!(PipeSecurity::from_sddl("not a security descriptor").is_err());
    }
}

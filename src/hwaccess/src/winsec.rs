// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows identity and security-descriptor helpers shared by the daemon and
//! elevated broker.

use anyhow::{anyhow, bail, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TokenSessionId, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// User SID and Windows session id of the current process token.
pub fn current_process_identity() -> Result<(String, u32)> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| anyhow!("OpenProcessToken: {e}"))?;
    let result = token_identity(token);
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn token_identity(token: HANDLE) -> Result<(String, u32)> {
    let mut len = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    if len == 0 {
        bail!("GetTokenInformation(TokenUser) size query returned 0");
    }
    let mut buf = vec![0u8; len as usize];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        )
    }
    .map_err(|e| anyhow!("GetTokenInformation(TokenUser): {e}"))?;

    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut sid_string = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut sid_string) }
        .map_err(|e| anyhow!("ConvertSidToStringSidW: {e}"))?;
    let sid = unsafe { sid_string.to_string() }.map_err(|e| anyhow!("SID to string: {e}"));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_string.0 as *mut _)));
    }

    let mut session = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            Some(&mut session as *mut u32 as *mut _),
            std::mem::size_of::<u32>() as u32,
            &mut len,
        )
    }
    .map_err(|e| anyhow!("GetTokenInformation(TokenSessionId): {e}"))?;
    Ok((sid?, session))
}

/// Reject strings that could inject extra SDDL clauses before interpolation.
pub fn validate_sid_string(sid: &str) -> Result<()> {
    if !sid.starts_with("S-1-")
        || sid.len() > 184
        || !sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
    {
        bail!("invalid SID string");
    }
    Ok(())
}

/// Protected broker-pipe DACL for one concrete coordinator SID plus SYSTEM.
pub fn coordinator_dacl_sddl(sid: &str) -> Result<String> {
    validate_sid_string(sid)?;
    Ok(format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dacl_is_protected_and_names_only_coordinator_and_system() {
        let sddl = coordinator_dacl_sddl("S-1-5-21-1-2-3-1001").unwrap();
        assert_eq!(sddl, "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)");
        assert!(!sddl.contains(";;;IU)"));
        assert!(!sddl.contains(";;;WD)"));
    }

    #[test]
    fn sid_validation_blocks_sddl_injection() {
        assert!(coordinator_dacl_sddl("S-1-5-21-1)(A;;GA;;;WD").is_err());
        assert!(coordinator_dacl_sddl("IU").is_err());
    }
}

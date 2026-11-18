// SPDX-License-Identifier: GPL-3.0-or-later
//! Service-mode self-hardening: process mitigations, a restricted DLL search
//! path, and a token stripped of every privilege the broker does not use.
//! PawnIO's device open is a DACL check, so no privilege is needed for it.

use anyhow::{anyhow, bail, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeNameW, TokenPrivileges,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_REMOVED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::SystemServices::{
    PROCESS_MITIGATION_DYNAMIC_CODE_POLICY, PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0,
    PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY,
    PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY_0, PROCESS_MITIGATION_IMAGE_LOAD_POLICY,
    PROCESS_MITIGATION_IMAGE_LOAD_POLICY_0,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, ProcessDynamicCodePolicy,
    ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, SetProcessMitigationPolicy,
    PROCESS_MITIGATION_POLICY,
};

const KEPT_PRIVILEGE: &str = "SeChangeNotifyPrivilege";

pub fn apply_process_mitigations() -> Result<()> {
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)
    }
    .map_err(|e| anyhow!("SetDefaultDllDirectories: {e}"))?;
    set_policy(
        ProcessExtensionPointDisablePolicy,
        &PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
            Anonymous: PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY_0 {
                Flags: 1, // DisableExtensionPoints
            },
        },
    )?;
    set_policy(
        ProcessDynamicCodePolicy,
        &PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
            Anonymous: PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0 {
                Flags: 1, // ProhibitDynamicCode
            },
        },
    )?;
    set_policy(
        ProcessImageLoadPolicy,
        &PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
            Anonymous: PROCESS_MITIGATION_IMAGE_LOAD_POLICY_0 {
                Flags: 0b101, // NoRemoteImages | PreferSystem32Images
            },
        },
    )
}

fn set_policy<T>(policy: PROCESS_MITIGATION_POLICY, value: &T) -> Result<()> {
    unsafe {
        SetProcessMitigationPolicy(
            policy,
            value as *const T as *const _,
            std::mem::size_of::<T>(),
        )
    }
    .map_err(|e| anyhow!("SetProcessMitigationPolicy({}): {e}", policy.0))
}

/// Which of the token's privileges to remove: all but [`KEPT_PRIVILEGE`].
fn privileges_to_remove<T: Clone>(privileges: &[(String, T)]) -> Vec<T> {
    privileges
        .iter()
        .filter(|(name, _)| name != KEPT_PRIVILEGE)
        .map(|(_, id)| id.clone())
        .collect()
}

pub fn drop_privileges() -> Result<Vec<String>> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
            &mut token,
        )
    }
    .map_err(|e| anyhow!("OpenProcessToken: {e}"))?;
    let result = strip_token(token);
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn strip_token(token: HANDLE) -> Result<Vec<String>> {
    let mut len = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenPrivileges, None, 0, &mut len) };
    if len == 0 {
        bail!("GetTokenInformation(TokenPrivileges) size query returned 0");
    }
    // u64 backing keeps TOKEN_PRIVILEGES suitably aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token,
            TokenPrivileges,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        )
    }
    .map_err(|e| anyhow!("GetTokenInformation(TokenPrivileges): {e}"))?;
    let header = buf.as_ptr() as *const TOKEN_PRIVILEGES;
    let entries = unsafe {
        std::slice::from_raw_parts(
            (*header).Privileges.as_ptr(),
            (*header).PrivilegeCount as usize,
        )
    };
    let named: Vec<(String, LUID)> = entries
        .iter()
        .map(|p| (privilege_name(p.Luid), p.Luid))
        .collect();
    let remove = privileges_to_remove(&named);
    for luid in &remove {
        let change = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: *luid,
                Attributes: SE_PRIVILEGE_REMOVED,
            }],
        };
        unsafe { AdjustTokenPrivileges(token, false, Some(&change), 0, None, None) }
            .map_err(|e| anyhow!("AdjustTokenPrivileges: {e}"))?;
        // Success with ERROR_NOT_ALL_ASSIGNED means nothing was removed.
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            bail!("AdjustTokenPrivileges did not remove LUID {luid:?}");
        }
    }
    Ok(named
        .into_iter()
        .filter(|(_, luid)| remove.contains(luid))
        .map(|(name, _)| name)
        .collect())
}

fn privilege_name(luid: LUID) -> String {
    let mut buf = [0u16; 128];
    let mut len = buf.len() as u32;
    match unsafe {
        LookupPrivilegeNameW(
            windows::core::PCWSTR::null(),
            &luid,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    } {
        Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
        Err(_) => format!("LUID {}:{}", luid.HighPart, luid.LowPart),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_change_notify_survives() {
        let privileges = [
            ("SeImpersonatePrivilege".to_owned(), 1),
            (KEPT_PRIVILEGE.to_owned(), 2),
            ("SeDebugPrivilege".to_owned(), 3),
            ("SeTcbPrivilege".to_owned(), 4),
        ];
        assert_eq!(privileges_to_remove(&privileges), [1, 3, 4]);
        assert!(privileges_to_remove::<u8>(&[]).is_empty());
    }
}

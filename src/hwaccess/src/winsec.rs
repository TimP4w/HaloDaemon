// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows identity and security-descriptor helpers shared by the daemon and
//! elevated broker.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, UNICODE_STRING};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{
    GetTokenInformation, TokenSessionId, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// User SID and Windows session id of the current process token.
pub fn current_process_identity() -> Result<(String, u32)> {
    process_token_identity(unsafe { GetCurrentProcess() })
}

/// User SID and Windows session id of another process's primary token.
pub fn process_identity(pid: u32) -> Result<(String, u32)> {
    let process = OwnedHandle::open_process(pid)?;
    process_token_identity(process.0)
}

/// Image path of a running process as an openable `\\?\GLOBALROOT\Device\...`
/// path. Uses `SystemProcessIdInformation`, which needs no process handle, so
/// a medium-integrity client can name a LocalSystem server.
pub fn process_image_path(pid: u32) -> Result<PathBuf> {
    #[repr(C)]
    struct SystemProcessIdInformation {
        process_id: *mut core::ffi::c_void,
        image_name: UNICODE_STRING,
    }
    const SYSTEM_PROCESS_ID_INFORMATION: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(88);

    let mut buf = vec![0u16; 0x7FFF];
    let mut info = SystemProcessIdInformation {
        process_id: pid as usize as *mut _,
        image_name: UNICODE_STRING {
            Length: 0,
            MaximumLength: (buf.len() * 2) as u16,
            Buffer: PWSTR(buf.as_mut_ptr()),
        },
    };
    unsafe {
        NtQuerySystemInformation(
            SYSTEM_PROCESS_ID_INFORMATION,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<SystemProcessIdInformation>() as u32,
            std::ptr::null_mut(),
        )
    }
    .ok()
    .map_err(|e| anyhow!("query image of process {pid}: {e}"))?;
    let name = String::from_utf16_lossy(&buf[..info.image_name.Length as usize / 2]);
    Ok(PathBuf::from(format!(r"\\?\GLOBALROOT{name}")))
}

/// `name` in the directory of the running executable.
pub fn installed_sibling(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    Ok(exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?
        .join(name))
}

/// Whether two paths name the same existing file after canonicalization,
/// compared case-insensitively. Image-path pinning raises the bar for a
/// renamed or relocated binary; it is not a boundary against same-user code.
pub fn same_installed_binary(expected: &Path, actual: &Path) -> bool {
    match (
        std::fs::canonicalize(expected),
        std::fs::canonicalize(actual),
    ) {
        (Ok(expected), Ok(actual)) => {
            expected.to_string_lossy().to_lowercase() == actual.to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn open_process(pid: u32) -> Result<Self> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map(Self)
            .map_err(|e| anyhow!("OpenProcess({pid}): {e}"))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_token_identity(process: HANDLE) -> Result<(String, u32)> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .map_err(|e| anyhow!("OpenProcessToken: {e}"))?;
    let token = OwnedHandle(token);
    token_identity(token.0)
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

    let sid = sid_to_string(unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid });

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

fn sid_to_string(sid: PSID) -> Result<String> {
    let mut sid_string = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut sid_string) }
        .map_err(|e| anyhow!("ConvertSidToStringSidW: {e}"))?;
    let text = unsafe { sid_string.to_string() }.map_err(|e| anyhow!("SID to string: {e}"));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_string.0 as *mut _)));
    }
    text
}

/// Numeric `S-1-…` form of a SID string or SDDL alias (`LA` becomes the
/// machine's `S-1-5-21-…-500`). Token SIDs always arrive in this form.
pub fn canonical_sid(sid: &str) -> Result<String> {
    let wide: Vec<u16> = sid.encode_utf16().chain([0]).collect();
    let mut psid = PSID::default();
    unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid) }
        .map_err(|e| anyhow!("ConvertStringSidToSidW({sid}): {e}"))?;
    let text = sid_to_string(psid);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psid.0)));
    }
    text
}

/// Principals that stand for many accounts or for none: world, creator and
/// capability SIDs, NT AUTHORITY well-knowns (SY, IU, AU, …), BUILTIN groups,
/// Guest, and the well-known domain groups. Never coordinators.
fn is_broad_principal(canonical: &str) -> bool {
    let parts: Vec<&str> = canonical.split('-').skip(2).collect();
    match parts.as_slice() {
        ["0" | "1" | "2" | "3" | "15" | "16", ..] => true,
        ["5", _] | ["5", "32", ..] => true,
        ["5", "21", .., rid] => rid
            .parse::<u32>()
            .is_ok_and(|rid| matches!(rid, 498 | 501 | 512..=527 | 553 | 571 | 572)),
        [] | [_] => true,
        _ => false,
    }
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

/// Protected broker-pipe DACL for the concrete coordinator SIDs plus SYSTEM.
pub fn coordinator_dacl_sddl(sids: &[String]) -> Result<String> {
    if sids.is_empty() {
        bail!("no broker coordinator SID configured");
    }
    let mut sddl = String::from("D:P");
    for sid in sids {
        validate_sid_string(sid)?;
        sddl.push_str(&format!("(A;;GA;;;{sid})"));
    }
    sddl.push_str("(A;;GA;;;SY)");
    Ok(sddl)
}

/// Concrete account SIDs granted SERVICE_START (`RP`) by a service DACL.
/// Well-known aliases (`SY`, `BA`, `IU`, ...) are never coordinators.
pub fn coordinator_sids_from_sddl(sddl: &str) -> Vec<String> {
    const SERVICE_START: u32 = 0x10;
    let Some(d_pos) = sddl.find("D:") else {
        return Vec::new();
    };
    let dacl = &sddl[d_pos + 2..];
    let dacl = &dacl[..dacl.find("S:").unwrap_or(dacl.len())];
    let mut sids: Vec<String> = Vec::new();
    for ace in dacl.split('(').skip(1) {
        let ace = ace.split(')').next().unwrap_or_default();
        let fields: Vec<&str> = ace.split(';').collect();
        let [kind, _, rights, _, _, account] = fields[..] else {
            continue;
        };
        let grants_start = match rights.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).is_ok_and(|m| m & SERVICE_START != 0),
            None => rights.as_bytes().chunks(2).any(|code| code == b"RP"),
        };
        if kind != "A" || !grants_start {
            continue;
        }
        let Ok(account) = canonical_sid(account) else {
            continue;
        };
        if !is_broad_principal(&account)
            && validate_sid_string(&account).is_ok()
            && !sids.contains(&account)
        {
            sids.push(account);
        }
    }
    sids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dacl_is_protected_and_names_only_coordinators_and_system() {
        let sddl = coordinator_dacl_sddl(&["S-1-5-21-1-2-3-1001".into()]).unwrap();
        assert_eq!(sddl, "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)");
        let two = coordinator_dacl_sddl(&["S-1-5-21-1".into(), "S-1-12-1-2".into()]).unwrap();
        assert_eq!(
            two,
            "D:P(A;;GA;;;S-1-5-21-1)(A;;GA;;;S-1-12-1-2)(A;;GA;;;SY)"
        );
        assert!(!two.contains(";;;IU)"));
        assert!(!two.contains(";;;WD)"));
    }

    #[test]
    fn dacl_without_a_coordinator_fails_closed() {
        assert!(coordinator_dacl_sddl(&[]).is_err());
    }

    #[test]
    fn sid_validation_blocks_sddl_injection() {
        assert!(coordinator_dacl_sddl(&["S-1-5-21-1)(A;;GA;;;WD".into()]).is_err());
        assert!(coordinator_dacl_sddl(&["IU".into()]).is_err());
    }

    #[test]
    fn service_dacl_yields_only_concrete_start_grantees() {
        let sddl = concat!(
            "D:(A;;RPLC;;;S-1-5-21-1-2-3-1001)(A;;CCLCSWRPWPDTLOCRRC;;;SY)",
            "(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;LC;;;S-1-5-21-9)",
            "(D;;RP;;;S-1-5-21-7)(A;;0x10;;;S-1-12-1-5)(A;;RPLC;;;S-1-5-21-1-2-3-1001)",
            "S:(AU;FA;RP;;;S-1-5-21-8)"
        );
        assert_eq!(
            coordinator_sids_from_sddl(sddl),
            ["S-1-5-21-1-2-3-1001", "S-1-12-1-5"]
        );
    }

    #[test]
    fn aliased_builtin_administrator_is_a_coordinator() {
        let sids = coordinator_sids_from_sddl("D:(A;;RPLC;;;LA)(A;;CCLCSWRPWPDTLOCRRC;;;SY)");
        assert_eq!(sids.len(), 1);
        assert!(sids[0].starts_with("S-1-5-21-") && sids[0].ends_with("-500"));
        assert_eq!(canonical_sid("LA").unwrap(), sids[0]);
        assert_eq!(canonical_sid(&sids[0]).unwrap(), sids[0]);
    }

    #[test]
    fn broad_principals_are_never_coordinators() {
        for alias in [
            "SY", "LS", "NS", "BA", "BU", "BG", "IU", "AU", "WD", "CO", "LG",
        ] {
            let sddl = format!("D:(A;;RPLC;;;{alias})");
            assert!(coordinator_sids_from_sddl(&sddl).is_empty(), "{alias}");
        }
        for sid in [
            "S-1-5-21-1-2-3-513",
            "S-1-5-21-1-2-3-512",
            "S-1-15-2-1",
            "S-1-16-12288",
        ] {
            assert!(is_broad_principal(sid), "{sid}");
        }
        for sid in [
            "S-1-5-21-1-2-3-1001",
            "S-1-5-21-1-2-3-500",
            "S-1-12-1-5-6-7-8",
        ] {
            assert!(!is_broad_principal(sid), "{sid}");
        }
    }

    #[test]
    fn rights_are_matched_as_two_letter_codes() {
        // "CRPC" contains R,P only across a code boundary.
        assert_eq!(
            coordinator_sids_from_sddl("D:(A;;WPRP;;;S-1-5-21-1)(A;;CRPC;;;S-1-5-21-2)"),
            ["S-1-5-21-1"]
        );
        assert!(coordinator_sids_from_sddl("O:SYG:SY").is_empty());
    }

    #[test]
    fn current_process_resolves_to_itself() {
        let pid = std::process::id();
        assert_eq!(
            process_identity(pid).unwrap(),
            current_process_identity().unwrap()
        );
        let exe = std::env::current_exe().unwrap();
        assert!(same_installed_binary(
            &exe,
            &process_image_path(pid).unwrap()
        ));
        assert!(!same_installed_binary(
            &exe.with_file_name("other.exe"),
            &process_image_path(pid).unwrap()
        ));
    }

    #[test]
    fn image_of_a_process_we_cannot_open_is_still_resolved() {
        // PID 4 is the System process, which no user can open.
        assert!(process_image_path(4).is_ok());
        assert!(process_image_path(u32::MAX - 2).is_err());
    }

    #[test]
    fn installed_binary_comparison_ignores_case_and_rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("halod.exe");
        let b = dir.path().join("x.exe");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        assert!(same_installed_binary(&a, &dir.path().join("HALOD.EXE")));
        assert!(!same_installed_binary(&a, &b));
        assert!(!same_installed_binary(&a, &dir.path().join("missing.exe")));
    }
}

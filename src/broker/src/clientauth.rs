// SPDX-License-Identifier: GPL-3.0-or-later
//! Identify the user behind a connected named-pipe client and bind the broker to
//! exactly one interactive session.
//!
//! The pipe DACL keeps out session-0 / network callers, but it grants every
//! *interactive* user (`IU`) — so on a multi-session box (fast-user-switching,
//! RDP) a second logged-in user could otherwise connect to the elevated broker
//! and issue raw register writes. To close that, the broker impersonates each
//! connecting client, reads its token's SID + logon-session id, and admits only
//! the concrete identity supplied through the authenticated broker bootstrap.
//! No caller can claim an unbound broker by winning a connection race.
//!
//! The FFI (impersonate → read token → revert) is kept small; the admission
//! decision itself is a pure function ([`decide`]) so it can be unit-tested
//! without a live pipe.

use anyhow::{anyhow, bail, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, RevertToSelf, TokenSessionId, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

/// The user identity behind a pipe connection: a string SID plus the logon
/// session id. Two connections are "the same user" only if both match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub sid: String,
    pub session: u32,
}

/// Outcome of testing a fresh connection against the current binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Serve it: it is the coordinator (or the first client, now bound).
    Ok,
    /// Refuse: the broker is bound to a different user/session.
    WrongUser,
    /// Refuse: too many clients are already connected.
    TooMany,
}

/// Broker-side gate state: the bound coordinator (if any) and the live count.
#[derive(Debug, Default)]
pub struct Gate {
    active: usize,
}

impl Gate {
    pub const fn new() -> Self {
        Gate { active: 0 }
    }

    pub fn active(&self) -> usize {
        self.active
    }
}

/// Admit only the exact coordinator identity configured at broker startup.
pub fn decide(
    gate: &mut Gate,
    id: &ClientIdentity,
    expected: &ClientIdentity,
    max_clients: usize,
) -> Admission {
    if gate.active >= max_clients {
        return Admission::TooMany;
    }
    if id != expected {
        return Admission::WrongUser;
    }
    gate.active += 1;
    Admission::Ok
}

/// Record a served connection ending. The configured coordinator remains fixed
/// for the whole broker process lifetime.
pub fn release(gate: &mut Gate) {
    gate.active = gate.active.saturating_sub(1);
}

/// Impersonate the client connected on `pipe`, read its token, and return its
/// SID + logon-session id. Reverts impersonation before returning on every path.
pub fn pipe_client_identity(pipe: HANDLE) -> Result<ClientIdentity> {
    // SAFETY: `pipe` is a connected pipe-server handle; the matching
    // RevertToSelf below runs on every return path.
    unsafe { ImpersonateNamedPipeClient(pipe) }
        .map_err(|e| anyhow!("ImpersonateNamedPipeClient: {e}"))?;
    let result = read_impersonated_identity();
    // SAFETY: paired with the impersonation above; failure here would leave the
    // thread impersonating, so it is logged rather than ignored.
    unsafe { RevertToSelf() }.map_err(|e| anyhow!("RevertToSelf after client auth: {e}"))?;
    result
}

/// Read the impersonated thread token's user SID and session id. Must run only
/// while impersonating (between Impersonate/Revert).
fn read_impersonated_identity() -> Result<ClientIdentity> {
    let mut token = HANDLE::default();
    // SAFETY: reads the current thread's impersonation token; `token` is closed
    // by the guard below.
    unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
        .map_err(|e| anyhow!("OpenThreadToken: {e}"))?;
    let _guard = TokenHandle(token);
    let sid = token_user_sid(token)?;
    let session = token_session_id(token)?;
    Ok(ClientIdentity { sid, session })
}

/// Closes a token handle on drop.
struct TokenHandle(HANDLE);
impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a token handle from OpenThreadToken, owned here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn token_user_sid(token: HANDLE) -> Result<String> {
    // Size query, then fetch into a right-sized buffer.
    let mut len = 0u32;
    // SAFETY: a NULL buffer with length 0 is the documented size-probe form; it
    // sets `len` and returns an error we ignore.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    if len == 0 {
        bail!("GetTokenInformation(TokenUser) size query returned 0");
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is `len` bytes; the call fills it with a TOKEN_USER.
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

    // SAFETY: the buffer was populated with a TOKEN_USER whose `User.Sid` points
    // into it and is valid for the duration of the conversion below.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut pstr = PWSTR::null();
    // SAFETY: `sid` is valid; `pstr` receives a LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut pstr) }
        .map_err(|e| anyhow!("ConvertSidToStringSidW: {e}"))?;
    // SAFETY: `pstr` is a valid, NUL-terminated wide string on success.
    let s = unsafe { pstr.to_string() }.map_err(|e| anyhow!("SID to string: {e}"));
    // SAFETY: free the LocalAlloc'd SID string regardless of `s`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(pstr.0 as *mut _)));
    }
    s
}

fn token_session_id(token: HANDLE) -> Result<u32> {
    let mut session = 0u32;
    let mut len = 0u32;
    // SAFETY: TokenSessionId writes a single u32 into `session`.
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
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(sid: &str, session: u32) -> ClientIdentity {
        ClientIdentity {
            sid: sid.to_string(),
            session,
        }
    }

    #[test]
    fn configured_coordinator_is_admitted() {
        let mut gate = Gate::new();
        let a = id("S-1-5-21-1", 1);
        assert_eq!(decide(&mut gate, &a, &a, 4), Admission::Ok);
        assert_eq!(gate.active(), 1);
        assert_eq!(decide(&mut gate, &a, &a, 4), Admission::Ok);
        assert_eq!(gate.active(), 2);
    }

    #[test]
    fn a_different_user_is_refused_while_bound() {
        let mut gate = Gate::new();
        let owner = id("S-1-5-21-1", 1);
        let intruder = id("S-1-5-21-2", 2);
        assert_eq!(decide(&mut gate, &owner, &owner, 4), Admission::Ok);
        assert_eq!(
            decide(&mut gate, &intruder, &owner, 4),
            Admission::WrongUser
        );
        // A refusal does not count against the connection cap.
        assert_eq!(gate.active(), 1);
    }

    #[test]
    fn same_user_different_session_is_refused() {
        let mut gate = Gate::new();
        let expected = id("S-1-5-21-1", 1);
        assert_eq!(decide(&mut gate, &expected, &expected, 4), Admission::Ok);
        assert_eq!(
            decide(&mut gate, &id("S-1-5-21-1", 2), &expected, 4),
            Admission::WrongUser
        );
    }

    #[test]
    fn connection_cap_refuses_excess_clients() {
        let mut gate = Gate::new();
        let a = id("S-1-5-21-1", 1);
        assert_eq!(decide(&mut gate, &a, &a, 2), Admission::Ok);
        assert_eq!(decide(&mut gate, &a, &a, 2), Admission::Ok);
        assert_eq!(decide(&mut gate, &a, &a, 2), Admission::TooMany);
    }

    #[test]
    fn release_does_not_allow_a_different_identity() {
        let mut gate = Gate::new();
        let a = id("S-1-5-21-1", 1);
        assert_eq!(decide(&mut gate, &a, &a, 4), Admission::Ok);
        release(&mut gate);
        assert_eq!(gate.active(), 0);
        let b = id("S-1-5-21-2", 2);
        assert_eq!(decide(&mut gate, &b, &a, 4), Admission::WrongUser);
    }

    #[test]
    fn release_never_underflows() {
        let mut gate = Gate::new();
        release(&mut gate);
        assert_eq!(gate.active(), 0);
    }
}

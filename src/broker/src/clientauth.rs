// SPDX-License-Identifier: GPL-3.0-or-later
//! Identify the user behind a connected named-pipe client and admit only the
//! configured coordinator SIDs.
//!
//! Identity is read from the client *process* token, not by impersonation, so
//! it is known before the client has sent anything and the broker needs no
//! SeImpersonatePrivilege. The admission decision is a pure function
//! ([`decide`]) so it can be unit-tested without a live pipe.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub pid: u32,
    pub sid: String,
    /// Logged only; the same SID in another session is the same principal.
    pub session: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Ok,
    WrongUser,
    TooMany,
}

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

pub fn decide(gate: &mut Gate, sid: &str, allowed: &[String], max_clients: usize) -> Admission {
    if gate.active >= max_clients {
        return Admission::TooMany;
    }
    if !allowed.iter().any(|a| a == sid) {
        return Admission::WrongUser;
    }
    gate.active += 1;
    Admission::Ok
}

pub fn release(gate: &mut Gate) {
    gate.active = gate.active.saturating_sub(1);
}

pub fn client_identity(pipe: HANDLE) -> Result<ClientIdentity> {
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }
        .map_err(|e| anyhow!("GetNamedPipeClientProcessId: {e}"))?;
    let (sid, session) = halod_hwaccess::winsec::process_identity(pid)?;
    Ok(ClientIdentity { pid, sid, session })
}

/// The client's image path when it is `expected`, else the mismatching path.
pub fn client_image_matches(pid: u32, expected: &Path) -> Result<PathBuf, PathBuf> {
    let actual = halod_hwaccess::winsec::process_image_path(pid)
        .map_err(|e| PathBuf::from(format!("<unknown: {e}>")))?;
    if halod_hwaccess::winsec::same_installed_binary(expected, &actual) {
        Ok(actual)
    } else {
        Err(actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(sids: &[&str]) -> Vec<String> {
        sids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn configured_coordinators_are_admitted() {
        let mut gate = Gate::new();
        let sids = allowed(&["S-1-5-21-1", "S-1-5-21-2"]);
        assert_eq!(decide(&mut gate, "S-1-5-21-1", &sids, 4), Admission::Ok);
        assert_eq!(decide(&mut gate, "S-1-5-21-2", &sids, 4), Admission::Ok);
        assert_eq!(gate.active(), 2);
    }

    #[test]
    fn a_different_user_is_refused_without_consuming_a_slot() {
        let mut gate = Gate::new();
        let sids = allowed(&["S-1-5-21-1"]);
        assert_eq!(decide(&mut gate, "S-1-5-21-1", &sids, 4), Admission::Ok);
        assert_eq!(
            decide(&mut gate, "S-1-5-21-2", &sids, 4),
            Admission::WrongUser
        );
        assert_eq!(gate.active(), 1);
    }

    #[test]
    fn no_configured_coordinator_admits_nobody() {
        assert_eq!(
            decide(&mut Gate::new(), "S-1-5-21-1", &[], 4),
            Admission::WrongUser
        );
    }

    #[test]
    fn connection_cap_refuses_excess_clients() {
        let mut gate = Gate::new();
        let sids = allowed(&["S-1-5-21-1"]);
        assert_eq!(decide(&mut gate, "S-1-5-21-1", &sids, 2), Admission::Ok);
        assert_eq!(decide(&mut gate, "S-1-5-21-1", &sids, 2), Admission::Ok);
        assert_eq!(
            decide(&mut gate, "S-1-5-21-1", &sids, 2),
            Admission::TooMany
        );
        release(&mut gate);
        assert_eq!(decide(&mut gate, "S-1-5-21-1", &sids, 2), Admission::Ok);
    }

    #[test]
    fn release_never_underflows() {
        let mut gate = Gate::new();
        release(&mut gate);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn client_is_identified_before_it_sends_data() {
        use crate::pipe::{create_instance, wait_for_client, PipeSecurity};

        let (sid, session) = halod_hwaccess::winsec::current_process_identity().unwrap();
        let name = format!(r"\\.\pipe\halod-auth-test-{}", std::process::id());
        let security = PipeSecurity::from_sddl(
            &halod_hwaccess::winsec::coordinator_dacl_sddl(std::slice::from_ref(&sid)).unwrap(),
        )
        .unwrap();
        let handle = create_instance(&name, &security, true).unwrap();
        let _server = crate::pipe::PipeStream::new(handle);
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
                .unwrap();
            opened_tx.send(()).unwrap();
            let _ = done_rx.recv();
            drop(pipe);
        });

        wait_for_client(handle).unwrap();
        opened_rx.recv().unwrap();
        assert_eq!(
            client_identity(handle).unwrap(),
            ClientIdentity {
                pid: std::process::id(),
                sid,
                session
            }
        );
        let exe = std::env::current_exe().unwrap();
        assert!(client_image_matches(std::process::id(), &exe).is_ok());
        assert!(
            client_image_matches(std::process::id(), &exe.with_file_name("halod.exe")).is_err()
        );
        done_tx.send(()).unwrap();
        client.join().unwrap();
    }
}

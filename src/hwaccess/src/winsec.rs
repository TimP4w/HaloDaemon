// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal Windows security helper for the broker's named pipe.

/// The DACL SDDL string that locks the broker's named pipe to interactive-logon
/// users plus LocalSystem.
///
/// `S-1-5-4` (well-known **Interactive** group) is present only in the token of
/// a user logged on interactively (local console or RDP) — never in a session-0
/// service token or a network/batch logon. So granting it, in a *protected*
/// (`P`) DACL that grants nobody else, restricts the pipe to exactly the kind of
/// process that runs the worker, while the broker itself (LocalSystem, `SY`) can
/// still manage it. This is a static string — no per-user SID query — so it
/// works identically whether the broker runs as the SCM service (LocalSystem,
/// possibly before any user has logged in) or as an elevated dev-run process.
///
/// This is *not* a boundary against a compromised worker (see the threat model):
/// any interactive user on the box could connect. It keeps out session-0 and
/// remote/network callers, which is the point.
pub fn interactive_dacl_sddl() -> String {
    "D:P(A;;GA;;;IU)(A;;GA;;;SY)".to_string()
}

#[cfg(test)]
mod tests {
    use super::interactive_dacl_sddl;

    #[test]
    fn dacl_is_protected_and_grants_only_interactive_and_system() {
        let sddl = interactive_dacl_sddl();
        // Protected DACL — no inherited ACEs can widen access.
        assert!(sddl.starts_with("D:P"));
        // Exactly the two allow-ACEs: Interactive (IU) and LocalSystem (SY).
        assert_eq!(sddl, "D:P(A;;GA;;;IU)(A;;GA;;;SY)");
    }

    #[test]
    fn dacl_excludes_everyone_and_anonymous() {
        let sddl = interactive_dacl_sddl();
        // No World (WD) / Anonymous (AN) grants slipped in.
        assert!(!sddl.contains(";;;WD)"));
        assert!(!sddl.contains(";;;AN)"));
    }
}

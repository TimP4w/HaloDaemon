// SPDX-License-Identifier: GPL-3.0-or-later
//! SSRF guard shared by every network-reaching plugin transport (tcp, http,
//! udp): resolve a host once to a vetted [`SocketAddr`], and classify which IP
//! ranges an untrusted integration must not reach by default.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use anyhow::{bail, Context, Result};

/// Resolve `host:port` **once** and return a single vetted [`SocketAddr`] to
/// connect to — the caller connects to this exact address rather than re-passing
/// the hostname, so a DNS rebind between check and connect can't redirect the
/// socket (resolving twice is the classic SSRF TOCTOU). Unless `allow_private`
/// is set, every resolved address must be routable: a name that resolves to
/// *any* loopback/private/link-local address is rejected, keeping an integration
/// off localhost admin services and the cloud metadata endpoint
/// (`169.254.169.254`).
pub(crate) fn resolve_vetted_addr(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving host '{host}'"))?
        .collect();
    let first = *addrs
        .first()
        .ok_or_else(|| anyhow::anyhow!("host '{host}' resolved to no addresses"))?;
    if allow_private {
        return Ok(first);
    }
    for sa in &addrs {
        if is_blocked_ip(&sa.ip()) {
            bail!(
                "host '{host}' resolves to a non-routable address {} (set allow_private to permit LAN/localhost targets)",
                sa.ip()
            );
        }
    }
    Ok(first)
}

/// Address ranges an untrusted integration must not reach by default.
pub(crate) fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                // Carrier-grade NAT 100.64.0.0/10.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 0x40)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7 and link-local fe80::/10.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: unwrap and re-check against the v4 rules.
                || v6.to_ipv4_mapped().map(|m| is_blocked_ip(&IpAddr::V4(m))).unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_vetted_addr_returns_one_checked_address() {
        // A public literal resolves to exactly itself and is returned, so the
        // caller connects to the vetted address (no second resolution a rebind
        // could redirect). A blocked literal is rejected unless private is allowed.
        let addr = resolve_vetted_addr("1.1.1.1", 443, false).unwrap();
        assert_eq!(addr.to_string(), "1.1.1.1:443");
        assert!(resolve_vetted_addr("127.0.0.1", 80, false).is_err());
        assert_eq!(
            resolve_vetted_addr("127.0.0.1", 80, true)
                .unwrap()
                .to_string(),
            "127.0.0.1:80"
        );
    }

    #[test]
    fn ssrf_guard_classifies_addresses() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // A public address is allowed.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }
}

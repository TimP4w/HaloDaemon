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

/// Whether an address is **not** globally routable, so an untrusted integration
/// must not reach it by default. This is the inverse of the unstable `IpAddr::is_global`:
/// everything outside the public unicast space — private,
/// loopback, link-local, CGN, documentation/TEST-NET, benchmarking, 6to4-anycast,
/// reserved/future, discard, and their IPv6 equivalents — is blocked.
pub(crate) fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                // TEST-NET-1/2/3 (192.0.2/24, 198.51.100/24, 203.0.113/24).
                || v4.is_documentation()
                // "This network" 0.0.0.0/8.
                || a == 0
                // Carrier-grade NAT 100.64.0.0/10.
                || (a == 100 && (b & 0xC0) == 0x40)
                // IETF protocol assignments 192.0.0.0/24.
                || (a == 192 && b == 0 && c == 0)
                // 6to4 relay anycast 192.88.99.0/24 (deprecated).
                || (a == 192 && b == 88 && c == 99)
                // Benchmarking 198.18.0.0/15.
                || (a == 198 && (b & 0xfe) == 18)
                // Reserved/future 240.0.0.0/4 (255.255.255.255 caught by broadcast).
                || (a & 0xf0) == 0xf0
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7 and link-local fe80::/10.
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                // Discard-only 100::/64.
                || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
                // Documentation 2001:db8::/32 and 3fff::/20.
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] & 0xfff0) == 0x3ff0
                // ORCHID/ORCHIDv2 2001:10::/28 and 2001:20::/28.
                || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0010)
                || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0020)
                // Reserved 5f00::/16.
                || s[0] == 0x5f00
                // IPv4-mapped/embedded: unwrap and re-check against the v4 rules.
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

    #[test]
    fn ssrf_guard_blocks_every_special_purpose_range() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let blocked_v4 = [
            (0, 1, 2, 3),          // 0.0.0.0/8 "this network"
            (192, 0, 0, 1),        // 192.0.0.0/24 IETF protocol
            (192, 0, 2, 5),        // TEST-NET-1
            (198, 51, 100, 5),     // TEST-NET-2
            (203, 0, 113, 5),      // TEST-NET-3
            (198, 18, 0, 1),       // 198.18.0.0/15 benchmarking
            (198, 19, 255, 254),   // benchmarking upper half
            (192, 88, 99, 1),      // 6to4 relay anycast
            (240, 0, 0, 1),        // 240.0.0.0/4 reserved
            (255, 255, 255, 255),  // limited broadcast
            (224, 0, 0, 1),        // multicast
        ];
        for (a, b, c, d) in blocked_v4 {
            assert!(
                is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(a, b, c, d))),
                "{a}.{b}.{c}.{d} should be blocked"
            );
        }
        // A globally-routable address next to a blocked range stays allowed.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 20, 0, 1))));

        // IPv6 documentation / reserved / discard.
        assert!(is_blocked_ip(&IpAddr::V6("2001:db8::1".parse().unwrap())));
        assert!(is_blocked_ip(&IpAddr::V6("3fff::1".parse().unwrap())));
        assert!(is_blocked_ip(&IpAddr::V6("100::1".parse().unwrap())));
        assert!(is_blocked_ip(&IpAddr::V6("2001:20::1".parse().unwrap())));
        assert!(is_blocked_ip(&IpAddr::V6("5f00::1".parse().unwrap())));
        // A blocked address wrapped as IPv4-mapped stays blocked.
        assert!(is_blocked_ip(&IpAddr::V6(
            Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped()
        )));
        // A global v6 unicast address is allowed.
        assert!(!is_blocked_ip(&IpAddr::V6(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap()
        )));
    }
}

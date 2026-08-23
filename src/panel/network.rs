//! LAN IP autodetection, used as a fallback when the control panel is
//! viewed via `localhost`/`127.0.0.1` (e.g. the person setting it up
//! opens it on the same machine the server runs on) but the pairing QR
//! needs an address a phone on the same WiFi network can actually reach
//! -- a QR encoding `ws://localhost:...` would be meaningless to the
//! phone, which resolves "localhost" to itself.

use std::net::{IpAddr, UdpSocket};

/// Finds the local IP address the OS would route outbound traffic
/// through, without actually sending any packets: `UdpSocket::connect`
/// on a UDP socket only performs local route selection (UDP is
/// connectionless), it doesn't touch the network. `8.8.8.8` is just a
/// well-known IPv4 address to route toward -- nothing is sent to it.
/// Returns `None` if there's no outbound route at all (e.g. a fully
/// isolated network with no default gateway); callers should fall back
/// to something else and let the user override via
/// `PANEL_PUBLIC_WS_BASE`.
pub fn detect_lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// Strips a trailing `:port` from an HTTP `Host` header value, handling
/// IPv6 literals in bracket notation (`[::1]:40000`).
pub fn strip_port(host_header: &str) -> &str {
    if let Some(rest) = host_header.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    host_header.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_loopback_host_recognizes_common_forms() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.50"));
        assert!(!is_loopback_host("relay.example.com"));
    }

    #[test]
    fn strip_port_handles_ipv4_and_hostnames() {
        assert_eq!(strip_port("192.168.1.50:40000"), "192.168.1.50");
        assert_eq!(strip_port("relay.example.com:40000"), "relay.example.com");
        assert_eq!(strip_port("localhost"), "localhost");
        assert_eq!(strip_port("localhost:40000"), "localhost");
    }

    #[test]
    fn strip_port_handles_ipv6_bracket_notation() {
        assert_eq!(strip_port("[::1]:40000"), "::1");
        assert_eq!(strip_port("[fe80::1]:40000"), "fe80::1");
        assert_eq!(strip_port("[::1]"), "::1");
    }
}

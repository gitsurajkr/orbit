//! QUIC endpoint setup.
//!
//! TLS: a fresh self-signed certificate each run, and the client doesn't verify it yet.
//! Tailscale (WireGuard) already encrypts and authenticates which devices can reach this port;
//! ORBIT's own pairing comes in the auth milestone.

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

pub fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    // Notice a vanished client within 5 s (then the capture stops).
    t.max_idle_timeout(Some(Duration::from_secs(5).try_into().unwrap()));
    // Fixed 1200-byte packets: fits Tailscale's 1280 MTU, and the datagram size never changes
    // under us mid-frame.
    t.initial_mtu(1200);
    t.mtu_discovery_config(None);
    t.datagram_send_buffer_size(crate::session::DATAGRAM_SEND_BUFFER);
    t.datagram_receive_buffer_size(Some(4 << 20));
    t
}

pub fn endpoint(bind: SocketAddr) -> Result<quinn::Endpoint> {
    let cert = rcgen::generate_simple_self_signed(vec!["orbit".to_string()])?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())?;
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport()));
    quinn::Endpoint::server(cfg, bind).with_context(|| format!("bind {bind}"))
}

fn is_cgnat(ip: Ipv4Addr) -> bool {
    ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64
}

/// Where ORBIT listens: every local address, port 9000. Binding to the Tailscale IP itself broke
/// after reboots (Tailscale's adapter comes and goes while it starts, leaving a dead socket);
/// instead, connections from anything but a Tailscale address are refused (see [`is_tailnet`]).
pub fn default_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], proto::DEFAULT_PORT))
}

/// True for Tailscale addresses (100.64.0.0/10, fd7a:115c:a1e0::/48) and loopback (for testing).
/// Everything else (LAN, internet) is refused: until ORBIT has its own pairing, Tailscale is the
/// lock on the door.
pub fn is_tailnet(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_cgnat(v4) || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_cgnat(v4) || v4.is_loopback(),
            None => v6.is_loopback() || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
        },
    }
}

/// This PC's Tailscale IPv4 address: an address in 100.64.0.0/10 on a network adapter that is up
/// and named "Tailscale". The adapter check matters: ISPs (carrier-grade NAT), hotspots and other
/// VPNs hand out addresses in the same 100.64.0.0/10 range, and those must not be mistaken for
/// Tailscale.
pub fn tailscale_ip() -> Option<Ipv4Addr> {
    use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 32 * 1024u32;
    let mut buf: Vec<u64> = Vec::new(); // u64 for alignment
    for _ in 0..3 {
        buf.resize((size as usize).div_ceil(8), 0);
        let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH), &mut size) };
        if r == 0 {
            break;
        }
        if r != 111 {
            // anything but ERROR_BUFFER_OVERFLOW
            return None;
        }
    }
    let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    unsafe {
        while !a.is_null() {
            let ad = &*a;
            let name = ad.FriendlyName.to_string().unwrap_or_default().to_lowercase();
            let desc = ad.Description.to_string().unwrap_or_default().to_lowercase();
            if ad.OperStatus == IfOperStatusUp && (name.contains("tailscale") || desc.contains("tailscale")) {
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let sa = (*u).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                        if is_cgnat(ip) {
                            return Some(ip);
                        }
                    }
                    u = (*u).Next;
                }
            }
            a = ad.Next;
        }
    }
    None
}

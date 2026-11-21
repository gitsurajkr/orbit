//! QUIC endpoint setup.
//!
//! TLS: a fresh self-signed certificate each run, and the client doesn't verify it yet.
//! Tailscale (WireGuard) already encrypts and authenticates which devices can reach this port;
//! ORBIT's own pairing comes in the auth milestone.

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
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

/// This PC's Tailscale IPv4 address (100.64.0.0/10), found by asking the OS which local address
/// routes to Tailscale's service IP. Nothing is sent.
pub fn tailscale_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("100.100.100.100:53").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64 => Some(ip),
        _ => None,
    }
}

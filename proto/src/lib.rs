//! Everything the server and client must agree on: the wire format, frame fragmentation and
//! reassembly, clock sync, and the stats helpers both sides print.
//!
//! Transport (set up by the binaries): one QUIC connection over Tailscale.
//! - Reliable bidirectional stream opened by the client: [`ctrl::Ctrl`] messages
//!   (hello, keyframe requests, keys, mouse buttons, wheel, ping/pong).
//! - Unreliable datagrams: video fragments (server → client, [`video`]) and mouse moves
//!   (client → server, [`ctrl::MouseMove`]). A late or lost datagram is never retransmitted,
//!   so nothing old can ever queue up in front of something new.

pub mod clock;
pub mod ctrl;
pub mod stats;
pub mod video;

/// Bumped whenever any byte layout in this crate changes.
pub const VERSION: u16 = 1;
/// QUIC ALPN, so an ORBIT client can't talk to something else by accident.
pub const ALPN: &[u8] = b"orbit/1";
/// Default UDP port.
pub const DEFAULT_PORT: u16 = 9000;

/// First byte of every datagram: what kind it is.
pub const DG_VIDEO: u8 = 0x10;
pub const DG_MOUSE_MOVE: u8 = 0x20;

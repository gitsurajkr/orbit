//! Control messages on the reliable stream, and the mouse-move datagram.
//!
//! Stream framing (little-endian): `u8 tag | u16 body_len | body`.

use anyhow::{bail, ensure, Result};

pub const HEADER_LEN: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctrl {
    /// C→S, first message.
    ClientHello { version: u16 },
    /// S→C, reply to hello: the captured monitor's size and the capture rate cap.
    ServerHello { version: u16, width: u16, height: u16, fps: u16 },
    /// C→S. Sent repeatedly (rate-limited) until a keyframe actually arrives.
    KeyframeRequest,
    /// C→S. `scan` is the hardware scan code, so it's independent of keyboard layout.
    Key { scan: u16, extended: bool, down: bool },
    /// C→S. 0 left, 1 right, 2 middle, 3 X1, 4 X2.
    MouseButton { button: u8, down: bool },
    /// C→S. Wheel delta in Windows units (120 = one notch).
    Wheel { delta: i16, horizontal: bool },
    /// C→S, clock sync: client's clock when sent.
    Ping { client_us: u64 },
    /// S→C, clock sync: echo plus server's clock when received.
    Pong { client_us: u64, server_us: u64 },
}

mod tag {
    pub const CLIENT_HELLO: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    pub const KEYFRAME_REQUEST: u8 = 3;
    pub const KEY: u8 = 4;
    pub const MOUSE_BUTTON: u8 = 5;
    pub const WHEEL: u8 = 6;
    pub const PING: u8 = 7;
    pub const PONG: u8 = 8;
}

impl Ctrl {
    /// Appends header + body to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0, 0]);
        let t = match *self {
            Ctrl::ClientHello { version } => {
                out.extend_from_slice(&version.to_le_bytes());
                tag::CLIENT_HELLO
            }
            Ctrl::ServerHello { version, width, height, fps } => {
                for v in [version, width, height, fps] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                tag::SERVER_HELLO
            }
            Ctrl::KeyframeRequest => tag::KEYFRAME_REQUEST,
            Ctrl::Key { scan, extended, down } => {
                out.extend_from_slice(&scan.to_le_bytes());
                out.push(extended as u8);
                out.push(down as u8);
                tag::KEY
            }
            Ctrl::MouseButton { button, down } => {
                out.push(button);
                out.push(down as u8);
                tag::MOUSE_BUTTON
            }
            Ctrl::Wheel { delta, horizontal } => {
                out.extend_from_slice(&delta.to_le_bytes());
                out.push(horizontal as u8);
                tag::WHEEL
            }
            Ctrl::Ping { client_us } => {
                out.extend_from_slice(&client_us.to_le_bytes());
                tag::PING
            }
            Ctrl::Pong { client_us, server_us } => {
                out.extend_from_slice(&client_us.to_le_bytes());
                out.extend_from_slice(&server_us.to_le_bytes());
                tag::PONG
            }
        };
        let body_len = (out.len() - start - HEADER_LEN) as u16;
        out[start] = t;
        out[start + 1..start + 3].copy_from_slice(&body_len.to_le_bytes());
    }

    /// Splits a 3-byte header into (tag, body_len).
    pub fn parse_header(h: [u8; HEADER_LEN]) -> (u8, usize) {
        (h[0], u16::from_le_bytes([h[1], h[2]]) as usize)
    }

    pub fn decode(tag: u8, b: &[u8]) -> Result<Ctrl> {
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let need = |n: usize| -> Result<()> {
            ensure!(b.len() >= n, "ctrl tag {tag}: body {} < {n}", b.len());
            Ok(())
        };
        Ok(match tag {
            tag::CLIENT_HELLO => {
                need(2)?;
                Ctrl::ClientHello { version: u16_at(0) }
            }
            tag::SERVER_HELLO => {
                need(8)?;
                Ctrl::ServerHello { version: u16_at(0), width: u16_at(2), height: u16_at(4), fps: u16_at(6) }
            }
            tag::KEYFRAME_REQUEST => Ctrl::KeyframeRequest,
            tag::KEY => {
                need(4)?;
                Ctrl::Key { scan: u16_at(0), extended: b[2] != 0, down: b[3] != 0 }
            }
            tag::MOUSE_BUTTON => {
                need(2)?;
                Ctrl::MouseButton { button: b[0], down: b[1] != 0 }
            }
            tag::WHEEL => {
                need(3)?;
                Ctrl::Wheel { delta: u16_at(0) as i16, horizontal: b[2] != 0 }
            }
            tag::PING => {
                need(8)?;
                Ctrl::Ping { client_us: u64_at(0) }
            }
            tag::PONG => {
                need(16)?;
                Ctrl::Pong { client_us: u64_at(0), server_us: u64_at(8) }
            }
            t => bail!("unknown ctrl tag {t}"),
        })
    }
}

/// Mouse move, sent as a datagram: `u8 DG_MOUSE_MOVE | u32 seq | u16 x | u16 y`.
/// x/y are absolute positions on the captured monitor, scaled to 0..=65535.
/// A datagram can arrive late or out of order; the server ignores any `seq` older than the
/// newest it has applied, so the pointer never jumps backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMove {
    pub seq: u32,
    pub x: u16,
    pub y: u16,
}

impl MouseMove {
    pub const LEN: usize = 9;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = crate::DG_MOUSE_MOVE;
        b[1..5].copy_from_slice(&self.seq.to_le_bytes());
        b[5..7].copy_from_slice(&self.x.to_le_bytes());
        b[7..9].copy_from_slice(&self.y.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<MouseMove> {
        ensure!(b.len() >= Self::LEN && b[0] == crate::DG_MOUSE_MOVE, "bad mouse datagram");
        Ok(MouseMove {
            seq: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            x: u16::from_le_bytes([b[5], b[6]]),
            y: u16::from_le_bytes([b[7], b[8]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let all = [
            Ctrl::ClientHello { version: 1 },
            Ctrl::ServerHello { version: 1, width: 2560, height: 1440, fps: 120 },
            Ctrl::KeyframeRequest,
            Ctrl::Key { scan: 0x1d, extended: true, down: false },
            Ctrl::MouseButton { button: 4, down: true },
            Ctrl::Wheel { delta: -240, horizontal: true },
            Ctrl::Ping { client_us: u64::MAX - 1 },
            Ctrl::Pong { client_us: 7, server_us: 1 << 40 },
        ];
        let mut buf = Vec::new();
        for m in &all {
            m.encode(&mut buf);
        }
        let mut rest = &buf[..];
        for m in &all {
            let (t, len) = Ctrl::parse_header(rest[..HEADER_LEN].try_into().unwrap());
            let body = &rest[HEADER_LEN..HEADER_LEN + len];
            assert_eq!(Ctrl::decode(t, body).unwrap(), *m);
            rest = &rest[HEADER_LEN + len..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn short_body_is_an_error_not_a_panic() {
        assert!(Ctrl::decode(tag::PONG, &[0; 15]).is_err());
        assert!(Ctrl::decode(99, &[]).is_err());
    }

    #[test]
    fn mouse_round_trips() {
        let m = MouseMove { seq: 123456, x: 65535, y: 1 };
        assert_eq!(MouseMove::decode(&m.encode()).unwrap(), m);
        assert!(MouseMove::decode(&[crate::DG_MOUSE_MOVE, 0]).is_err());
    }
}

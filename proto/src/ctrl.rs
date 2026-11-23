//! Control messages on the reliable stream, and the mouse-move datagram.
//!
//! Stream framing (little-endian): `u8 tag | u16 body_len | body`.

use anyhow::{bail, ensure, Result};

pub const HEADER_LEN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ctrl {
    /// C→S, first message. `flags`: [`hello::CLIPBOARD`]. `name` is the viewer PC's name, for
    /// the sharing PC's device list.
    ClientHello { version: u16, flags: u8, name: String },
    /// S→C, reply to hello: the captured monitor's size and the capture rate cap.
    ServerHello { version: u16, width: u16, height: u16, fps: u16 },
    /// S→C, instead of [`Ctrl::ServerHello`]: this viewer isn't on the sharing PC's device list.
    /// With `code_active` the viewer may answer with [`Ctrl::PairStart`] (6-digit code);
    /// otherwise the sharing PC first has to show a code.
    PairRequired { code_active: bool },
    /// C→S: first SPAKE2 message of the code check (see `auth::pake`).
    PairStart { msg: Vec<u8> },
    /// S→C: SPAKE2 answer.
    PairReply { msg: Vec<u8> },
    /// Both ways: proof of knowing the code (viewer first; the sharing PC answers only if the
    /// viewer's proof was right).
    PairProof { proof: Vec<u8> },
    /// S→C: connection refused, with the reason to show. The connection closes after it.
    Denied { reason: String },
    /// C→S: frames from `from` on were lost; invalidate them and send a recovery frame.
    RefInvalidate { from: u32 },
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
    /// S→C: the remote pointer's shape changed. `kind` is [`cursor::HIDDEN`], a standard shape
    /// (index into [`cursor::STANDARD`] + 1) or [`cursor::CUSTOM`] with `key` naming an image
    /// sent earlier in a [`Ctrl::CursorImage`].
    Cursor { kind: u8, key: u32 },
    /// S→C: the sharing PC's screens and which one is shown (after the hello, and on change).
    Monitors { current: u8, list: Vec<MonitorDesc> },
    /// C→S: show screen `index` of [`Ctrl::Monitors`].
    SelectMonitor { index: u8 },
    /// S→C: the video's size changed (another screen is shown).
    VideoFormat { width: u16, height: u16 },
    /// S→C: an app-defined pointer image, sent once per distinct image before the first
    /// [`Ctrl::Cursor`] that uses it. `key` is a hash of the pixels; `bgra` is top-down,
    /// straight (not premultiplied) alpha, `width * height * 4` bytes.
    CursorImage { key: u32, hot_x: u16, hot_y: u16, width: u16, height: u16, bgra: Vec<u8> },
}

/// One screen in [`Ctrl::Monitors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorDesc {
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

/// [`Ctrl::ClientHello`] flags.
pub mod hello {
    /// The viewer wants clipboard sync (both ways).
    pub const CLIPBOARD: u8 = 1;
}

/// Pointer shapes, shared by both sides.
pub mod cursor {
    pub const HIDDEN: u8 = 0;
    pub const CUSTOM: u8 = 255;
    /// Largest custom pointer sent (a bigger one shows as the arrow). 96×96×4 bytes keeps a
    /// [`super::Ctrl::CursorImage`] under the 64 KiB message limit.
    pub const MAX_SIZE: u16 = 96;
    /// Windows' standard cursors (`IDC_*` resource ids), in wire order: kind = index + 1.
    pub const STANDARD: [u16; 16] = [
        32512, // IDC_ARROW
        32513, // IDC_IBEAM
        32514, // IDC_WAIT
        32515, // IDC_CROSS
        32516, // IDC_UPARROW
        32642, // IDC_SIZENWSE
        32643, // IDC_SIZENESW
        32644, // IDC_SIZEWE
        32645, // IDC_SIZENS
        32646, // IDC_SIZEALL
        32648, // IDC_NO
        32649, // IDC_HAND
        32650, // IDC_APPSTARTING
        32651, // IDC_HELP
        32671, // IDC_PIN
        32672, // IDC_PERSON
    ];

    /// FNV-1a over the image, its size and hotspot: the cache key for a custom pointer.
    pub fn image_key(width: u16, height: u16, hot_x: u16, hot_y: u16, bgra: &[u8]) -> u32 {
        let mut h: u32 = 0x811c_9dc5;
        for b in [width, height, hot_x, hot_y].iter().flat_map(|v| v.to_le_bytes()).chain(bgra.iter().copied()) {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }
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
    pub const CURSOR: u8 = 9;
    pub const CURSOR_IMAGE: u8 = 10;
    pub const PAIR_REQUIRED: u8 = 11;
    pub const PAIR_START: u8 = 12;
    pub const PAIR_REPLY: u8 = 13;
    pub const PAIR_PROOF: u8 = 14;
    pub const DENIED: u8 = 15;
    pub const MONITORS: u8 = 16;
    pub const SELECT_MONITOR: u8 = 17;
    pub const VIDEO_FORMAT: u8 = 18;
    pub const REF_INVALIDATE: u8 = 19;
}

/// Longest name / reason text carried (bytes, cut at a character boundary).
const MAX_TEXT: usize = 200;

fn put_text(out: &mut Vec<u8>, s: &str) {
    let mut end = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&s.as_bytes()[..end]);
}

impl Ctrl {
    /// Appends header + body to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0, 0]);
        let t = match *self {
            Ctrl::Cursor { kind, key } => {
                out.push(kind);
                out.extend_from_slice(&key.to_le_bytes());
                tag::CURSOR
            }
            Ctrl::CursorImage { key, hot_x, hot_y, width, height, ref bgra } => {
                out.extend_from_slice(&key.to_le_bytes());
                for v in [hot_x, hot_y, width, height] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(bgra);
                tag::CURSOR_IMAGE
            }
            Ctrl::ClientHello { version, flags, ref name } => {
                out.extend_from_slice(&version.to_le_bytes());
                out.push(flags);
                put_text(out, name);
                tag::CLIENT_HELLO
            }
            Ctrl::Monitors { current, ref list } => {
                out.push(current);
                out.push(list.len().min(255) as u8);
                for m in list.iter().take(255) {
                    out.extend_from_slice(&m.width.to_le_bytes());
                    out.extend_from_slice(&m.height.to_le_bytes());
                    out.push(m.primary as u8);
                }
                tag::MONITORS
            }
            Ctrl::SelectMonitor { index } => {
                out.push(index);
                tag::SELECT_MONITOR
            }
            Ctrl::RefInvalidate { from } => {
                out.extend_from_slice(&from.to_le_bytes());
                tag::REF_INVALIDATE
            }
            Ctrl::VideoFormat { width, height } => {
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                tag::VIDEO_FORMAT
            }
            Ctrl::PairRequired { code_active } => {
                out.push(code_active as u8);
                tag::PAIR_REQUIRED
            }
            Ctrl::PairStart { ref msg } => {
                out.extend_from_slice(msg);
                tag::PAIR_START
            }
            Ctrl::PairReply { ref msg } => {
                out.extend_from_slice(msg);
                tag::PAIR_REPLY
            }
            Ctrl::PairProof { ref proof } => {
                out.extend_from_slice(proof);
                tag::PAIR_PROOF
            }
            Ctrl::Denied { ref reason } => {
                put_text(out, reason);
                tag::DENIED
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
                // v2 clients send only the version; the rest is then empty (and the version
                // check refuses them anyway).
                let flags = b.get(2).copied().unwrap_or(0);
                let name = b.get(3..).map(|n| String::from_utf8_lossy(&n[..n.len().min(MAX_TEXT)]).into_owned()).unwrap_or_default();
                Ctrl::ClientHello { version: u16_at(0), flags, name }
            }
            tag::MONITORS => {
                need(2)?;
                let n = b[1] as usize;
                need(2 + n * 5)?;
                let list = (0..n).map(|i| MonitorDesc { width: u16_at(2 + i * 5), height: u16_at(4 + i * 5), primary: b[6 + i * 5] != 0 }).collect();
                Ctrl::Monitors { current: b[0], list }
            }
            tag::SELECT_MONITOR => {
                need(1)?;
                Ctrl::SelectMonitor { index: b[0] }
            }
            tag::REF_INVALIDATE => {
                need(4)?;
                Ctrl::RefInvalidate { from: u32::from_le_bytes(b[0..4].try_into().unwrap()) }
            }
            tag::VIDEO_FORMAT => {
                need(4)?;
                Ctrl::VideoFormat { width: u16_at(0), height: u16_at(2) }
            }
            tag::PAIR_REQUIRED => {
                need(1)?;
                Ctrl::PairRequired { code_active: b[0] != 0 }
            }
            tag::PAIR_START => Ctrl::PairStart { msg: b.to_vec() },
            tag::PAIR_REPLY => Ctrl::PairReply { msg: b.to_vec() },
            tag::PAIR_PROOF => Ctrl::PairProof { proof: b.to_vec() },
            tag::DENIED => Ctrl::Denied { reason: String::from_utf8_lossy(&b[..b.len().min(MAX_TEXT)]).into_owned() },
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
            tag::CURSOR => {
                need(5)?;
                Ctrl::Cursor { kind: b[0], key: u32::from_le_bytes(b[1..5].try_into().unwrap()) }
            }
            tag::CURSOR_IMAGE => {
                need(12)?;
                let (width, height) = (u16_at(8), u16_at(10));
                ensure!(width <= cursor::MAX_SIZE && height <= cursor::MAX_SIZE, "cursor {width}x{height} too big");
                let len = width as usize * height as usize * 4;
                need(12 + len)?;
                Ctrl::CursorImage { key: u32::from_le_bytes(b[0..4].try_into().unwrap()), hot_x: u16_at(4), hot_y: u16_at(6), width, height, bgra: b[12..12 + len].to_vec() }
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
            Ctrl::ClientHello { version: 1, flags: hello::CLIPBOARD, name: "LAPTOP-NOVA".into() },
            Ctrl::ClientHello { version: 1, flags: 0, name: String::new() },
            Ctrl::PairRequired { code_active: true },
            Ctrl::Monitors { current: 1, list: vec![MonitorDesc { width: 2560, height: 1440, primary: true }, MonitorDesc { width: 1920, height: 1080, primary: false }] },
            Ctrl::Monitors { current: 0, list: vec![] },
            Ctrl::SelectMonitor { index: 2 },
            Ctrl::RefInvalidate { from: 0xFFFF_FFF0 },
            Ctrl::VideoFormat { width: 1920, height: 1080 },
            Ctrl::PairStart { msg: vec![1, 2, 3] },
            Ctrl::PairReply { msg: vec![4; 33] },
            Ctrl::PairProof { proof: vec![5; 32] },
            Ctrl::Denied { reason: "not paired · ✓".into() },
            Ctrl::ServerHello { version: 1, width: 2560, height: 1440, fps: 120 },
            Ctrl::KeyframeRequest,
            Ctrl::Key { scan: 0x1d, extended: true, down: false },
            Ctrl::MouseButton { button: 4, down: true },
            Ctrl::Wheel { delta: -240, horizontal: true },
            Ctrl::Ping { client_us: u64::MAX - 1 },
            Ctrl::Pong { client_us: 7, server_us: 1 << 40 },
            Ctrl::Cursor { kind: cursor::CUSTOM, key: 0xdead_beef },
            Ctrl::CursorImage { key: 1, hot_x: 3, hot_y: 4, width: 2, height: 1, bgra: vec![1, 2, 3, 4, 5, 6, 7, 8] },
            Ctrl::CursorImage { key: 2, hot_x: 0, hot_y: 0, width: 96, height: 96, bgra: vec![9; 96 * 96 * 4] },
        ];
        let mut buf = Vec::new();
        for m in &all {
            m.encode(&mut buf);
        }
        let mut rest = &buf[..];
        for m in &all {
            let (t, len) = Ctrl::parse_header(rest[..HEADER_LEN].try_into().unwrap());
            let body = &rest[HEADER_LEN..HEADER_LEN + len];
            assert_eq!(&Ctrl::decode(t, body).unwrap(), m);
            rest = &rest[HEADER_LEN + len..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn short_body_is_an_error_not_a_panic() {
        assert!(Ctrl::decode(tag::PONG, &[0; 15]).is_err());
        assert!(Ctrl::decode(99, &[]).is_err());
        // image shorter than width*height*4, or bigger than the limit
        assert!(Ctrl::decode(tag::CURSOR_IMAGE, &[0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2, 0, 1]).is_err());
        assert!(Ctrl::decode(tag::CURSOR_IMAGE, &[0, 0, 0, 0, 0, 0, 0, 0, 200, 0, 1, 0]).is_err());
    }

    #[test]
    fn long_text_is_cut_at_a_character_boundary() {
        let mut buf = Vec::new();
        Ctrl::Denied { reason: "é".repeat(150) }.encode(&mut buf);
        let (t, len) = Ctrl::parse_header(buf[..HEADER_LEN].try_into().unwrap());
        assert!(len <= MAX_TEXT);
        let Ctrl::Denied { reason } = Ctrl::decode(t, &buf[HEADER_LEN..]).unwrap() else { panic!() };
        assert_eq!(reason, "é".repeat(100));
    }

    #[test]
    fn image_key_depends_on_pixels_and_hotspot() {
        let a = cursor::image_key(1, 1, 0, 0, &[1, 2, 3, 4]);
        assert_eq!(a, cursor::image_key(1, 1, 0, 0, &[1, 2, 3, 4]));
        assert_ne!(a, cursor::image_key(1, 1, 0, 0, &[1, 2, 3, 5]));
        assert_ne!(a, cursor::image_key(1, 1, 1, 0, &[1, 2, 3, 4]));
    }

    #[test]
    fn mouse_round_trips() {
        let m = MouseMove { seq: 123456, x: 65535, y: 1 };
        assert_eq!(MouseMove::decode(&m.encode()).unwrap(), m);
        assert!(MouseMove::decode(&[crate::DG_MOUSE_MOVE, 0]).is_err());
    }
}

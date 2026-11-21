//! Video over datagrams.
//!
//! One encoded frame is split into fragments, each sent as one QUIC datagram:
//!
//! ```text
//! u8  DG_VIDEO
//! u32 frame_id      // +1 per frame sent; a gap means a frame was lost or dropped
//! u16 frag_index
//! u16 frag_count
//! u8  flags         // bit 0: keyframe (IDR)
//! u64 capture_us    // server's clock when the screen was captured (see clock.rs)
//! [payload]
//! ```
//!
//! No FEC and no retransmission in v1: a frame with a missing fragment is dropped whole, the
//! client asks for a keyframe, and deltas are skipped until it arrives ([`KeyframeGate`]).

use anyhow::{ensure, Result};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const HEADER_LEN: usize = 18;
pub const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragHeader {
    pub frame_id: u32,
    pub frag_index: u16,
    pub frag_count: u16,
    pub flags: u8,
    pub capture_us: u64,
}

impl FragHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(crate::DG_VIDEO);
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.frag_index.to_le_bytes());
        out.extend_from_slice(&self.frag_count.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.capture_us.to_le_bytes());
    }

    pub fn parse(b: &[u8]) -> Result<(FragHeader, &[u8])> {
        ensure!(b.len() >= HEADER_LEN && b[0] == crate::DG_VIDEO, "bad video datagram");
        let h = FragHeader {
            frame_id: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            frag_index: u16::from_le_bytes([b[5], b[6]]),
            frag_count: u16::from_le_bytes([b[7], b[8]]),
            flags: b[9],
            capture_us: u64::from_le_bytes(b[10..18].try_into().unwrap()),
        };
        ensure!(h.frag_count > 0 && h.frag_index < h.frag_count, "bad fragment index");
        Ok((h, &b[HEADER_LEN..]))
    }
}

/// Number of datagrams a frame of `len` bytes needs at `max_datagram` bytes each.
pub fn fragment_count(len: usize, max_datagram: usize) -> usize {
    let payload = max_datagram - HEADER_LEN;
    len.div_ceil(payload).max(1)
}

/// Splits one encoded frame into datagrams of at most `max_datagram` bytes.
pub fn fragment(frame_id: u32, capture_us: u64, keyframe: bool, data: &[u8], max_datagram: usize) -> Result<Vec<Vec<u8>>> {
    ensure!(max_datagram > HEADER_LEN + 64, "datagram size {max_datagram} too small");
    let payload = max_datagram - HEADER_LEN;
    let count = fragment_count(data.len(), max_datagram);
    ensure!(count <= u16::MAX as usize, "frame of {} bytes too big", data.len());
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let chunk = &data[(i * payload).min(data.len())..((i + 1) * payload).min(data.len())];
        let mut d = Vec::with_capacity(HEADER_LEN + chunk.len());
        FragHeader { frame_id, frag_index: i as u16, frag_count: count as u16, flags, capture_us }.write(&mut d);
        d.extend_from_slice(chunk);
        out.push(d);
    }
    Ok(out)
}

/// A frame rebuilt from all its fragments.
#[derive(Debug)]
pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub data: Vec<u8>,
    /// When the first fragment arrived; `complete_at - first_at` is time spent on the wire
    /// spreading out one frame (big keyframes show up here).
    pub first_at: Instant,
    pub complete_at: Instant,
}

#[derive(Debug)]
pub enum Event {
    Frame(Frame),
    /// Frames `from..=to` will never be delivered.
    Lost { from: u32, to: u32 },
}

struct Partial {
    keyframe: bool,
    capture_us: u64,
    frags: Vec<Option<Vec<u8>>>,
    got: usize,
    first_at: Instant,
}

/// Rebuilds frames from fragments and reports losses. Frames are delivered in id order;
/// once a newer frame completes, every older unfinished frame is declared lost (it would be
/// stale anyway). An unfinished frame is also declared lost after `timeout`, so the last
/// frame before the screen goes still can't hang forever waiting for a successor.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    /// Highest frame id delivered or given up on.
    done_through: Option<u32>,
    timeout: Duration,
}

/// Bounds memory if fragments of many frames arrive without any completing.
const MAX_PARTIAL: usize = 64;

impl Reassembler {
    pub fn new(timeout: Duration) -> Self {
        Reassembler { partial: BTreeMap::new(), done_through: None, timeout }
    }

    pub fn push(&mut self, dgram: &[u8], now: Instant, out: &mut Vec<Event>) -> Result<()> {
        let (h, payload) = FragHeader::parse(dgram)?;
        if self.done_through.is_some_and(|d| h.frame_id <= d) {
            return Ok(()); // late fragment of a frame already finished or given up
        }
        let p = self.partial.entry(h.frame_id).or_insert_with(|| Partial {
            keyframe: h.flags & FLAG_KEYFRAME != 0,
            capture_us: h.capture_us,
            frags: vec![None; h.frag_count as usize],
            got: 0,
            first_at: now,
        });
        ensure!(p.frags.len() == h.frag_count as usize, "fragment count changed within frame {}", h.frame_id);
        let slot = &mut p.frags[h.frag_index as usize];
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            p.got += 1;
        }
        if p.got == p.frags.len() {
            let p = self.partial.remove(&h.frame_id).unwrap();
            if let Some(prev) = h.frame_id.checked_sub(1) {
                self.give_up_through(prev, out);
            }
            self.done_through = Some(h.frame_id);
            let mut data = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().map_or(0, Vec::len)).sum());
            for f in p.frags {
                data.extend_from_slice(&f.unwrap());
            }
            out.push(Event::Frame(Frame {
                id: h.frame_id,
                keyframe: p.keyframe,
                capture_us: p.capture_us,
                data,
                first_at: p.first_at,
                complete_at: now,
            }));
        } else if self.partial.len() > MAX_PARTIAL {
            let oldest = *self.partial.keys().next().unwrap();
            self.give_up_through(oldest, out);
        }
        Ok(())
    }

    /// Call regularly (e.g. every few ms): gives up on frames older than the timeout.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<Event>) {
        let newest_expired = self
            .partial
            .iter()
            .filter(|(_, p)| now.duration_since(p.first_at) > self.timeout)
            .map(|(&id, _)| id)
            .max();
        if let Some(id) = newest_expired {
            self.give_up_through(id, out);
        }
    }

    /// Declares every undelivered frame up to and including `id` lost.
    fn give_up_through(&mut self, id: u32, out: &mut Vec<Event>) {
        let from = match self.done_through {
            Some(d) if id <= d => return,
            Some(d) => d + 1,
            // Before the first delivered frame, only frames we saw fragments of count.
            None => match self.partial.keys().next() {
                Some(&first) if first <= id => first,
                _ => return,
            },
        };
        self.partial.retain(|&k, _| k > id);
        self.done_through = Some(id);
        out.push(Event::Lost { from, to: id });
    }
}

/// Decides which frames may go to the decoder and when to ask for a keyframe.
///
/// After any loss, H.264 deltas reference a picture the decoder never got, so they'd decode as
/// garbage: skip them until a keyframe arrives. Keep asking (every `interval`) until one does,
/// so a lost request or lost keyframe can't freeze the picture forever.
pub struct KeyframeGate {
    waiting: bool,
    last_request: Option<Instant>,
    interval: Duration,
}

impl KeyframeGate {
    pub fn new(interval: Duration) -> Self {
        // Waiting from the start: nothing decodes before the first keyframe.
        KeyframeGate { waiting: true, last_request: None, interval }
    }

    pub fn on_lost(&mut self) {
        self.waiting = true;
    }

    /// True if this frame should be decoded.
    pub fn admit(&mut self, keyframe: bool) -> bool {
        if keyframe {
            self.waiting = false;
            self.last_request = None;
        }
        !self.waiting
    }

    /// True if a keyframe request should be sent now.
    pub fn should_request(&mut self, now: Instant) -> bool {
        if self.waiting && self.last_request.is_none_or(|t| now.duration_since(t) >= self.interval) {
            self.last_request = Some(now);
            return true;
        }
        false
    }

    pub fn waiting(&self) -> bool {
        self.waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: usize = 1200;

    fn frame(id: u32, len: usize, key: bool) -> Vec<Vec<u8>> {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + id as usize) as u8).collect();
        fragment(id, 1000 + id as u64, key, &data, MTU).unwrap()
    }

    fn frames(ev: &[Event]) -> Vec<u32> {
        ev.iter().filter_map(|e| if let Event::Frame(f) = e { Some(f.id) } else { None }).collect()
    }

    fn lost(ev: &[Event]) -> Vec<(u32, u32)> {
        ev.iter().filter_map(|e| if let Event::Lost { from, to } = e { Some((*from, *to)) } else { None }).collect()
    }

    #[test]
    fn fragments_fit_and_rebuild_exactly() {
        for len in [0, 1, MTU - HEADER_LEN, MTU - HEADER_LEN + 1, 300_000] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let grams = fragment(5, 42, true, &data, MTU).unwrap();
            assert_eq!(grams.len(), fragment_count(len, MTU));
            assert!(grams.iter().all(|g| g.len() <= MTU));
            let mut r = Reassembler::new(Duration::from_millis(100));
            let mut ev = Vec::new();
            for g in grams.iter().rev() {
                r.push(g, Instant::now(), &mut ev).unwrap();
            }
            match &ev[..] {
                [Event::Frame(f)] => {
                    assert_eq!(f.data, data);
                    assert!(f.keyframe);
                    assert_eq!(f.capture_us, 42);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn newer_frame_completing_declares_older_lost() {
        let mut r = Reassembler::new(Duration::from_secs(10));
        let mut ev = Vec::new();
        let now = Instant::now();
        for g in frame(0, 100, true) {
            r.push(&g, now, &mut ev).unwrap();
        }
        let f1 = frame(1, 5000, false);
        r.push(&f1[0], now, &mut ev).unwrap(); // frame 1 incomplete
        // frame 2 never seen at all, frame 3 completes
        for g in frame(3, 100, false) {
            r.push(&g, now, &mut ev).unwrap();
        }
        assert_eq!(frames(&ev), vec![0, 3]);
        assert_eq!(lost(&ev), vec![(1, 2)]);
        // late fragment of frame 1 is ignored
        ev.clear();
        r.push(&f1[1], now, &mut ev).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn stuck_frame_expires_without_a_successor() {
        let mut r = Reassembler::new(Duration::from_millis(50));
        let mut ev = Vec::new();
        let t0 = Instant::now();
        for g in frame(0, 100, true) {
            r.push(&g, t0, &mut ev).unwrap();
        }
        r.push(&frame(1, 5000, false)[0], t0, &mut ev).unwrap();
        r.expire(t0 + Duration::from_millis(10), &mut ev);
        assert!(lost(&ev).is_empty());
        r.expire(t0 + Duration::from_millis(60), &mut ev);
        assert_eq!(lost(&ev), vec![(1, 1)]);
    }

    #[test]
    fn first_frames_lost_are_reported() {
        let mut r = Reassembler::new(Duration::from_secs(10));
        let mut ev = Vec::new();
        let now = Instant::now();
        r.push(&frame(7, 5000, true)[0], now, &mut ev).unwrap();
        for g in frame(8, 100, false) {
            r.push(&g, now, &mut ev).unwrap();
        }
        assert_eq!(lost(&ev), vec![(7, 7)]);
        assert_eq!(frames(&ev), vec![8]);
    }

    #[test]
    fn partial_count_is_bounded() {
        let mut r = Reassembler::new(Duration::from_secs(10));
        let mut ev = Vec::new();
        for id in 0..200 {
            r.push(&frame(id, 5000, false)[0], Instant::now(), &mut ev).unwrap();
        }
        assert!(r.partial.len() <= MAX_PARTIAL + 1);
    }

    #[test]
    fn malformed_datagrams_are_errors() {
        let mut r = Reassembler::new(Duration::from_secs(1));
        let mut ev = Vec::new();
        assert!(r.push(&[crate::DG_VIDEO; 5], Instant::now(), &mut ev).is_err());
        let mut g = frame(0, 3000, false)[0].clone();
        g[7] = 0; // frag_count = 0 (low byte) with index 0 -> count becomes small/invalid
        g[8] = 0;
        assert!(r.push(&g, Instant::now(), &mut ev).is_err());
    }

    #[test]
    fn gate_skips_deltas_and_repeats_requests_until_keyframe() {
        let mut g = KeyframeGate::new(Duration::from_millis(100));
        let t = Instant::now();
        assert!(!g.admit(false));
        assert!(g.should_request(t));
        assert!(!g.should_request(t + Duration::from_millis(50)));
        assert!(g.should_request(t + Duration::from_millis(100))); // lost request is retried
        assert!(g.admit(true));
        assert!(g.admit(false));
        assert!(!g.should_request(t + Duration::from_secs(1)));
        g.on_lost();
        assert!(!g.admit(false));
        assert!(g.should_request(t + Duration::from_secs(1)));
    }
}

//! Video over datagrams.
//!
//! One encoded frame is split into `data_count` equal-size shards plus `parity_count`
//! Reed-Solomon parity shards, each sent as one QUIC datagram:
//!
//! ```text
//! u8  DG_VIDEO
//! u32 frame_id      // +1 per frame sent; a gap means a frame was lost or dropped
//! u16 shard_index   // 0..data_count are data, data_count.. are parity
//! u16 data_count
//! u16 parity_count
//! u8  flags         // bit 0: keyframe (IDR), bit 1: recovery frame
//! u64 capture_us    // server's clock when the screen was captured (see clock.rs)
//! u32 frame_len     // bytes of real data (the last data shard is zero-padded)
//! u32 recovery_for  // recovery frames: the first lost frame they repair (else 0)
//! [shard]
//! ```
//!
//! FEC: the client rebuilds the frame from **any** `data_count` of the shards, so up to
//! `parity_count` lost datagrams per frame cost nothing. Data shards go first, so with no loss
//! the frame completes before its parity arrives (no added delay). A frame that loses more than
//! that is dropped whole and deltas are skipped until the picture is repaired
//! ([`KeyframeGate`]): preferably by a **recovery frame** (the client reports the lost frame,
//! the encoder invalidates it and everything sent after it, and encodes the next frame from an
//! older picture the client still has: about the size of a normal frame), else by a keyframe.
//! No retransmission.

use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const HEADER_LEN: usize = 28;
pub const FLAG_KEYFRAME: u8 = 1;
pub const FLAG_RECOVERY: u8 = 2;
/// reed-solomon-simd's limit on data shards per frame.
const MAX_DATA_SHARDS: usize = 32768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragHeader {
    pub frame_id: u32,
    pub shard_index: u16,
    pub data_count: u16,
    pub parity_count: u16,
    pub flags: u8,
    pub capture_us: u64,
    pub frame_len: u32,
    pub recovery_for: u32,
}

impl FragHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(crate::DG_VIDEO);
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.shard_index.to_le_bytes());
        out.extend_from_slice(&self.data_count.to_le_bytes());
        out.extend_from_slice(&self.parity_count.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.frame_len.to_le_bytes());
        out.extend_from_slice(&self.recovery_for.to_le_bytes());
    }

    pub fn parse(b: &[u8]) -> Result<(FragHeader, &[u8])> {
        ensure!(b.len() >= HEADER_LEN && b[0] == crate::DG_VIDEO, "bad video datagram");
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let h = FragHeader {
            frame_id: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            shard_index: u16_at(5),
            data_count: u16_at(7),
            parity_count: u16_at(9),
            flags: b[11],
            capture_us: u64::from_le_bytes(b[12..20].try_into().unwrap()),
            frame_len: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            recovery_for: u32::from_le_bytes(b[24..28].try_into().unwrap()),
        };
        let total = h.data_count as usize + h.parity_count as usize;
        ensure!(h.data_count > 0 && h.data_count as usize <= MAX_DATA_SHARDS && h.parity_count > 0, "bad shard counts");
        ensure!((h.shard_index as usize) < total, "bad shard index");
        let shard = &b[HEADER_LEN..];
        ensure!(!shard.is_empty() && shard.len() % 2 == 0, "bad shard size");
        ensure!(h.frame_len as usize <= h.data_count as usize * shard.len(), "frame length larger than its shards");
        Ok((h, shard))
    }
}

/// Parity shards for `data` data shards at `fec_percent`: at least 2 once a frame spans 2+
/// datagrams (internet loss tends to come in pairs), 1 for a single-datagram frame.
pub fn parity_count(data: usize, fec_percent: u32) -> usize {
    let floor = if data >= 2 { 2 } else { 1 };
    (data * fec_percent as usize).div_ceil(100).max(floor)
}

/// Largest even shard that fits in one datagram after the header.
fn shard_capacity(max_datagram: usize) -> usize {
    (max_datagram - HEADER_LEN) & !1
}

/// Data shards a frame of `len` bytes needs at `max_datagram` bytes per datagram.
fn data_count(len: usize, max_datagram: usize) -> usize {
    len.div_ceil(shard_capacity(max_datagram)).max(1)
}

/// Datagrams (data + parity) a frame of `len` bytes is sent as.
pub fn datagram_count(len: usize, max_datagram: usize, fec_percent: u32) -> usize {
    let k = data_count(len, max_datagram);
    k + parity_count(k, fec_percent)
}

/// Splits one encoded frame into data + parity datagrams of at most `max_datagram` bytes.
/// `recovery_for`: this frame repairs the loss that started at that frame id.
pub fn fragment(frame_id: u32, capture_us: u64, keyframe: bool, recovery_for: Option<u32>, data: &[u8], max_datagram: usize, fec_percent: u32) -> Result<Vec<Vec<u8>>> {
    ensure!(max_datagram > HEADER_LEN + 64, "datagram size {max_datagram} too small");
    let k = data_count(data.len(), max_datagram);
    let m = parity_count(k, fec_percent);
    ensure!(k <= MAX_DATA_SHARDS && k + m <= u16::MAX as usize, "frame of {} bytes too big", data.len());
    // Equal-size even shards (Reed-Solomon needs both); the last one is zero-padded.
    let shard_len = (data.len().div_ceil(k).max(1) + 1) & !1;
    let shards: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            let mut v = data[(i * shard_len).min(data.len())..((i + 1) * shard_len).min(data.len())].to_vec();
            v.resize(shard_len, 0);
            v
        })
        .collect();
    let parity = reed_solomon_simd::encode(k, m, &shards).context("FEC encode")?;
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 } | if recovery_for.is_some() { FLAG_RECOVERY } else { 0 };
    let mut out = Vec::with_capacity(k + m);
    for (i, shard) in shards.iter().chain(parity.iter()).enumerate() {
        let mut d = Vec::with_capacity(HEADER_LEN + shard_len);
        FragHeader { frame_id, shard_index: i as u16, data_count: k as u16, parity_count: m as u16, flags, capture_us, frame_len: data.len() as u32, recovery_for: recovery_for.unwrap_or(0) }
            .write(&mut d);
        d.extend_from_slice(shard);
        out.push(d);
    }
    Ok(out)
}

/// A frame rebuilt from its shards.
#[derive(Debug)]
pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    /// Recovery frame: repairs the loss that started at this frame id.
    pub recovery_for: Option<u32>,
    pub capture_us: u64,
    pub data: Vec<u8>,
    /// True if FEC rebuilt missing data shards (the frame would have been lost without it).
    pub recovered: bool,
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
    recovery_for: Option<u32>,
    capture_us: u64,
    frame_len: usize,
    data_count: usize,
    parity_count: usize,
    shard_len: usize,
    shards: Vec<Option<Vec<u8>>>,
    got: usize,
    data_got: usize,
    first_at: Instant,
}

impl Partial {
    /// Joins the data shards, rebuilding missing ones from parity first if needed.
    /// Returns the frame and whether FEC was needed.
    fn assemble(self) -> Result<(Vec<u8>, bool)> {
        let k = self.data_count;
        let recovered = self.data_got < k;
        let mut data_shards = self.shards;
        let parity = data_shards.split_off(k);
        if recovered {
            let present = data_shards.iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
            let recovery = parity.iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
            let restored = reed_solomon_simd::decode(k, self.parity_count, present, recovery).context("FEC decode")?;
            for (i, s) in restored {
                data_shards[i] = Some(s);
            }
        }
        let mut data = Vec::with_capacity(k * self.shard_len);
        for s in data_shards {
            data.extend_from_slice(&s.context("shard missing after FEC")?);
        }
        data.truncate(self.frame_len);
        Ok((data, recovered))
    }
}

/// Rebuilds frames from shards and reports losses. Frames are delivered in id order;
/// once a newer frame completes, every older unfinished frame is declared lost (it would be
/// stale anyway). An unfinished frame is also declared lost after `timeout`, so the last
/// frame before the screen goes still can't hang forever waiting for a successor.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    /// Highest frame id delivered or given up on.
    done_through: Option<u32>,
    timeout: Duration,
}

/// Bounds memory if shards of many frames arrive without any completing.
const MAX_PARTIAL: usize = 64;

impl Reassembler {
    pub fn new(timeout: Duration) -> Self {
        Reassembler { partial: BTreeMap::new(), done_through: None, timeout }
    }

    pub fn push(&mut self, dgram: &[u8], now: Instant, out: &mut Vec<Event>) -> Result<()> {
        let (h, shard) = FragHeader::parse(dgram)?;
        if self.done_through.is_some_and(|d| h.frame_id <= d) {
            return Ok(()); // late shard of a frame already finished or given up (often parity)
        }
        let (k, m) = (h.data_count as usize, h.parity_count as usize);
        let p = self.partial.entry(h.frame_id).or_insert_with(|| Partial {
            keyframe: h.flags & FLAG_KEYFRAME != 0,
            recovery_for: (h.flags & FLAG_RECOVERY != 0).then_some(h.recovery_for),
            capture_us: h.capture_us,
            frame_len: h.frame_len as usize,
            data_count: k,
            parity_count: m,
            shard_len: shard.len(),
            shards: vec![None; k + m],
            got: 0,
            data_got: 0,
            first_at: now,
        });
        ensure!(
            p.data_count == k && p.parity_count == m && p.shard_len == shard.len() && p.frame_len == h.frame_len as usize,
            "shard layout changed within frame {}",
            h.frame_id
        );
        let i = h.shard_index as usize;
        if p.shards[i].is_none() {
            p.shards[i] = Some(shard.to_vec());
            p.got += 1;
            if i < k {
                p.data_got += 1;
            }
        }
        if p.got >= p.data_count {
            let p = self.partial.remove(&h.frame_id).unwrap();
            if let Some(prev) = h.frame_id.checked_sub(1) {
                self.give_up_through(prev, out);
            }
            let (keyframe, recovery_for, capture_us, first_at) = (p.keyframe, p.recovery_for, p.capture_us, p.first_at);
            match p.assemble() {
                Ok((data, recovered)) => {
                    self.done_through = Some(h.frame_id);
                    out.push(Event::Frame(Frame { id: h.frame_id, keyframe, recovery_for, capture_us, data, recovered, first_at, complete_at: now }));
                }
                Err(_) => self.give_up_through(h.frame_id, out),
            }
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

/// Decides which frames may go to the decoder, and what to ask the server for after a loss.
///
/// After a loss, H.264 deltas reference a picture the decoder never got, so they'd decode as
/// garbage: skip them until the picture is repaired, by
/// - a **recovery frame** for that loss: the server invalidated the lost frame and everything
///   after it, so this frame only references pictures from before the loss, which the decoder
///   has (asked for with [`Request::Invalidate`]);
/// - or a **keyframe** (at the start, after a decoder error, or when recovery doesn't arrive
///   after [`INVALIDATE_TRIES`] requests).
///
/// Keeps asking (every `retry`, see [`keyframe_retry`]) until repaired, so a lost recovery
/// frame or keyframe can't freeze the picture.
pub struct KeyframeGate {
    waiting: bool,
    /// First frame the decoder didn't get in the current outage.
    first_lost: Option<u32>,
    /// Only a keyframe can repair the picture now.
    need_keyframe: bool,
    requests: u32,
    last_request: Option<Instant>,
}

/// Invalidation requests sent for one outage before asking for a keyframe instead.
pub const INVALIDATE_TRIES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Keyframe,
    /// Invalidate frame `from` and everything sent after it, then send a recovery frame.
    Invalidate { from: u32 },
}

/// How long to wait for a requested keyframe before asking again: the request and the keyframe
/// each take about half a round trip, plus the keyframe's own time on the wire and jitter.
/// Asking sooner only makes the server send extra keyframes for the same loss.
pub fn keyframe_retry(rtt: Duration) -> Duration {
    (rtt * 2 + Duration::from_millis(50)).clamp(Duration::from_millis(150), Duration::from_secs(1))
}

impl Default for KeyframeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeGate {
    pub fn new() -> Self {
        // Waiting from the start: nothing decodes before the first keyframe.
        KeyframeGate { waiting: true, first_lost: None, need_keyframe: true, requests: 0, last_request: None }
    }

    /// Frames from `from` on were lost.
    pub fn on_lost(&mut self, from: u32) {
        if !self.waiting {
            self.waiting = true;
            self.first_lost = Some(from);
            self.requests = 0;
            self.last_request = None;
        }
    }

    /// The decoder failed: only a keyframe helps.
    pub fn on_decode_error(&mut self) {
        self.waiting = true;
        self.need_keyframe = true;
    }

    /// True if this frame should be decoded.
    pub fn admit(&mut self, keyframe: bool, recovery_for: Option<u32>) -> bool {
        let repairs = match (recovery_for, self.first_lost) {
            // It references only frames from before `r`; the decoder has all frames before
            // `first_lost`, so any r <= first_lost is safe.
            (Some(r), Some(lost)) => !self.need_keyframe && r.wrapping_sub(lost) as i32 <= 0,
            _ => false,
        };
        if keyframe || (self.waiting && repairs) {
            self.waiting = false;
            self.need_keyframe = false;
            self.first_lost = None;
            self.requests = 0;
            self.last_request = None;
        }
        !self.waiting
    }

    /// What to ask the server for now, if anything.
    pub fn should_request(&mut self, now: Instant, retry: Duration) -> Option<Request> {
        if !self.waiting || self.last_request.is_some_and(|t| now.duration_since(t) < retry) {
            return None;
        }
        self.last_request = Some(now);
        self.requests += 1;
        match self.first_lost {
            Some(from) if !self.need_keyframe && self.requests <= INVALIDATE_TRIES => Some(Request::Invalidate { from }),
            _ => {
                self.need_keyframe = true;
                Some(Request::Keyframe)
            }
        }
    }

    pub fn waiting(&self) -> bool {
        self.waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: usize = 1200;
    const FEC: u32 = 15;

    fn frame(id: u32, len: usize, key: bool) -> Vec<Vec<u8>> {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + id as usize) as u8).collect();
        fragment(id, 1000 + id as u64, key, None, &data, MTU, FEC).unwrap()
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
            let grams = fragment(5, 42, true, None, &data, MTU, FEC).unwrap();
            assert_eq!(grams.len(), datagram_count(len, MTU, FEC));
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

    fn deliver(grams: &[Vec<u8>], skip: &[usize]) -> Vec<Event> {
        let mut r = Reassembler::new(Duration::from_secs(10));
        let mut ev = Vec::new();
        for (i, g) in grams.iter().enumerate() {
            if !skip.contains(&i) {
                r.push(g, Instant::now(), &mut ev).unwrap();
            }
        }
        ev
    }

    #[test]
    fn parity_count_rules() {
        assert_eq!(parity_count(1, 15), 1); // single-datagram frame
        assert_eq!(parity_count(2, 15), 2); // floor of 2
        assert_eq!(parity_count(10, 15), 2);
        assert_eq!(parity_count(20, 15), 3);
        assert_eq!(parity_count(100, 40), 40);
    }

    #[test]
    fn fec_rebuilds_up_to_parity_count_lost_datagrams_anywhere() {
        let data: Vec<u8> = (0..30_001).map(|i| (i * 13) as u8).collect(); // odd length: padding
        let grams = fragment(0, 1, false, None, &data, MTU, FEC).unwrap();
        let k = data.len().div_ceil(shard_capacity(MTU));
        let m = grams.len() - k;
        assert_eq!(m, parity_count(k, FEC));
        // first m, last m, spread out, a data+parity mix
        for skip in [(0..m).collect::<Vec<_>>(), (k + m - m..k + m).collect(), (0..m).map(|i| i * 5).collect(), vec![3, k]] {
            match &deliver(&grams, &skip)[..] {
                [Event::Frame(f)] => {
                    assert_eq!(f.data, data, "skip {skip:?}");
                    assert_eq!(f.recovered, skip.iter().any(|&i| i < k), "skip {skip:?}");
                }
                other => panic!("skip {skip:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn too_many_lost_datagrams_lose_the_frame() {
        let data = vec![7u8; 30_000];
        let grams = fragment(0, 1, false, None, &data, MTU, FEC).unwrap();
        let k = data.len().div_ceil(shard_capacity(MTU));
        let m = grams.len() - k;
        let skip: Vec<usize> = (0..=m).collect(); // one more than parity can cover
        assert!(frames(&deliver(&grams, &skip)).is_empty());
    }

    #[test]
    fn clean_frame_completes_before_its_parity() {
        let data = vec![1u8; 10_000];
        let grams = fragment(0, 1, false, None, &data, MTU, FEC).unwrap();
        let k = data.len().div_ceil(shard_capacity(MTU));
        let mut r = Reassembler::new(Duration::from_secs(1));
        let mut ev = Vec::new();
        for g in &grams[..k] {
            r.push(g, Instant::now(), &mut ev).unwrap();
        }
        assert!(matches!(&ev[..], [Event::Frame(f)] if !f.recovered && f.data == data));
        for g in &grams[k..] {
            r.push(g, Instant::now(), &mut ev).unwrap(); // late parity: ignored
        }
        assert_eq!(ev.len(), 1);
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
        g[7] = 0; // data_count = 0
        g[8] = 0;
        assert!(r.push(&g, Instant::now(), &mut ev).is_err());
    }

    #[test]
    fn gate_skips_deltas_and_repeats_requests_until_keyframe() {
        let mut g = KeyframeGate::new();
        let r = Duration::from_millis(100);
        let t = Instant::now();
        assert!(!g.admit(false, None));
        assert_eq!(g.should_request(t, r), Some(Request::Keyframe), "nothing decoded yet: keyframe");
        assert_eq!(g.should_request(t + Duration::from_millis(50), r), None);
        assert_eq!(g.should_request(t + Duration::from_millis(100), r), Some(Request::Keyframe)); // retried
        assert!(g.admit(true, None));
        assert!(g.admit(false, None));
        assert_eq!(g.should_request(t + Duration::from_secs(1), r), None);
        g.on_decode_error();
        assert!(!g.admit(false, None));
        assert_eq!(g.should_request(t + Duration::from_secs(1), r), Some(Request::Keyframe));
    }

    #[test]
    fn gate_prefers_a_recovery_frame_after_a_loss() {
        let mut g = KeyframeGate::new();
        let r = Duration::from_millis(100);
        let t = Instant::now();
        assert!(g.admit(true, None));
        g.on_lost(10);
        g.on_lost(12); // a second loss in the same outage keeps the first lost frame
        assert!(!g.admit(false, None), "deltas after the loss are skipped");
        assert_eq!(g.should_request(t, r), Some(Request::Invalidate { from: 10 }));
        assert!(!g.admit(false, Some(11)), "a recovery for a later loss doesn't repair frame 10");
        assert!(g.admit(false, Some(10)), "recovery for the loss opens the gate");
        assert!(g.admit(false, None));
        // An older recovery (references even earlier pictures) is fine too.
        g.on_lost(20);
        assert!(g.admit(false, Some(15)));
    }

    #[test]
    fn gate_falls_back_to_a_keyframe() {
        let mut g = KeyframeGate::new();
        let r = Duration::from_millis(100);
        let t = Instant::now();
        assert!(g.admit(true, None));
        g.on_lost(5);
        for i in 0..INVALIDATE_TRIES {
            assert_eq!(g.should_request(t + r * i, r), Some(Request::Invalidate { from: 5 }));
        }
        assert_eq!(g.should_request(t + r * INVALIDATE_TRIES, r), Some(Request::Keyframe));
        assert!(!g.admit(false, Some(5)), "after asking for a keyframe, only a keyframe repairs");
        assert!(g.admit(true, None));
    }

    #[test]
    fn recovery_flag_and_id_travel() {
        let mut rs = Reassembler::new(Duration::from_millis(80));
        let mut ev = Vec::new();
        for g in fragment(3, 9, false, Some(2), &[7u8; 3000], MTU, FEC).unwrap() {
            rs.push(&g, Instant::now(), &mut ev).unwrap();
        }
        let Some(Event::Frame(f)) = ev.into_iter().last() else { panic!() };
        assert_eq!((f.id, f.keyframe, f.recovery_for), (3, false, Some(2)));
    }

    #[test]
    fn keyframe_retry_follows_rtt() {
        let ms = Duration::from_millis;
        assert_eq!(keyframe_retry(ms(2)), ms(150)); // LAN: floor
        assert_eq!(keyframe_retry(ms(90)), ms(230)); // relayed Tailscale
        assert_eq!(keyframe_retry(ms(800)), ms(1000)); // cap
    }
}

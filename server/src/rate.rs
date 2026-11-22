//! Adaptive bitrate: once a second, pick the encoder bitrate from what the connection shows.
//!
//! Signals (all measured on the server, no client report needed):
//! - RTT against the best RTT of the last 10 s: a growing queue somewhere on the path shows up
//!   as extra delay *before* packets start dropping, so this backs off early.
//! - packet loss (quinn's lost / sent packets in that second).
//! - "net wait": the fraction of the second the video loop spent waiting for quinn's send
//!   queue to drain (the encoder makes more than the path carries).
//!
//! Rules (first match wins), adapted from Annex's controller:
//!
//! | condition                                              | action                  |
//! |--------------------------------------------------------|-------------------------|
//! | RTT > 3×best + 100 ms, or net wait > 50 %              | ×0.5                    |
//! | loss > 5 %, loss two seconds running, RTT > 2×best +   | ×0.8                    |
//! | 15 ms, or net wait > 10 %                              |                         |
//! | one stray lost packet                                  | hold (burst, not a cap) |
//! | RTT > 1.5×best + 8 ms                                  | ×0.9                    |
//! | clean                                                  | climb (see below)       |
//!
//! After a cut: wait [`HOLD`] clean seconds, then climb +10 % per second, but only up to 90 %
//! of the rate where trouble happened; above that, probe slowly (+1 % of max per second). The
//! trouble rate is forgotten after [`FORGET`] clean seconds, in case the path got better.
//!
//! FEC (Reed-Solomon parity, see proto/video.rs) follows loss: +5 points for each second with
//! any lost packet, -1 per clean second, between 10 and 40 %. The rate above is the total on
//! the wire; the encoder gets [`RateControl::video_bps`] = rate x 100 / (100 + FEC %).

use std::collections::VecDeque;
use std::time::Duration;

/// Never go below this (a 1080p desktop is still readable at 1 Mbit/s).
pub const MIN_BPS: u32 = 1_000_000;
/// Seconds of RTT history for the "best RTT" baseline.
const RTT_WINDOW: usize = 10;
/// Clean seconds to wait after a cut before climbing.
const HOLD: u32 = 3;
/// Clean seconds after which the rate where trouble happened stops capping the climb.
const FORGET: u32 = 30;
/// FEC parity as a percent of data shards: start, floor, ceiling, steps.
const FEC_START: u32 = 15;
const FEC_MIN: u32 = 10;
const FEC_MAX: u32 = 40;
const FEC_UP: u32 = 5;
const FEC_DOWN: u32 = 1;

/// One second of measurements.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    /// Fraction (0..=1) of the second spent waiting for the send queue.
    pub net_wait: f64,
}

pub struct RateControl {
    bps: u32,
    max: u32,
    rtts: VecDeque<Duration>,
    lossy_prev: bool,
    hold: u32,
    trouble_at: Option<u32>,
    clean: u32,
    reason: &'static str,
    fec: u32,
}

impl RateControl {
    /// Starts at `max_bps` (the configured bitrate is the ceiling).
    pub fn new(max_bps: u32) -> Self {
        let max = max_bps.max(MIN_BPS);
        RateControl { bps: max, max, rtts: VecDeque::new(), lossy_prev: false, hold: 0, trouble_at: None, clean: 0, reason: "start", fec: FEC_START }
    }

    /// Total bitrate on the wire (video + FEC parity).
    pub fn bps(&self) -> u32 {
        self.bps
    }

    /// FEC parity percent for [`proto::video::fragment`].
    pub fn fec_percent(&self) -> u32 {
        self.fec
    }

    /// Bitrate for the encoder: the wire rate minus room for FEC parity.
    pub fn video_bps(&self) -> u32 {
        (self.bps as u64 * 100 / (100 + self.fec) as u64) as u32
    }

    /// Why the rate is what it is, for stats: start, queue, loss, net wait, rtt, burst, hold,
    /// climb, probe, max.
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// Feed one second of measurements; returns the new bitrate.
    pub fn update(&mut self, r: &Report) -> u32 {
        self.rtts.push_back(r.rtt);
        if self.rtts.len() > RTT_WINDOW {
            self.rtts.pop_front();
        }
        let best = self.rtts.iter().min().copied().unwrap_or(r.rtt);
        let ms = Duration::from_millis;
        let loss = if r.sent_packets > 0 { r.lost_packets as f64 / r.sent_packets as f64 } else { 0.0 };
        let lossy = r.lost_packets > 0;

        if r.rtt > best * 3 + ms(100) || r.net_wait > 0.5 {
            self.cut(0.5, if r.net_wait > 0.5 { "net wait" } else { "queue" });
        } else if loss > 0.05 || (lossy && self.lossy_prev) {
            self.cut(0.8, "loss");
        } else if r.net_wait > 0.1 {
            self.cut(0.8, "net wait");
        } else if r.rtt > best * 2 + ms(15) {
            self.cut(0.8, "rtt");
        } else if lossy {
            self.reason = "burst"; // one stray loss: keep the rate, don't count as clean
        } else if r.rtt.as_secs_f64() > best.as_secs_f64() * 1.5 + 0.008 {
            self.cut(0.9, "rtt");
        } else {
            self.climb();
        }
        self.lossy_prev = lossy;
        self.fec = if lossy { (self.fec + FEC_UP).min(FEC_MAX) } else { self.fec.saturating_sub(FEC_DOWN).max(FEC_MIN) };
        self.bps
    }

    fn cut(&mut self, factor: f64, reason: &'static str) {
        self.trouble_at = Some(self.bps);
        self.bps = ((self.bps as f64 * factor) as u32).max(MIN_BPS);
        self.hold = HOLD;
        self.clean = 0;
        self.reason = reason;
    }

    fn climb(&mut self) {
        self.clean += 1;
        if self.clean >= FORGET {
            self.trouble_at = None;
        }
        if self.hold > 0 {
            self.hold -= 1;
            self.reason = "hold";
            return;
        }
        let ceiling = self.trouble_at.map_or(self.max, |t| (t as f64 * 0.9) as u32).min(self.max);
        if self.bps < ceiling {
            self.bps = ((self.bps as f64 * 1.1) as u32).min(ceiling);
            self.reason = "climb";
        } else if self.bps < self.max {
            self.bps = (self.bps + self.max / 100).min(self.max);
            self.reason = "probe";
        } else {
            self.reason = "max";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u32 = 1_000_000;

    fn rep(rtt_ms: u64, sent: u64, lost: u64, wait: f64) -> Report {
        Report { rtt: Duration::from_millis(rtt_ms), sent_packets: sent, lost_packets: lost, net_wait: wait }
    }

    fn clean(rtt_ms: u64) -> Report {
        rep(rtt_ms, 1000, 0, 0.0)
    }

    #[test]
    fn stays_at_max_on_a_clean_path() {
        let mut r = RateControl::new(20 * M);
        for _ in 0..20 {
            assert_eq!(r.update(&clean(90)), 20 * M);
        }
        assert_eq!(r.reason(), "max");
    }

    #[test]
    fn halves_when_a_queue_builds() {
        let mut r = RateControl::new(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&clean(400)), 10 * M); // 400 > 3*90 + 100
        assert_eq!(r.reason(), "queue");
        assert_eq!(r.update(&rep(90, 1000, 0, 0.7)), 5 * M);
        assert_eq!(r.reason(), "net wait");
    }

    #[test]
    fn one_stray_loss_holds_two_in_a_row_cut() {
        let mut r = RateControl::new(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&rep(90, 1000, 1, 0.0)), 20 * M);
        assert_eq!(r.reason(), "burst");
        assert_eq!(r.update(&rep(90, 1000, 1, 0.0)), 16 * M);
        assert_eq!(r.reason(), "loss");
    }

    #[test]
    fn heavy_loss_cuts_at_once() {
        let mut r = RateControl::new(20 * M);
        assert_eq!(r.update(&rep(90, 1000, 80, 0.0)), 16 * M);
    }

    #[test]
    fn creeping_rtt_backs_off_gently() {
        let mut r = RateControl::new(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&clean(150)), 18 * M); // > 1.5*90+8, < 2*90+15
        assert_eq!(r.reason(), "rtt");
    }

    #[test]
    fn after_a_cut_holds_then_climbs_to_90_percent_of_trouble() {
        let mut r = RateControl::new(20 * M);
        r.update(&clean(90));
        r.update(&rep(90, 1000, 100, 0.0)); // 20 -> 16, trouble at 20
        for _ in 0..HOLD {
            assert_eq!(r.update(&clean(90)), 16 * M);
            assert_eq!(r.reason(), "hold");
        }
        assert_eq!(r.update(&clean(90)), 17_600_000);
        assert_eq!(r.update(&clean(90)), 18 * M); // capped at 0.9 * 20
        assert_eq!(r.update(&clean(90)), 18_200_000); // slow probe above it
        assert_eq!(r.reason(), "probe");
    }

    #[test]
    fn trouble_rate_is_forgotten_after_clean_period() {
        let mut r = RateControl::new(20 * M);
        r.update(&clean(90));
        r.update(&rep(90, 1000, 100, 0.0));
        for _ in 0..FORGET {
            r.update(&clean(90));
        }
        assert!(r.trouble_at.is_none());
    }

    #[test]
    fn never_below_minimum() {
        let mut r = RateControl::new(20 * M);
        for _ in 0..30 {
            r.update(&rep(90, 1000, 0, 0.9));
        }
        assert_eq!(r.bps(), MIN_BPS);
    }

    #[test]
    fn fec_follows_loss_within_bounds() {
        let mut r = RateControl::new(20 * M);
        assert_eq!(r.fec_percent(), 15);
        assert_eq!(r.video_bps(), 20 * M * 100 / 115);
        r.update(&rep(90, 1000, 1, 0.0));
        assert_eq!(r.fec_percent(), 20);
        for _ in 0..10 {
            r.update(&rep(90, 1000, 1, 0.0));
        }
        assert_eq!(r.fec_percent(), 40);
        for _ in 0..100 {
            r.update(&clean(90));
        }
        assert_eq!(r.fec_percent(), 10);
    }

    #[test]
    fn lan_jitter_is_not_congestion() {
        let mut r = RateControl::new(20 * M);
        for rtt in [1, 2, 1, 3, 2, 1, 4, 2] {
            assert_eq!(r.update(&clean(rtt)), 20 * M); // 4 ms < 1.5*1 + 8
        }
    }
}

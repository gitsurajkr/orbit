//! Capture + encode on one thread, one frame at a time:
//!
//! ```text
//! AcquireNextFrame ─► CopyResource into our texture ─► ReleaseFrame ─► NVENC encode (sync)
//! ```
//!
//! No queue anywhere: the next capture starts only after the previous frame's bits are out.
//! If the screen changes faster than we encode, Desktop Duplication merges the changes and we
//! simply get the newest image next time.
//!
//! Where Windows refuses Desktop Duplication (Remote Desktop session, screen on another GPU,
//! basic display driver) the pipeline falls back to GDI capture (`COMPAT CAPTURE`).

use crate::capture::{find_adapter, in_remote_desktop, is_duplication_unsupported, Bridge, Duplicator, GdiCapture, Gpu, Grab, MonitorInfo, NVIDIA_VENDOR};
use crate::nvenc::{Encoder, EncoderConfig};
use anyhow::{Context, Result};
use proto::clock::now_us;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

enum Source {
    /// Desktop Duplication on the encoder's GPU (zero-copy).
    Dup(Duplicator),
    /// Desktop Duplication on another GPU (hybrid laptop), copied over through system memory.
    BridgedDup(Duplicator, Bridge),
    /// GDI fallback.
    Gdi(GdiCapture),
}

pub struct Pipeline {
    /// The NVIDIA GPU that encodes.
    gpu: Gpu,
    /// NVENC's input texture, on `gpu`.
    tex: ID3D11Texture2D,
    source: Source,
    enc: Encoder,
    mon: MonitorInfo,
    /// True once `tex` holds a real image (so a keyframe request can re-encode it).
    have_image: bool,
}

pub struct Encoded {
    pub keyframe: bool,
    pub capture_us: u64,
    /// Cross-GPU or GDI copy time (0 for zero-copy duplication).
    pub bridge_ms: f64,
    pub encode_ms: f64,
}

impl Pipeline {
    pub fn new(mon: MonitorInfo, fps: u32, bitrate_bps: u32) -> Result<Pipeline> {
        let (w, h) = (mon.width, mon.height);
        let gpu = if mon.vendor == NVIDIA_VENDOR {
            Gpu::on_adapter(mon.adapter_index)?
        } else {
            let (nv, _) = find_adapter(NVIDIA_VENDOR)?.context("no NVIDIA GPU found: NVENC needs one")?;
            Gpu::on_adapter(nv)?
        };
        let tex = gpu.screen_texture(w, h)?;
        let enc = Encoder::new(&gpu.device, &tex, EncoderConfig { width: w, height: h, fps, bitrate_bps })?;

        let force_gdi = std::env::var("ORBIT_CAPTURE").is_ok_and(|v| v.eq_ignore_ascii_case("gdi"));
        let dup = if force_gdi {
            None
        } else if mon.vendor == NVIDIA_VENDOR {
            Some(Duplicator::new(&gpu, mon.clone()).map(Source::Dup))
        } else {
            eprintln!(
                "note: {} is driven by '{}'; encoding on the NVIDIA GPU with a cross-GPU copy (adds a few ms)",
                mon.device_name, mon.adapter_name
            );
            Some((|| {
                let bridge = Bridge::new(Gpu::on_adapter(mon.adapter_index)?, w, h)?;
                let d = Duplicator::new(&bridge.src_gpu, mon.clone())?;
                Ok(Source::BridgedDup(d, bridge))
            })())
        };
        let source = match dup {
            Some(Ok(s)) => s,
            Some(Err(e)) if !is_duplication_unsupported(&e) => return Err(e),
            _ => {
                eprintln!(
                    "note: Desktop Duplication isn't available here{}; using GDI capture (slower)",
                    if in_remote_desktop() { " (Remote Desktop session)" } else { "" }
                );
                Source::Gdi(GdiCapture::new(mon.clone())?)
            }
        };
        Ok(Pipeline { gpu, tex, source, enc, mon, have_image: false })
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.mon
    }

    pub fn encoder(&mut self) -> &mut Encoder {
        &mut self.enc
    }

    /// Reference frames the encoder keeps.
    pub fn encoder_max_refs(&self) -> u32 {
        self.enc.max_refs()
    }

    /// The encoder can repair a loss without a keyframe.
    pub fn encoder_rfi(&self) -> bool {
        self.enc.supports_rfi()
    }

    /// True when frames take an extra copy (cross-GPU or GDI).
    pub fn bridged(&self) -> bool {
        !matches!(self.source, Source::Dup(_))
    }

    /// "dxgi", "dxgi+bridge" or "gdi", for stats.
    pub fn capture_mode(&self) -> &'static str {
        match self.source {
            Source::Dup(_) => "dxgi",
            Source::BridgedDup(..) => "dxgi+bridge",
            Source::Gdi(_) => "gdi",
        }
    }

    /// Waits up to `timeout_ms` for the screen to change, then encodes it into `out`.
    /// With `force_idr` (keyframe) or `force_frame` (recovery frame) and no change, re-encodes
    /// the last image right away, so a repair never waits for the screen to change.
    pub fn next(&mut self, timeout_ms: u32, force_idr: bool, force_frame: bool, out: &mut Vec<u8>) -> Result<Option<Encoded>> {
        let force_idr_or_frame = force_idr || force_frame;
        let wait = if force_idr_or_frame && self.have_image { 0 } else { timeout_ms };
        let mut copy_ms = 0.0;
        let mut capture_us = 0;
        let grabbed = match &mut self.source {
            Source::Dup(d) => matches!(d.grab(&self.gpu, wait, &self.tex)?, Grab::Frame),
            Source::BridgedDup(d, b) => {
                let g = matches!(d.grab(&b.src_gpu, wait, &b.src_tex)?, Grab::Frame);
                if g {
                    let t = Instant::now();
                    b.transfer(&self.gpu, &self.tex)?;
                    copy_ms = t.elapsed().as_secs_f64() * 1e3;
                }
                g
            }
            Source::Gdi(g) => {
                // No change notification: capture every call (the caller paces to the fps cap).
                // Timestamp before the copy, so latency stats include it.
                capture_us = now_us();
                let t = Instant::now();
                g.grab(&self.gpu, &self.tex)?;
                copy_ms = t.elapsed().as_secs_f64() * 1e3;
                true
            }
        };
        if capture_us == 0 {
            capture_us = now_us();
        }
        if grabbed {
            self.have_image = true;
        } else if !(force_idr_or_frame && self.have_image) {
            return Ok(None);
        }
        let t1 = Instant::now();
        let e = self.enc.encode(force_idr, capture_us, out)?;
        let t2 = Instant::now();
        Ok(Some(Encoded { keyframe: e.keyframe, capture_us, bridge_ms: copy_ms, encode_ms: (t2 - t1).as_secs_f64() * 1e3 }))
    }
}

//! Capture + encode on one thread, one frame at a time:
//!
//! ```text
//! AcquireNextFrame ─► CopyResource into our texture ─► ReleaseFrame ─► NVENC encode (sync)
//! ```
//!
//! No queue anywhere: the next capture starts only after the previous frame's bits are out.
//! If the screen changes faster than we encode, Desktop Duplication merges the changes and we
//! simply get the newest image next time.

use crate::capture::{find_adapter, Bridge, Duplicator, Gpu, Grab, MonitorInfo, NVIDIA_VENDOR};
use crate::nvenc::{Encoder, EncoderConfig};
use anyhow::{Context, Result};
use proto::clock::now_us;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub struct Pipeline {
    /// The NVIDIA GPU that encodes.
    gpu: Gpu,
    /// NVENC's input texture, on `gpu`.
    tex: ID3D11Texture2D,
    /// Set when the screen is on another GPU (hybrid laptop): capture there, then copy over.
    bridge: Option<Bridge>,
    dup: Duplicator,
    enc: Encoder,
    /// True once `tex` holds a real image (so a keyframe request can re-encode it).
    have_image: bool,
}

pub struct Encoded {
    pub keyframe: bool,
    pub capture_us: u64,
    /// Cross-GPU copy time (0 on a single-GPU system).
    pub bridge_ms: f64,
    pub encode_ms: f64,
}

impl Pipeline {
    pub fn new(mon: MonitorInfo, fps: u32, bitrate_bps: u32) -> Result<Pipeline> {
        let (w, h) = (mon.width, mon.height);
        let (gpu, bridge) = if mon.vendor == NVIDIA_VENDOR {
            (Gpu::on_adapter(mon.adapter_index)?, None)
        } else {
            let (nv, name) = find_adapter(NVIDIA_VENDOR)?.context("no NVIDIA GPU found: NVENC needs one")?;
            eprintln!(
                "note: {} is driven by '{}'; encoding on '{name}' with a cross-GPU copy (adds a few ms)",
                mon.device_name, mon.adapter_name
            );
            (Gpu::on_adapter(nv)?, Some(Bridge::new(Gpu::on_adapter(mon.adapter_index)?, w, h)?))
        };
        let tex = gpu.screen_texture(w, h)?;
        let enc = Encoder::new(&gpu.device, &tex, EncoderConfig { width: w, height: h, fps, bitrate_bps })?;
        let dup = Duplicator::new(bridge.as_ref().map_or(&gpu, |b| &b.src_gpu), mon)?;
        Ok(Pipeline { gpu, tex, bridge, dup, enc, have_image: false })
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.dup.mon
    }

    pub fn encoder(&mut self) -> &mut Encoder {
        &mut self.enc
    }

    pub fn bridged(&self) -> bool {
        self.bridge.is_some()
    }

    /// Waits up to `timeout_ms` for the screen to change, then encodes it into `out`.
    /// With `force_idr` and no change, re-encodes the last image as a keyframe right away, so a
    /// keyframe request never waits for the screen to change.
    pub fn next(&mut self, timeout_ms: u32, force_idr: bool, out: &mut Vec<u8>) -> Result<Option<Encoded>> {
        let wait = if force_idr && self.have_image { 0 } else { timeout_ms };
        let (cap_gpu, cap_tex) = match &self.bridge {
            Some(b) => (&b.src_gpu, &b.src_tex),
            None => (&self.gpu, &self.tex),
        };
        let grabbed = matches!(self.dup.grab(cap_gpu, wait, cap_tex)?, Grab::Frame);
        let capture_us = now_us();
        let t0 = Instant::now();
        if grabbed {
            if let Some(b) = &self.bridge {
                b.transfer(&self.gpu, &self.tex)?;
            }
            self.have_image = true;
        } else if !(force_idr && self.have_image) {
            return Ok(None);
        }
        let t1 = Instant::now();
        let e = self.enc.encode(force_idr, capture_us, out)?;
        let t2 = Instant::now();
        Ok(Some(Encoded {
            keyframe: e.keyframe,
            capture_us,
            bridge_ms: (t1 - t0).as_secs_f64() * 1e3,
            encode_ms: (t2 - t1).as_secs_f64() * 1e3,
        }))
    }
}

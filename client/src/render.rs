//! Shows decoded frames with as little delay as Windows allows.
//!
//! - The GPU's video processor converts NV12 → RGB and scales into the window in one pass,
//!   reading the decoder's texture directly (no copies).
//! - Flip-model swap chain, max frame latency 1, and tearing allowed: `Present` doesn't wait
//!   for vsync and frames can't pile up in a present queue. (Tearing can show a horizontal
//!   seam during fast motion; that's the price of not waiting up to a refresh interval.)

use crate::decode::Decoded;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct Renderer {
    pub device: ID3D11Device,
    swap: IDXGISwapChain1,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vp: Option<Vp>,
    views: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
    tearing: bool,
    swap_flags: u32,
    pub win_w: u32,
    pub win_h: u32,
}

struct Vp {
    key: (u32, u32, u32, u32),
    enumr: ID3D11VideoProcessorEnumerator,
    proc_: ID3D11VideoProcessor,
}

/// Up to this many pixels smaller than the window (each way), the video is shown 1:1 with a thin
/// border instead of stretched: a sharing PC sizing its virtual display to this window rounds the
/// width down to a multiple of 8, and stretching by those few pixels would blur everything.
const ONE_TO_ONE_SLACK: u32 = 16;

/// Where the video goes in the window: 1:1 and centered if it's the window's size or a few pixels
/// smaller, otherwise the largest rectangle with the video's aspect ratio, centered.
pub fn fit(video_w: u32, video_h: u32, win_w: u32, win_h: u32) -> RECT {
    if video_w <= win_w && video_h <= win_h && win_w - video_w <= ONE_TO_ONE_SLACK && win_h - video_h <= ONE_TO_ONE_SLACK {
        let (x, y) = (((win_w - video_w) / 2) as i32, ((win_h - video_h) / 2) as i32);
        return RECT { left: x, top: y, right: x + video_w as i32, bottom: y + video_h as i32 };
    }
    let (vw, vh, ww, wh) = (video_w.max(1) as u64, video_h.max(1) as u64, win_w.max(1) as u64, win_h.max(1) as u64);
    let (w, h) = if ww * vh <= wh * vw { (ww, ww * vh / vw) } else { (wh * vw / vh, wh) };
    let (x, y) = ((ww - w) / 2, (wh - h) / 2);
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

impl Renderer {
    pub fn new(hwnd: HWND, win_w: u32, win_h: u32) -> Result<Renderer> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.unwrap();
            let ctx: ID3D11DeviceContext = ctx.unwrap();
            // The decoder uses this device from its own threads too.
            let _ = ctx.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);

            let factory: IDXGIFactory2 = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
            let mut allow = BOOL(0);
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f5| f5.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut _ as *mut _, 4))
                .is_ok()
                && allow.as_bool();
            let swap_flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32 | if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: win_w.max(1),
                Height: win_h.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: swap_flags,
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?;
            // Alt+Enter belongs to the remote PC, not to DXGI fullscreen.
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            swap.cast::<IDXGISwapChain2>()?.SetMaximumFrameLatency(1)?;
            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            Ok(Renderer { device, swap, vdev, vctx, vp: None, views: HashMap::new(), tearing, swap_flags, win_w, win_h })
        }
    }

    pub fn tearing(&self) -> bool {
        self.tearing
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if w == 0 || h == 0 || (w, h) == (self.win_w, self.win_h) {
            return Ok(());
        }
        self.win_w = w;
        self.win_h = h;
        unsafe { self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(self.swap_flags as i32))? };
        Ok(())
    }

    fn processor(&mut self, tex_w: u32, tex_h: u32) -> Result<&Vp> {
        let key = (tex_w, tex_h, self.win_w, self.win_h);
        if self.vp.as_ref().map(|v| v.key) != Some(key) {
            self.views.clear();
            unsafe {
                let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                    InputWidth: tex_w,
                    InputHeight: tex_h,
                    OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                    OutputWidth: self.win_w,
                    OutputHeight: self.win_h,
                    Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
                };
                let enumr = self.vdev.CreateVideoProcessorEnumerator(&cd)?;
                let proc_ = self.vdev.CreateVideoProcessor(&enumr, 0)?;
                // Input: BT.709 limited range (what the server's encoder signals). Output: full-range RGB.
                let stream_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
                let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                self.vctx.VideoProcessorSetStreamColorSpace(&proc_, 0, &stream_cs);
                self.vctx.VideoProcessorSetOutputColorSpace(&proc_, &out_cs);
                self.vctx.VideoProcessorSetStreamFrameFormat(&proc_, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                self.vctx.VideoProcessorSetStreamAutoProcessingMode(&proc_, 0, false);
                let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
                self.vctx.VideoProcessorSetOutputBackgroundColor(&proc_, false, &black);
                self.vp = Some(Vp { key, enumr, proc_ });
            }
        }
        Ok(self.vp.as_ref().unwrap())
    }

    /// Scales `frame` (whose visible area is `video_w`×`video_h`) into the window and presents.
    pub fn present(&mut self, frame: &Decoded, video_w: u32, video_h: u32) -> Result<()> {
        unsafe {
            let mut td = D3D11_TEXTURE2D_DESC::default();
            frame.texture.GetDesc(&mut td);
            self.processor(td.Width, td.Height)?;
            let key = (frame.texture.as_raw() as usize, frame.index);
            if !self.views.contains_key(&key) {
                if self.views.len() > 64 {
                    self.views.clear();
                }
                let vp = self.vp.as_ref().unwrap();
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: frame.index } },
                };
                let mut view = None;
                self.vdev.CreateVideoProcessorInputView(&frame.texture, &vp.enumr, &desc, Some(&mut view))?;
                self.views.insert(key, view.unwrap());
            }
            let view = self.views[&key].clone();
            let vp = self.vp.as_ref().unwrap();

            let bb: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut ov = None;
            self.vdev.CreateVideoProcessorOutputView(&bb, &vp.enumr, &od, Some(&mut ov))?;
            let ov = ov.unwrap();

            let src = RECT { left: 0, top: 0, right: video_w.min(td.Width) as i32, bottom: video_h.min(td.Height) as i32 };
            let dst = fit(video_w, video_h, self.win_w, self.win_h);
            let full = RECT { left: 0, top: 0, right: self.win_w as i32, bottom: self.win_h as i32 };
            self.vctx.VideoProcessorSetStreamSourceRect(&vp.proc_, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(&vp.proc_, 0, true, Some(&dst));
            self.vctx.VideoProcessorSetOutputTargetRect(&vp.proc_, true, Some(&full));
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(view)),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&vp.proc_, &ov, 0, &streams);
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.context("VideoProcessorBlt")?;

            let (interval, flags) = if self.tearing { (0, DXGI_PRESENT_ALLOW_TEARING) } else { (0, DXGI_PRESENT(0)) };
            self.swap.Present(interval, flags).ok().context("Present")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    fn rect(r: windows::Win32::Foundation::RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right - r.left, r.bottom - r.top)
    }

    #[test]
    fn exact_or_slightly_smaller_video_is_shown_one_to_one() {
        assert_eq!(rect(fit(1920, 990, 1920, 990)), (0, 0, 1920, 990));
        // 1606 wide rounded down to 1600 by the sharing PC: 1:1 with a 3 px edge, not stretched.
        assert_eq!(rect(fit(1600, 978, 1606, 978)), (3, 0, 1600, 978));
    }

    #[test]
    fn other_sizes_scale_to_fit() {
        assert_eq!(rect(fit(1920, 1080, 960, 540)), (0, 0, 960, 540));
        assert_eq!(rect(fit(1280, 720, 1920, 1200)), (0, 60, 1920, 1080), "much smaller: scaled up");
        assert_eq!(rect(fit(1920, 1080, 1920, 1200)), (0, 60, 1920, 1080), "letterboxed");
    }
}

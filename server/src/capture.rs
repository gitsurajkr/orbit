//! Screen capture with DXGI Desktop Duplication.
//!
//! Why this API: it hands us the desktop as a GPU texture, only when something changed, with no
//! border or permission prompt. We copy that texture into our own (`CopyResource`, GPU-only,
//! microseconds) and release the duplication frame immediately, so Windows can keep composing.
//! The copy is what NVENC reads: the picture never touches system memory.

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub const NVIDIA_VENDOR: u32 = 0x10DE;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    pub adapter_name: String,
    pub vendor: u32,
    pub device_name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Every monitor attached to the desktop, on every GPU, in DXGI order.
pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let ad = adapter.GetDesc1()?;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let od = output.GetDesc()?;
                if od.AttachedToDesktop.as_bool() {
                    let r = od.DesktopCoordinates;
                    out.push(MonitorInfo {
                        adapter_index: a,
                        output_index: o,
                        adapter_name: wide(&ad.Description),
                        vendor: ad.VendorId,
                        device_name: wide(&od.DeviceName),
                        left: r.left,
                        top: r.top,
                        width: (r.right - r.left) as u32,
                        height: (r.bottom - r.top) as u32,
                    });
                }
                o += 1;
            }
            a += 1;
        }
    }
    Ok(out)
}

/// Index of the first adapter from `vendor`, if any.
pub fn find_adapter(vendor: u32) -> Result<Option<(u32, String)>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let d = adapter.GetDesc1()?;
            if d.VendorId == vendor {
                return Ok(Some((a, wide(&d.Description))));
            }
            a += 1;
        }
    }
    Ok(None)
}

pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
}

impl Gpu {
    pub fn on_adapter(adapter_index: u32) -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(adapter_index)?;
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            Ok(Gpu { device: device.unwrap(), ctx: ctx.unwrap() })
        }
    }

    /// A BGRA texture the size of the screen: the capture target and NVENC's input.
    pub fn screen_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.unwrap())
    }

    /// A CPU-readable copy target, for moving frames between GPUs.
    pub fn staging_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.unwrap())
    }
}

/// Moves a frame from the GPU that drives the screen to the GPU that encodes (hybrid laptops):
/// GPU copy to a staging texture, map it, upload into the encoder GPU's texture. Costs a few
/// ms per frame, so it's only used when the two GPUs differ.
pub struct Bridge {
    pub src_gpu: Gpu,
    pub src_tex: ID3D11Texture2D,
    staging: ID3D11Texture2D,
}

impl Bridge {
    pub fn new(src_gpu: Gpu, width: u32, height: u32) -> Result<Bridge> {
        let src_tex = src_gpu.screen_texture(width, height)?;
        let staging = src_gpu.staging_texture(width, height)?;
        Ok(Bridge { src_gpu, src_tex, staging })
    }

    pub fn transfer(&self, dst_gpu: &Gpu, dst: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let ctx = &self.src_gpu.ctx;
            ctx.CopyResource(&self.staging, &self.src_tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).context("map staging")?;
            dst_gpu.ctx.UpdateSubresource(dst, 0, None, m.pData, m.RowPitch, 0);
            ctx.Unmap(&self.staging, 0);
        }
        Ok(())
    }
}

pub enum Grab {
    /// The screen changed and was copied into the destination texture.
    Frame,
    /// Nothing changed within the timeout (or only the mouse moved).
    Timeout,
    /// Access was lost (UAC prompt, lock screen, mode change) and the duplication was
    /// recreated; the next frame is a full fresh image.
    Recreated,
}

pub struct Duplicator {
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    pub mon: MonitorInfo,
}

impl Duplicator {
    pub fn new(gpu: &Gpu, mon: MonitorInfo) -> Result<Duplicator> {
        let output = unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            factory.EnumAdapters1(mon.adapter_index)?.EnumOutputs(mon.output_index)?.cast::<IDXGIOutput1>()?
        };
        let mut d = Duplicator { output, dup: None, mon };
        d.recreate(gpu)?;
        Ok(d)
    }

    fn recreate(&mut self, gpu: &Gpu) -> Result<()> {
        self.dup = None;
        // On the secure desktop (UAC, Ctrl+Alt+Del) duplication is refused for a while; keep
        // trying instead of giving up.
        for attempt in 0.. {
            match unsafe { self.output.DuplicateOutput(&gpu.device) } {
                Ok(d) => {
                    self.dup = Some(d);
                    return Ok(());
                }
                Err(e) if attempt < 100 && (e.code() == windows::Win32::Foundation::E_ACCESSDENIED || e.code() == DXGI_ERROR_UNSUPPORTED || e.code() == DXGI_ERROR_SESSION_DISCONNECTED) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => return Err(e).context("DuplicateOutput"),
            }
        }
        unreachable!()
    }

    /// Waits up to `timeout_ms` for a new desktop image and copies it into `dst`.
    pub fn grab(&mut self, gpu: &Gpu, timeout_ms: u32, dst: &ID3D11Texture2D) -> Result<Grab> {
        let Some(dup) = &self.dup else { bail!("no duplication") };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                self.recreate(gpu)?;
                return Ok(Grab::Recreated);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        // LastPresentTime == 0 means only the pointer moved; the client draws its own pointer.
        let changed = info.LastPresentTime != 0;
        if changed {
            if let Some(res) = res {
                let tex: ID3D11Texture2D = res.cast()?;
                unsafe { gpu.ctx.CopyResource(dst, &tex) };
            }
        }
        unsafe { dup.ReleaseFrame()? };
        Ok(if changed { Grab::Frame } else { Grab::Timeout })
    }
}

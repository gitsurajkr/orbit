//! NVENC, driven directly through the driver's `nvEncodeAPI64.dll` (no SDK or CUDA install).
//!
//! Low-latency choices, each one removing a source of delay:
//! - **Synchronous mode**: `encode()` submits a frame and waits for its bitstream. No frame can
//!   sit inside the encoder waiting for the next one.
//! - **D3D11 texture input in BGRA**: the captured texture is registered once; NVENC does the
//!   RGB→YUV conversion itself, so there's no separate conversion pass.
//! - **P1 preset + ULTRA_LOW_LATENCY tuning, no B-frames, no lookahead, zero reorder delay**:
//!   each frame's output depends only on the past.
//! - **CBR with a one-frame VBV**: every frame is at most about bitrate/fps bits, so no frame
//!   (even a keyframe) bursts onto the network and builds a queue.
//! - **Infinite GOP**: keyframes are sent only at start and when the client asks (after a loss).

#[allow(clippy::all)]
mod sys;

use anyhow::{bail, ensure, Context, Result};
use std::ffi::{c_void, CStr};
use std::mem::zeroed;
use std::ptr::null_mut;
use sys::*;
use windows::core::{s, w, Interface};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const fn struct_ver(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}
const OPEN_SESSION_VER: u32 = struct_ver(1);
const FUNCTION_LIST_VER: u32 = struct_ver(2);
const PRESET_CONFIG_VER: u32 = struct_ver(4) | (1 << 31);
const CONFIG_VER: u32 = struct_ver(8) | (1 << 31);
const INIT_PARAMS_VER: u32 = struct_ver(6) | (1 << 31);
const RECONFIGURE_VER: u32 = struct_ver(1) | (1 << 31);
const REGISTER_VER: u32 = struct_ver(4);
const MAP_VER: u32 = struct_ver(4);
const BITSTREAM_BUFFER_VER: u32 = struct_ver(1);
const PIC_PARAMS_VER: u32 = struct_ver(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_ver(1) | (1 << 31);

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}
const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const PRESET_P1: GUID = guid(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]);

const FMT_ARGB: NV_ENC_BUFFER_FORMAT = _NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_ARGB;
const TUNING_ULL: NV_ENC_TUNING_INFO = NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

pub struct EncodedFrame {
    pub keyframe: bool,
}

pub struct Encoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    bitstream: NV_ENC_OUTPUT_PTR,
    registered: NV_ENC_REGISTERED_PTR,
    // Boxed: NVENC keeps a pointer to the config for reconfiguration.
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    cfg: EncoderConfig,
    frame_idx: u32,
    _input: ID3D11Texture2D,
    _device: ID3D11Device,
}

type CreateInstanceFn = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxVersionFn = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

fn load_api() -> Result<NV_ENCODE_API_FUNCTION_LIST> {
    unsafe {
        let lib = LoadLibraryW(w!("nvEncodeAPI64.dll")).context("nvEncodeAPI64.dll not found: is an NVIDIA driver installed?")?;
        let max: MaxVersionFn = std::mem::transmute(GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion")).context("no NvEncodeAPIGetMaxSupportedVersion")?);
        let mut v = 0u32;
        nv(max(&mut v), "NvEncodeAPIGetMaxSupportedVersion")?;
        let ours = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        ensure!(v >= ours, "driver supports NVENC API {}.{}, need {}.{}: update the NVIDIA driver", v >> 4, v & 0xf, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
        let create: CreateInstanceFn = std::mem::transmute(GetProcAddress(lib, s!("NvEncodeAPICreateInstance")).context("no NvEncodeAPICreateInstance")?);
        let mut api: NV_ENCODE_API_FUNCTION_LIST = zeroed();
        api.version = FUNCTION_LIST_VER;
        nv(create(&mut api), "NvEncodeAPICreateInstance")?;
        Ok(api)
    }
}

fn nv(status: NVENCSTATUS, what: &str) -> Result<()> {
    if status == _NVENCSTATUS_NV_ENC_SUCCESS {
        Ok(())
    } else {
        bail!("{what} failed: NVENCSTATUS {status}")
    }
}

/// Calls an entry of the function list, failing cleanly if the driver left it empty.
macro_rules! call {
    ($self:expr, $f:ident ( $($arg:expr),* )) => {{
        let f = $self.api.$f.ok_or_else(|| anyhow::anyhow!(concat!(stringify!($f), " missing")))?;
        let st = f($($arg),*);
        if st != _NVENCSTATUS_NV_ENC_SUCCESS {
            bail!("{} failed: NVENCSTATUS {} ({})", stringify!($f), st, $self.last_error());
        }
    }};
}

impl Encoder {
    /// Opens NVENC on `device` (must be an NVIDIA GPU) and registers `input`, the BGRA texture
    /// every frame will be read from.
    pub fn new(device: &ID3D11Device, input: &ID3D11Texture2D, cfg: EncoderConfig) -> Result<Encoder> {
        let api = load_api()?;
        unsafe {
            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
            open.version = OPEN_SESSION_VER;
            open.deviceType = _NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            open.apiVersion = NVENCAPI_VERSION;
            let mut enc = null_mut();
            let f = api.nvEncOpenEncodeSessionEx.context("nvEncOpenEncodeSessionEx missing")?;
            nv(f(&mut open, &mut enc), "nvEncOpenEncodeSessionEx (is this an NVIDIA GPU?)")?;

            let mut this = Encoder {
                api,
                enc,
                bitstream: null_mut(),
                registered: null_mut(),
                init: Box::new(zeroed()),
                config: Box::new(zeroed()),
                cfg,
                frame_idx: 0,
                _input: input.clone(),
                _device: device.clone(),
            };
            // From here on, Drop cleans up whatever was created if a step fails.
            this.configure()?;
            call!(this, nvEncInitializeEncoder(this.enc, &mut *this.init));

            let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
            bs.version = BITSTREAM_BUFFER_VER;
            call!(this, nvEncCreateBitstreamBuffer(this.enc, &mut bs));
            this.bitstream = bs.bitstreamBuffer;

            let mut reg: NV_ENC_REGISTER_RESOURCE = zeroed();
            reg.version = REGISTER_VER;
            reg.resourceType = _NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = cfg.width;
            reg.height = cfg.height;
            reg.resourceToRegister = input.as_raw();
            reg.bufferFormat = FMT_ARGB;
            reg.bufferUsage = _NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE;
            call!(this, nvEncRegisterResource(this.enc, &mut reg));
            this.registered = reg.registeredResource;
            Ok(this)
        }
    }

    /// Fills `self.config` / `self.init` from the P1 low-latency preset plus our overrides.
    unsafe fn configure(&mut self) -> Result<()> {
        let cfg = self.cfg;
        let mut preset: NV_ENC_PRESET_CONFIG = zeroed();
        preset.version = PRESET_CONFIG_VER;
        preset.presetCfg.version = CONFIG_VER;
        call!(self, nvEncGetEncodePresetConfigEx(self.enc, CODEC_H264, PRESET_P1, TUNING_ULL, &mut preset));

        let c = &mut *self.config;
        *c = preset.presetCfg;
        c.version = CONFIG_VER;
        c.gopLength = NVENC_INFINITE_GOPLENGTH;
        c.frameIntervalP = 1; // I and P only: no B-frames, so no reordering delay

        let rc = &mut c.rcParams;
        rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = cfg.bitrate_bps;
        rc.maxBitRate = cfg.bitrate_bps;
        rc.vbvBufferSize = cfg.bitrate_bps / cfg.fps.max(1); // one frame
        rc.vbvInitialDelay = rc.vbvBufferSize;
        rc.set_zeroReorderDelay(1);
        rc.set_enableLookahead(0);
        rc.lookaheadDepth = 0;

        let h = &mut c.encodeCodecConfig.h264Config;
        h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h.set_repeatSPSPPS(1); // every keyframe is self-contained, so a client can join any time
        h.set_outputAUD(0);
        // Tell the decoder exactly how to turn YUV back into RGB: BT.709, limited range.
        let vui = &mut h.h264VUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vui.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;

        let i = &mut *self.init;
        *i = zeroed();
        i.version = INIT_PARAMS_VER;
        i.encodeGUID = CODEC_H264;
        i.presetGUID = PRESET_P1;
        i.tuningInfo = TUNING_ULL;
        i.encodeWidth = cfg.width;
        i.encodeHeight = cfg.height;
        i.darWidth = cfg.width;
        i.darHeight = cfg.height;
        i.maxEncodeWidth = cfg.width;
        i.maxEncodeHeight = cfg.height;
        i.frameRateNum = cfg.fps;
        i.frameRateDen = 1;
        i.enableEncodeAsync = 0;
        i.enablePTD = 1;
        i.bufferFormat = FMT_ARGB;
        i.encodeConfig = &mut *self.config;
        Ok(())
    }

    fn last_error(&self) -> String {
        unsafe {
            match self.api.nvEncGetLastErrorString {
                Some(f) if !self.enc.is_null() => {
                    let p = f(self.enc);
                    if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                }
                _ => String::new(),
            }
        }
    }

    /// Encodes whatever is currently in the input texture. Appends the H.264 Annex-B bitstream
    /// to `out`. `force_idr` makes this frame a keyframe.
    pub fn encode(&mut self, force_idr: bool, timestamp_us: u64, out: &mut Vec<u8>) -> Result<EncodedFrame> {
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
            map.version = MAP_VER;
            map.registeredResource = self.registered;
            call!(self, nvEncMapInputResource(self.enc, &mut map));
            let result = self.encode_mapped(map.mappedResource, force_idr, timestamp_us, out);
            call!(self, nvEncUnmapInputResource(self.enc, map.mappedResource));
            result
        }
    }

    unsafe fn encode_mapped(&mut self, input: NV_ENC_INPUT_PTR, force_idr: bool, ts: u64, out: &mut Vec<u8>) -> Result<EncodedFrame> {
        let mut pic: NV_ENC_PIC_PARAMS = zeroed();
        pic.version = PIC_PARAMS_VER;
        pic.inputWidth = self.cfg.width;
        pic.inputHeight = self.cfg.height;
        pic.inputPitch = self.cfg.width;
        pic.inputBuffer = input;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = FMT_ARGB;
        pic.pictureStruct = _NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME;
        pic.frameIdx = self.frame_idx;
        pic.inputTimeStamp = ts;
        if force_idr {
            pic.encodePicFlags = (_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR | _NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) as u32;
        }
        self.frame_idx = self.frame_idx.wrapping_add(1);
        call!(self, nvEncEncodePicture(self.enc, &mut pic));

        let mut lock: NV_ENC_LOCK_BITSTREAM = zeroed();
        lock.version = LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        call!(self, nvEncLockBitstream(self.enc, &mut lock)); // waits for this frame
        let bytes = std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize);
        out.extend_from_slice(bytes);
        let keyframe = lock.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR || lock.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_I;
        call!(self, nvEncUnlockBitstream(self.enc, self.bitstream));
        Ok(EncodedFrame { keyframe })
    }

    /// Changes the bitrate on the fly (no new keyframe needed).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.cfg.bitrate_bps = bps;
        unsafe {
            let rc = &mut self.config.rcParams;
            rc.averageBitRate = bps;
            rc.maxBitRate = bps;
            rc.vbvBufferSize = bps / self.cfg.fps.max(1);
            rc.vbvInitialDelay = rc.vbvBufferSize;
            let mut re: NV_ENC_RECONFIGURE_PARAMS = zeroed();
            re.version = RECONFIGURE_VER;
            re.reInitEncodeParams = *self.init;
            re.reInitEncodeParams.encodeConfig = &mut *self.config;
            call!(self, nvEncReconfigureEncoder(self.enc, &mut re));
        }
        Ok(())
    }

    pub fn config(&self) -> EncoderConfig {
        self.cfg
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.registered.is_null() {
                if let Some(f) = self.api.nvEncUnregisterResource {
                    f(self.enc, self.registered);
                }
            }
            if !self.bitstream.is_null() {
                if let Some(f) = self.api.nvEncDestroyBitstreamBuffer {
                    f(self.enc, self.bitstream);
                }
            }
            if !self.enc.is_null() {
                if let Some(f) = self.api.nvEncDestroyEncoder {
                    f(self.enc);
                }
            }
        }
    }
}

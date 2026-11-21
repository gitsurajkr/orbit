//! H.264 decoding with the Media Foundation decoder on the GPU (DXVA: NVDEC, AMD VCN, Intel QSV).
//!
//! `MF_LOW_LATENCY` makes the decoder output each frame as soon as it's decoded, instead of
//! holding frames for reordering (our stream has no B-frames, so nothing ever needs reordering).
//! Output stays on the GPU: an NV12 texture (often one slice of a texture array).

use anyhow::{bail, Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

pub struct Decoded {
    pub texture: ID3D11Texture2D,
    pub index: u32,
    /// Held so the decoder doesn't reuse this texture while we still show it.
    _sample: IMFSample,
}

pub struct Decoder {
    mft: IMFTransform,
    _manager: IMFDXGIDeviceManager,
}

impl Decoder {
    pub fn new(device: &ID3D11Device) -> Result<Decoder> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).context("H.264 decoder")?;
            mft.GetAttributes()?.SetUINT32(&MF_LOW_LATENCY, 1)?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no DXGI device manager")?;
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("decoder refused GPU acceleration")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            mft.SetInputType(0, &input, 0)?;
            let d = Decoder { mft, _manager: manager };
            d.set_output_type()?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(d)
        }
    }

    fn set_output_type(&self) -> Result<()> {
        unsafe {
            for i in 0.. {
                let Ok(t) = self.mft.GetOutputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(());
                }
            }
        }
        bail!("decoder offers no NV12 output")
    }

    /// Decodes one access unit (Annex-B). Decoded frames are appended to `out` (normally one).
    pub fn decode(&mut self, data: &[u8], timestamp_us: u64, out: &mut Vec<Decoded>) -> Result<()> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(timestamp_us as i64 * 10)?;
            if let Err(e) = self.mft.ProcessInput(0, &sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(e).context("ProcessInput");
                }
                self.drain(out)?;
                self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
            }
            self.drain(out)
        }
    }

    fn drain(&mut self, out: &mut Vec<Decoded>) -> Result<()> {
        unsafe {
            loop {
                let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
                let mut status = 0u32;
                let r = self.mft.ProcessOutput(0, &mut db, &mut status);
                let sample = ManuallyDrop::take(&mut db[0].pSample);
                drop(ManuallyDrop::take(&mut db[0].pEvents));
                match r {
                    Ok(()) => {
                        let sample = sample.context("decoder returned no sample")?;
                        let buf: IMFDXGIBuffer = sample.GetBufferByIndex(0)?.cast().context("decoder output isn't on the GPU")?;
                        let mut tex: Option<ID3D11Texture2D> = None;
                        buf.GetResource(&ID3D11Texture2D::IID, &mut tex as *mut _ as *mut _)?;
                        let index = buf.GetSubresourceIndex()?;
                        out.push(Decoded { texture: tex.context("no texture")?, index, _sample: sample });
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_output_type()?,
                    Err(e) => return Err(e).context("ProcessOutput"),
                }
            }
        }
    }
}

//! H.264 encoding with Media Foundation.
//!
//! Prefers a hardware encoder MFT (NVIDIA / AMD / Intel), which takes D3D11
//! NV12 textures directly and works asynchronously. Falls back to
//! Microsoft's software H.264 encoder (synchronous, fed from CPU memory).
//! Output is Annex-B; SPS/PPS are added to keyframes if missing.

use std::mem::ManuallyDrop;
use std::sync::Once;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, Result};

pub(super) struct Encoded {
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub key: bool,
}

const MF_VERSION_FULL: u32 = 0x0002_0070;
const EV_NEED_INPUT: u32 = 601; // METransformNeedInput
const EV_HAVE_OUTPUT: u32 = 602; // METransformHaveOutput

pub(super) struct Encoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>, // Some => asynchronous hardware MFT
    _manager: Option<IMFDXGIDeviceManager>,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    staging: Option<ID3D11Texture2D>,
    need_input: u32,
    seq_header: Vec<u8>,
    width: u32,
    height: u32,
    fps: u32,
}

fn startup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION_FULL, MFSTARTUP_FULL);
    });
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn set_common(t: &IMFMediaType, w: u32, h: u32, fps: u32) -> Result<()> {
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // BT.709, studio range: matches the video processor's output and the
        // macOS host, and what the viewer's canvas path expects.
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

impl Encoder {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, w: u32, h: u32, fps: u32, kbps: u32) -> Result<Encoder> {
        startup();
        match Self::hardware(device, context, w, h, fps, kbps) {
            Ok(e) => Ok(e),
            Err(hw_err) => {
                eprintln!("hardware H.264 encoder unavailable ({hw_err}); using software encoder");
                Self::software(device, context, w, h, fps, kbps)
            }
        }
    }

    fn hardware(device: &ID3D11Device, context: &ID3D11DeviceContext, w: u32, h: u32, fps: u32, kbps: u32) -> Result<Encoder> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )?;
        }
        let activates: Vec<IMFActivate> = unsafe {
            let v = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
            if !list.is_null() {
                CoTaskMemFree(Some(list as *const _));
            }
            v
        };
        let mut last_err = windows::core::Error::new(MF_E_NOT_FOUND, "no hardware H.264 encoder");
        for act in activates {
            match Self::try_hardware(&act, device, context, w, h, fps, kbps) {
                Ok(e) => return Ok(e),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    fn try_hardware(
        act: &IMFActivate,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        w: u32,
        h: u32,
        fps: u32,
        kbps: u32,
    ) -> Result<Encoder> {
        unsafe {
            let mft: IMFTransform = act.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let mut enc = Encoder {
                codec: mft.cast().ok(),
                events: Some(mft.cast()?),
                mft,
                _manager: Some(manager),
                context: context.clone(),
                device: device.clone(),
                staging: None,
                need_input: 0,
                seq_header: Vec::new(),
                width: w,
                height: h,
                fps,
            };
            enc.configure(kbps)?;
            Ok(enc)
        }
    }

    fn software(device: &ID3D11Device, context: &ID3D11DeviceContext, w: u32, h: u32, fps: u32, kbps: u32) -> Result<Encoder> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;
            let mut enc = Encoder {
                codec: mft.cast().ok(),
                events: None,
                mft,
                _manager: None,
                context: context.clone(),
                device: device.clone(),
                staging: None,
                need_input: 0,
                seq_header: Vec::new(),
                width: w,
                height: h,
                fps,
            };
            enc.configure(kbps)?;
            Ok(enc)
        }
    }

    fn codec_u32(&self, api: &GUID, v: u32) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(api, &VARIANT::from(v));
            }
        }
    }

    fn configure(&mut self, kbps: u32) -> Result<()> {
        let (w, h, fps) = (self.width, self.height, self.fps);
        // Rate control and latency first: some encoders lock them once the
        // media types are set.
        self.codec_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
        self.codec_u32(&CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
        self.codec_u32(&CODECAPI_AVLowLatencyMode, 1);
        self.codec_u32(&CODECAPI_AVEncMPVGOPSize, fps * 3);
        self.codec_u32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        unsafe {
            let out = MFCreateMediaType()?;
            set_common(&out, w, h, fps)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            self.mft.SetOutputType(0, &out, 0)?;

            // Pick the encoder's own NV12 input type, then fill in the details.
            let mut input = None;
            for i in 0.. {
                match self.mft.GetInputAvailableType(0, i) {
                    Ok(t) => {
                        if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                            input = Some(t);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let input = match input {
                Some(t) => t,
                None => {
                    let t = MFCreateMediaType()?;
                    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                    t
                }
            };
            set_common(&input, w, h, fps)?;
            self.mft.SetInputType(0, &input, 0)?;

            self.read_seq_header();
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    fn read_seq_header(&mut self) {
        unsafe {
            if let Ok(t) = self.mft.GetOutputCurrentType(0) {
                if let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    let mut buf = vec![0u8; n as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() {
                        self.seq_header = buf;
                    }
                }
            }
        }
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.codec_u32(&CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
    }

    /// Queue one NV12 frame. Encoded output lands in `out` (now or on a later
    /// `encode`/`poll` call: hardware encoders work asynchronously).
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, pts_us: i64, key: bool, out: &mut Vec<Encoded>) -> Result<()> {
        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.SetSampleTime(pts_us * 10)?;
            sample.SetSampleDuration(10_000_000 / self.fps.max(1) as i64)?;
        }
        if self.events.is_some() {
            // Async: input is only accepted after METransformNeedInput.
            self.poll(out)?;
            for _ in 0..10 {
                if self.need_input > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                self.poll(out)?;
            }
            if self.need_input == 0 {
                return Ok(()); // encoder busy: drop this frame
            }
            let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)? };
            unsafe { sample.AddBuffer(&buffer)? };
            if key {
                self.codec_u32(&CODECAPI_AVEncVideoForceKeyFrame, 1);
            }
            unsafe { self.mft.ProcessInput(0, &sample, 0)? };
            self.need_input -= 1;
            // Hardware encoders finish in a few ms; collect it right away.
            for _ in 0..12 {
                let before = out.len();
                self.poll(out)?;
                if out.len() > before {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        } else {
            let buffer = self.copy_to_memory(nv12)?;
            unsafe { sample.AddBuffer(&buffer)? };
            if key {
                self.codec_u32(&CODECAPI_AVEncVideoForceKeyFrame, 1);
            }
            unsafe { self.mft.ProcessInput(0, &sample, 0)? };
            while self.pull_output(out)? {}
        }
        Ok(())
    }

    /// Handle pending async encoder events (no-op for the software encoder).
    pub fn poll(&mut self, out: &mut Vec<Encoded>) -> Result<()> {
        let Some(events) = self.events.clone() else { return Ok(()) };
        loop {
            let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            };
            match unsafe { ev.GetType()? } {
                EV_NEED_INPUT => self.need_input += 1,
                EV_HAVE_OUTPUT => {
                    self.pull_output(out)?;
                }
                _ => {}
            }
        }
    }

    /// Fetch one output sample. Returns false when the encoder needs more input.
    fn pull_output(&mut self, out: &mut Vec<Encoded>) -> Result<bool> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(0)?;
            let provides =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            let own = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                let b = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?;
                s.AddBuffer(&b)?;
                Some(s)
            };
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
            let sample = ManuallyDrop::into_inner(buf.pSample);
            drop(ManuallyDrop::into_inner(buf.pEvents));
            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    self.read_seq_header();
                    return Ok(true);
                }
                Err(e) => return Err(e),
                Ok(()) => {}
            }
            let Some(sample) = sample else { return Ok(true) };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let pts_us = sample.GetSampleTime().unwrap_or(0) / 10;
            let media = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            media.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = media.Unlock();
            out.push(Encoded { data: self.annex_b(bytes, key), pts_us, key });
            Ok(true)
        }
    }

    /// Media Foundation encoders emit Annex-B; make sure keyframes carry SPS/PPS.
    fn annex_b(&self, data: Vec<u8>, key: bool) -> Vec<u8> {
        if !key || self.seq_header.is_empty() || has_sps(&data) {
            return data;
        }
        let mut v = self.seq_header.clone();
        if !v.starts_with(&[0, 0, 0, 1]) && !v.starts_with(&[0, 0, 1]) {
            v.splice(0..0, [0, 0, 0, 1]);
        }
        v.extend_from_slice(&data);
        v
    }

    /// Software encoder input: copy the GPU NV12 texture into an MF buffer.
    fn copy_to_memory(&mut self, nv12: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        let (w, h) = (self.width, self.height);
        unsafe {
            if self.staging.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
                self.staging = t;
            }
            let staging = self.staging.as_ref().unwrap();
            self.context.CopyResource(staging, nv12);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let size = (w * h * 3 / 2) as usize;
            let buffer = MFCreateMemoryBuffer(size as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            let src = map.pData as *const u8;
            let pitch = map.RowPitch as usize;
            for row in 0..(h as usize * 3 / 2) {
                std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w as usize), w as usize);
            }
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(size as u32)?;
            self.context.Unmap(staging, 0);
            Ok(buffer)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Some(ev) = &self.events {
                if let Ok(s) = ev.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
        }
    }
}

fn has_sps(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

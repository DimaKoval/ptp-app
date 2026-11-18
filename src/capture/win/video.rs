//! Windows.Graphics.Capture → D3D11 video processor → H.264.

use super::encoder::{Encoded, Encoder};
use super::sources::{hwnd, monitors};
use super::{Config, Shared, fit_size, qpc_us};
use crate::capture::SourceKind;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{Interface, Result};

pub(super) fn run(cfg: Config, shared: Arc<Shared>, ready: mpsc::Sender<std::result::Result<(u32, u32), String>>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut ready = Some(ready);
    if let Err(e) = run_inner(&cfg, &shared, &mut ready) {
        let msg = format!("Video capture failed: {e}");
        match ready.take() {
            Some(tx) => {
                let _ = tx.send(Err(msg));
            }
            None => (shared.callbacks.error)(msg),
        }
    }
    unsafe { CoUninitialize() };
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device: ID3D11Device = device.unwrap();
    // Media Foundation uses the device from its own threads too.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Ok((device, context.unwrap()))
}

fn capture_item(cfg: &Config) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe {
        match cfg.kind {
            SourceKind::Window => interop.CreateForWindow(hwnd(cfg.id)),
            _ => {
                let m = monitors()
                    .into_iter()
                    .nth(cfg.id.saturating_sub(1) as usize)
                    .ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "display is no longer available"))?;
                interop.CreateForMonitor(m.handle)
            }
        }
    }
}

fn run_inner(
    cfg: &Config,
    shared: &Arc<Shared>,
    ready: &mut Option<mpsc::Sender<std::result::Result<(u32, u32), String>>>,
) -> Result<()> {
    let (device, context) = create_device()?;
    let dxgi: IDXGIDevice = device.cast()?;
    let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

    let item = capture_item(cfg)?;
    let mut pool_size = item.Size()?;
    let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, format, 2, pool_size)?;
    let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    let _ = session.SetIsBorderRequired(false); // Windows 11; harmless to fail
    let (frame_tx, frame_rx) = mpsc::channel::<()>();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        let _ = frame_tx.send(());
        Ok(())
    }))?;

    let (mut enc_w, mut enc_h) = fit_size(pool_size.Width as u32, pool_size.Height as u32, cfg.max_width, cfg.max_height);
    let mut vp = Converter::new(&device, &context, cfg.fps)?;
    let mut encoder = Encoder::new(&device, &context, enc_w, enc_h, cfg.fps, cfg.bitrate_kbps)?;
    let mut bitrate = cfg.bitrate_kbps;
    session.StartCapture()?;
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok((enc_w, enc_h)));
    }

    let mut out: Vec<Encoded> = Vec::new();
    let mut last: Option<ID3D11Texture2D> = None;
    let mut last_encode = Instant::now();
    let mut last_pts = 0i64;
    let mut cand: Option<((u32, u32), Instant)> = None;

    while !shared.stop.load(Ordering::SeqCst) {
        let _ = frame_rx.recv_timeout(Duration::from_millis(20));
        let kbps = shared.bitrate_kbps.load(Ordering::SeqCst);
        if kbps != bitrate {
            bitrate = kbps;
            encoder.set_bitrate(kbps);
        }

        // Newest frame only; older ones are dropped.
        let mut latest = None;
        while let Ok(f) = pool.TryGetNextFrame() {
            latest = Some(f);
        }

        if let Some(frame) = latest {
            let content: SizeInt32 = frame.ContentSize()?;
            if content.Width > 0 && content.Height > 0 && (content.Width != pool_size.Width || content.Height != pool_size.Height) {
                // The window was resized: grow/shrink the frame pool now, the
                // encoder once the new size has been stable for 300 ms.
                pool_size = content;
                pool.Recreate(&d3d, format, 2, pool_size)?;
                vp.forget_inputs();
            }
            let target = fit_size(content.Width.max(1) as u32, content.Height.max(1) as u32, cfg.max_width, cfg.max_height);
            if target != (enc_w, enc_h) {
                match cand {
                    Some((c, since)) if c == target && since.elapsed() >= Duration::from_millis(300) => {
                        (enc_w, enc_h) = target;
                        encoder = Encoder::new(&device, &context, enc_w, enc_h, cfg.fps, bitrate)?;
                        shared.force_key.store(true, Ordering::SeqCst);
                        cand = None;
                    }
                    Some((c, _)) if c == target => {}
                    _ => cand = Some((target, Instant::now())),
                }
            } else {
                cand = None;
            }

            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let src = RECT { left: 0, top: 0, right: content.Width, bottom: content.Height };
            let nv12 = vp.convert(&tex, src, enc_w, enc_h)?;
            let mut pts = frame.SystemRelativeTime()?.Duration / 10;
            if pts <= last_pts {
                pts = last_pts + 1000;
            }
            last_pts = pts;
            let key = shared.force_key.swap(false, Ordering::SeqCst);
            encoder.encode(&nv12, pts, key, &mut out)?;
            last = Some(nv12);
            last_encode = Instant::now();
        } else if let Some(nv12) = &last {
            // Nothing changed on screen: re-send the last picture now and then,
            // and right away when a viewer asks for a keyframe.
            let idle = last_encode.elapsed();
            let key_wanted = shared.force_key.load(Ordering::SeqCst);
            if idle > Duration::from_millis(500) || (key_wanted && idle > Duration::from_millis(40)) {
                let pts = qpc_us().max(last_pts + 1000);
                last_pts = pts;
                let key = shared.force_key.swap(false, Ordering::SeqCst);
                encoder.encode(nv12, pts, key, &mut out)?;
                last_encode = Instant::now();
            }
        }

        encoder.poll(&mut out)?;
        for e in out.drain(..) {
            (shared.callbacks.video)(&e.data, e.pts_us, e.key);
        }
    }
    let _ = session.Close();
    let _ = pool.Close();
    Ok(())
}

/// BGRA capture texture → NV12 (BT.709, studio range) at the encoder size,
/// scaled on the GPU by the D3D11 video processor.
struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    device: ID3D11Device,
    fps: u32,
    proc: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, (u32, u32, u32, u32))>,
    inputs: HashMap<usize, ID3D11VideoProcessorInputView>,
    /// Output ring, so the encoder can still be reading one texture while
    /// the next frame is converted into another.
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    out_size: (u32, u32),
    next: usize,
}

impl Converter {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, fps: u32) -> Result<Converter> {
        Ok(Converter {
            vdev: device.cast()?,
            vctx: context.cast()?,
            device: device.clone(),
            fps,
            proc: None,
            inputs: HashMap::new(),
            outputs: Vec::new(),
            out_size: (0, 0),
            next: 0,
        })
    }

    fn forget_inputs(&mut self) {
        self.inputs.clear();
    }

    fn ensure(&mut self, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<()> {
        let key = (in_w, in_h, out_w, out_h);
        if self.proc.as_ref().map(|p| p.2) != Some(key) {
            let rate = DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let en = unsafe { self.vdev.CreateVideoProcessorEnumerator(&desc)? };
            let vp = unsafe { self.vdev.CreateVideoProcessor(&en, 0)? };
            unsafe {
                self.vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                self.vctx.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                self.vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
                self.vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            }
            self.proc = Some((en, vp, key));
            self.inputs.clear();
            self.outputs.clear();
        }
        if self.out_size != (out_w, out_h) || self.outputs.is_empty() {
            self.outputs.clear();
            let en = &self.proc.as_ref().unwrap().0;
            for _ in 0..4 {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: out_w,
                    Height: out_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex = None;
                unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
                let tex = tex.unwrap();
                let vdesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                unsafe { self.vdev.CreateVideoProcessorOutputView(&tex, en, &vdesc, Some(&mut view))? };
                self.outputs.push((tex, view.unwrap()));
            }
            self.out_size = (out_w, out_h);
        }
        Ok(())
    }

    fn convert(&mut self, input: &ID3D11Texture2D, src: RECT, out_w: u32, out_h: u32) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.GetDesc(&mut desc) };
        self.ensure(desc.Width, desc.Height, out_w, out_h)?;
        let (en, vp, _) = self.proc.as_ref().unwrap();
        let key = input.as_raw() as usize;
        if !self.inputs.contains_key(&key) {
            let idesc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut view = None;
            unsafe { self.vdev.CreateVideoProcessorInputView(input, en, &idesc, Some(&mut view))? };
            self.inputs.insert(key, view.unwrap());
        }
        let in_view = self.inputs[&key].clone();
        let src = RECT {
            left: 0,
            top: 0,
            right: src.right.min(desc.Width as i32),
            bottom: src.bottom.min(desc.Height as i32),
        };
        let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
        let (out_tex, out_view) = self.outputs[self.next].clone();
        self.next = (self.next + 1) % self.outputs.len();
        unsafe {
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
            self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            };
            let r = self.vctx.VideoProcessorBlt(vp, &out_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r?;
        }
        Ok(out_tex)
    }
}

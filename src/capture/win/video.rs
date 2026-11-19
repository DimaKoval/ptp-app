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
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, format, 3, pool_size)?;
    let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    // Windows 11 24H2+ may otherwise throttle frame delivery (~60 fps);
    // ask for frames as often as the display produces them.
    let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 }); // 1 ms
    if borderless_allowed() {
        let _ = session.SetIsBorderRequired(false); // Windows 11+; older systems keep the border
    }
    let (frame_tx, frame_rx) = mpsc::channel::<()>();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        let _ = frame_tx.send(());
        Ok(())
    }))?;

    let (mut enc_w, mut enc_h) = fit_size(pool_size.Width as u32, pool_size.Height as u32, cfg.max_width, cfg.max_height);
    let mut vp = Converter::new(&device, &context, cfg.fps)?;
    let mut encoder = Encoder::new(&device, &context, cfg.codec, enc_w, enc_h, cfg.fps, cfg.bitrate_kbps)?;
    let mut bitrate = cfg.bitrate_kbps;
    session.StartCapture()?;
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok((enc_w, enc_h)));
    }

    let mut out: Vec<Encoded> = Vec::new();
    // Our own copy of the newest frame: re-sent while the screen is static,
    // and measured for black bars (a paused video sends no new frames).
    let mut held: Option<(ID3D11Texture2D, u32, u32)> = None;
    let mut lb = super::letterbox::Letterbox::new(cfg.kind, cfg.id);
    let mut last_encode = Instant::now();
    let mut last_pts = 0i64;
    let mut cand: Option<((u32, u32), Instant)> = None;
    // Frame-rate cap: Windows delivers every frame the screen shows (a game
    // at 240 fps would be encoded 240 times a second). Encode at most one
    // frame per slot of 1/fps, always the newest; a frame that arrives early
    // waits for its slot (under one frame time).
    let interval = Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64);
    let mut next_due = Instant::now();
    let mut pending_pts: Option<i64> = None;

    while !shared.stop.load(Ordering::SeqCst) {
        let wait = match pending_pts {
            Some(_) => next_due.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)),
            None => Duration::from_millis(20),
        };
        let _ = frame_rx.recv_timeout(wait);
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
                pool.Recreate(&d3d, format, 3, pool_size)?;
                vp.forget_inputs();
            }
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let w = (content.Width.max(1) as u32).min(desc.Width);
            let h = (content.Height.max(1) as u32).min(desc.Height);
            if held.as_ref().map(|x| (x.1, x.2)) != Some((w, h)) {
                let copy_desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    // Same binding as the capture frames, which the video
                    // processor already accepts as input.
                    BindFlags: desc.BindFlags,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                unsafe { device.CreateTexture2D(&copy_desc, None, Some(&mut t))? };
                held = Some((t.unwrap(), w, h));
                vp.forget_inputs();
            }
            let dst = &held.as_ref().unwrap().0;
            let b = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            unsafe { context.CopySubresourceRegion(dst, 0, 0, 0, 0, &tex, 0, Some(&b)) };
            pending_pts = Some(frame.SystemRelativeTime()?.Duration / 10);
            drop(frame); // back to the pool right away
        }
        let Some((src_tex, cw, ch)) = held.clone() else {
            encoder.poll(&mut out)?;
            continue;
        };

        // Black bars (fullscreen only); a crop change applies immediately.
        let lb_changed = lb.update(&device, &context, &src_tex, cw, ch);
        let src = lb.crop().unwrap_or(RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 });
        let target = fit_size((src.right - src.left) as u32, (src.bottom - src.top) as u32, cfg.max_width, cfg.max_height);
        if target != (enc_w, enc_h) {
            match cand {
                _ if lb_changed => {
                    (enc_w, enc_h) = target;
                    encoder = Encoder::new(&device, &context, cfg.codec, enc_w, enc_h, cfg.fps, bitrate)?;
                    shared.force_key.store(true, Ordering::SeqCst);
                    cand = None;
                }
                Some((c, since)) if c == target && since.elapsed() >= Duration::from_millis(300) => {
                    (enc_w, enc_h) = target;
                    encoder = Encoder::new(&device, &context, cfg.codec, enc_w, enc_h, cfg.fps, bitrate)?;
                    shared.force_key.store(true, Ordering::SeqCst);
                    cand = None;
                }
                Some((c, _)) if c == target => {}
                _ => cand = Some((target, Instant::now())),
            }
        } else {
            cand = None;
        }

        // Encode new frames; while the screen is static re-send the last
        // picture now and then, right away when a viewer asks for a
        // keyframe, and when the crop changed.
        let now = Instant::now();
        let idle = now - last_encode;
        let key_wanted = shared.force_key.load(Ordering::SeqCst);
        let fresh_due = pending_pts.is_some() && now >= next_due;
        if fresh_due || lb_changed || idle > Duration::from_millis(500) || (key_wanted && idle > Duration::from_millis(40)) {
            // Next slot one frame time on; if we fell behind by more than a
            // frame, restart the schedule rather than bursting to catch up.
            next_due = (next_due + interval).max(now + interval / 2);
            let nv12 = vp.convert(&src_tex, src, enc_w, enc_h)?;
            // Capture time for new frames, now for re-sends.
            let pts = pending_pts.take().unwrap_or_else(qpc_us).max(last_pts + 1000);
            last_pts = pts;
            let key = shared.force_key.swap(false, Ordering::SeqCst);
            encoder.encode(&nv12, pts, key, &mut out)?;
            last_encode = Instant::now();
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
            left: src.left.max(0),
            top: src.top.max(0),
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

/// The yellow capture border can only be turned off after asking Windows for
/// borderless access (granted automatically to unpackaged desktop apps on
/// Windows 11). Asked once per process; false on systems without the API.
fn borderless_allowed() -> bool {
    use std::sync::OnceLock;
    use windows::Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind};
    use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        let status = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|op| op.join());
        let ok = matches!(status, Ok(AppCapabilityAccessStatus::Allowed));
        if !ok {
            eprintln!("borderless capture not available: {status:?}");
        }
        ok
    })
}

//! Windows backend.
//!
//! * Sources and thumbnails: Win32 window/monitor enumeration + GDI.
//! * Video: Windows.Graphics.Capture → D3D11 video processor (BGRA → NV12,
//!   scaling, BT.709) → Media Foundation H.264 encoder (hardware MFT when
//!   available, Microsoft's software encoder otherwise).
//! * Audio: WASAPI process loopback (system audio minus one app's process
//!   tree, or a single app), plus the default microphone if asked.

mod audio;
mod encoder;
mod sources;
mod video;

use super::*;
use anyhow::{Result, anyhow};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

pub use sources::{list_sources, thumbnail};

/// Windows needs no screen-capture permission.
pub fn has_screen_access() -> bool {
    true
}

pub fn request_screen_access() -> bool {
    true
}

pub fn mic_supported() -> bool {
    true
}

/// What the capture threads need, copied out of `CaptureConfig`.
#[derive(Clone)]
pub(crate) struct Config {
    pub kind: SourceKind,
    pub id: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub audio: AudioCapture,
    pub audio_app: Option<String>,
    pub excluded_apps: Vec<String>,
}

/// State shared between the `Capture` handle and its threads.
pub(crate) struct Shared {
    pub stop: AtomicBool,
    pub force_key: AtomicBool,
    pub bitrate_kbps: AtomicU32,
    pub callbacks: Callbacks,
}

/// A running capture. Dropping it stops capture; no callbacks fire afterwards.
pub struct Capture {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    pub width: u32,
    pub height: u32,
}

impl Capture {
    pub fn start(cfg: &CaptureConfig, callbacks: Callbacks) -> Result<Capture> {
        if cfg.kind == SourceKind::TestPattern {
            return Err(anyhow!("The test pattern is only available on macOS"));
        }
        let config = Config {
            kind: cfg.kind,
            id: cfg.id,
            max_width: cfg.max_width,
            max_height: cfg.max_height,
            fps: cfg.fps.max(1),
            bitrate_kbps: cfg.bitrate_kbps,
            audio: cfg.audio,
            audio_app: cfg.audio_app.clone(),
            excluded_apps: cfg.excluded_apps.clone(),
        };
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            force_key: AtomicBool::new(true),
            bitrate_kbps: AtomicU32::new(cfg.bitrate_kbps),
            callbacks,
        });

        let mut threads = Vec::new();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(u32, u32), String>>();
        {
            let (config, shared) = (config.clone(), shared.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("ptp-video".into())
                    .spawn(move || video::run(config, shared, ready_tx))?,
            );
        }
        let stop_all = |shared: &Arc<Shared>, threads: Vec<JoinHandle<()>>| {
            shared.stop.store(true, Ordering::SeqCst);
            for t in threads {
                let _ = t.join();
            }
        };
        let (width, height) = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(size)) => size,
            Ok(Err(e)) => {
                stop_all(&shared, threads);
                return Err(anyhow!(e));
            }
            Err(_) => {
                stop_all(&shared, threads);
                return Err(anyhow!("Timed out starting capture"));
            }
        };

        if cfg.audio != AudioCapture::None {
            let (config, shared) = (config.clone(), shared.clone());
            threads.push(std::thread::Builder::new().name("ptp-audio".into()).spawn(move || audio::run_system(config, shared))?);
        }
        if cfg.mic {
            let shared = shared.clone();
            threads.push(std::thread::Builder::new().name("ptp-mic".into()).spawn(move || audio::run_mic(shared))?);
        }
        Ok(Capture { shared, threads, width, height })
    }

    pub fn request_keyframe(&self) {
        self.shared.force_key.store(true, Ordering::SeqCst);
    }

    pub fn set_bitrate(&self, kbps: u32) {
        self.shared.bitrate_kbps.store(kbps, Ordering::SeqCst);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Fit (w, h) inside (max_w, max_h) keeping the aspect ratio, never
/// upscaling, rounded down to even numbers (H.264 4:2:0 needs even sizes).
pub(crate) fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let scale = (max_w as f64 / w).min(max_h as f64 / h).min(1.0);
    let ow = ((w * scale).round() as u32) & !1;
    let oh = ((h * scale).round() as u32) & !1;
    (ow.max(16), oh.max(16))
}

/// Microseconds on the QueryPerformanceCounter clock, the same clock as
/// Windows.Graphics.Capture frame timestamps.
pub(crate) fn qpc_us() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    if f == 0 { 0 } else { (c as i128 * 1_000_000 / f as i128) as i64 }
}

//! macOS backend: safe wrapper around the Objective-C capture + encode layer
//! (`native/macos`, ScreenCaptureKit + VideoToolbox).

use super::*;
use anyhow::{Result, anyhow};
use std::ffi::{CStr, CString, c_char, c_void};

#[repr(C)]
struct RawConfig {
    source_kind: u32,
    source_id: u32,
    max_width: u32,
    max_height: u32,
    fps: u32,
    bitrate_kbps: u32,
    audio_mode: u32,
    audio_app_bundle: *const c_char,
    excluded_bundles: *const c_char,
    capture_mic: u32,
    ctx: *mut c_void,
    on_video: extern "C" fn(*mut c_void, *const u8, usize, i64, i32),
    on_audio: extern "C" fn(*mut c_void, *const f32, usize, u32, i64, i32),
    on_error: extern "C" fn(*mut c_void, *const c_char),
}

unsafe extern "C" {
    fn ptp_list_sources_json() -> *mut c_char;
    fn ptp_free(ptr: *mut c_void);
    fn ptp_thumbnail(kind: u32, id: u32, max_w: u32, max_h: u32, out: *mut *mut u8, w: *mut u32, h: *mut u32) -> i32;
    fn ptp_has_screen_access() -> i32;
    fn ptp_request_screen_access() -> i32;
    fn ptp_mic_supported() -> i32;
    fn ptp_start(cfg: *const RawConfig, w: *mut u32, h: *mut u32, err: *mut c_char, err_len: usize) -> *mut c_void;
    fn ptp_request_keyframe(h: *mut c_void);
    fn ptp_set_bitrate(h: *mut c_void, kbps: u32);
    fn ptp_stop(h: *mut c_void);
}

pub fn list_sources() -> Result<SourceList> {
    unsafe {
        let ptr = ptp_list_sources_json();
        if ptr.is_null() {
            return Err(anyhow!("Could not list screens/windows. Is Screen Recording permission granted?"));
        }
        let json = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        ptp_free(ptr as *mut c_void);
        let value: serde_json::Value = serde_json::from_str(&json)?;
        if let Some(err) = value.get("error") {
            return Err(anyhow!(
                "ScreenCaptureKit: {} (domain {}, code {}, preflight {}, bundle {})",
                err.as_str().unwrap_or("?"),
                value["domain"].as_str().unwrap_or(""),
                value["code"],
                value["preflight"],
                value["bundle"].as_str().unwrap_or("")
            ));
        }
        Ok(serde_json::from_value(value)?)
    }
}

pub fn thumbnail(kind: SourceKind, id: u32, max_w: u32, max_h: u32) -> Option<Thumbnail> {
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let (mut w, mut h) = (0u32, 0u32);
        if ptp_thumbnail(kind.raw(), id, max_w, max_h, &mut out, &mut w, &mut h) != 0 || out.is_null() {
            return None;
        }
        let rgba = std::slice::from_raw_parts(out, (w * h * 4) as usize).to_vec();
        ptp_free(out as *mut c_void);
        Some(Thumbnail { width: w, height: h, rgba })
    }
}

pub fn has_screen_access() -> bool {
    unsafe { ptp_has_screen_access() != 0 }
}

pub fn request_screen_access() -> bool {
    unsafe { ptp_request_screen_access() != 0 }
}

pub fn mic_supported() -> bool {
    unsafe { ptp_mic_supported() != 0 }
}

extern "C" fn on_video(ctx: *mut c_void, data: *const u8, len: usize, pts: i64, key: i32) {
    let cbs = unsafe { &*(ctx as *const Callbacks) };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    (cbs.video)(slice, pts, key != 0);
}

extern "C" fn on_audio(ctx: *mut c_void, data: *const f32, frames: usize, rate: u32, pts: i64, mic: i32) {
    let cbs = unsafe { &*(ctx as *const Callbacks) };
    let slice = unsafe { std::slice::from_raw_parts(data, frames * 2) };
    (cbs.audio)(slice, rate, pts, mic != 0);
}

extern "C" fn on_error(ctx: *mut c_void, msg: *const c_char) {
    let cbs = unsafe { &*(ctx as *const Callbacks) };
    let s = unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned();
    (cbs.error)(s);
}

/// A running capture. Dropping it stops capture; no callbacks fire afterwards.
pub struct Capture {
    handle: *mut c_void,
    ctx: *mut Callbacks,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    pub fn start(cfg: &CaptureConfig, callbacks: Callbacks) -> Result<Capture> {
        let ctx = Box::into_raw(Box::new(callbacks));
        let app = CString::new(cfg.audio_app.clone().unwrap_or_default()).unwrap_or_default();
        let excluded = CString::new(cfg.excluded_apps.join("\n")).unwrap_or_default();
        let raw = RawConfig {
            source_kind: cfg.kind.raw(),
            source_id: cfg.id,
            max_width: cfg.max_width,
            max_height: cfg.max_height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            audio_mode: match cfg.audio {
                AudioCapture::None => 0,
                AudioCapture::SystemExcluding => 1,
                AudioCapture::SingleApp => 2,
            },
            audio_app_bundle: app.as_ptr(),
            excluded_bundles: excluded.as_ptr(),
            capture_mic: cfg.mic as u32,
            ctx: ctx as *mut c_void,
            on_video,
            on_audio,
            on_error,
        };
        let mut err = vec![0 as c_char; 512];
        let (mut w, mut h) = (0u32, 0u32);
        let handle = unsafe { ptp_start(&raw, &mut w, &mut h, err.as_mut_ptr(), err.len()) };
        if handle.is_null() {
            unsafe { drop(Box::from_raw(ctx)) };
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            return Err(anyhow!(if msg.is_empty() { "Failed to start capture".to_string() } else { msg }));
        }
        Ok(Capture { handle, ctx, width: w, height: h })
    }

    pub fn request_keyframe(&self) {
        unsafe { ptp_request_keyframe(self.handle) }
    }

    pub fn set_bitrate(&self, kbps: u32) {
        unsafe { ptp_set_bitrate(self.handle, kbps) }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            ptp_stop(self.handle);
            drop(Box::from_raw(self.ctx));
        }
    }
}

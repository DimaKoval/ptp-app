//! Screen/window capture + H.264 encoding, one backend per OS.
//!
//! Each backend provides `list_sources`, `thumbnail`, `has_screen_access`,
//! `request_screen_access`, `mic_supported` and a `Capture` handle with
//! `start`, `request_keyframe`, `set_bitrate`, `width`/`height`. Dropping
//! the `Capture` stops it; no callbacks fire afterwards.

use serde::Deserialize;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use self::win::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, Deserialize)]
pub enum SourceKind {
    Display,
    Window,
    TestPattern,
}

impl SourceKind {
    #[allow(dead_code)]
    fn raw(self) -> u32 {
        match self {
            SourceKind::Display => 0,
            SourceKind::Window => 1,
            SourceKind::TestPattern => 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub is_main: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub bundle_id: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceList {
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
    pub apps: Vec<AppInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCapture {
    None,
    SystemExcluding,
    SingleApp,
}

#[derive(Clone)]
pub struct CaptureConfig {
    pub kind: SourceKind,
    pub id: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub audio: AudioCapture,
    pub audio_app: Option<String>,
    pub excluded_apps: Vec<String>,
    pub mic: bool,
}

pub struct Callbacks {
    /// Annex-B H.264 access unit, presentation time (µs), keyframe flag.
    pub video: Box<dyn Fn(&[u8], i64, bool) + Send + Sync>,
    /// Interleaved stereo f32, sample rate, pts (µs), is_mic.
    pub audio: Box<dyn Fn(&[f32], u32, i64, bool) + Send + Sync>,
    pub error: Box<dyn Fn(String) + Send + Sync>,
}

pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}


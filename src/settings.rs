//! Persistent user settings (~/Library/Application Support/ptp-share/settings.json).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    P720,
    P1080,
    P1440,
    P2160,
}

/// Widest frame the H.264 hardware encoders accept (VideoToolbox, NVENC,
/// AMF and Quick Sync all stop at 4096).
pub const MAX_ENCODE_WIDTH: u32 = 4096;
/// AV1 hardware encoders go to 8192, so ultrawide screens stay native.
pub const MAX_AV1_ENCODE_WIDTH: u32 = 8192;

impl Resolution {
    pub const ALL: [Resolution; 4] = [Resolution::P720, Resolution::P1080, Resolution::P1440, Resolution::P2160];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "4K",
        }
    }

    /// Box the captured image is scaled down to fit (keeps aspect ratio,
    /// never upscales). The preset sets the height ("1440p" = 1440 lines);
    /// width is free up to the H.264 hardware encoders' limit, so a 32:9
    /// ultrawide isn't squeezed into a 16:9 box (5120×1440 at "4K" used to
    /// come out as 3840×1080; now 4096×1152).
    pub fn bounds(self) -> (u32, u32) {
        let lines = match self {
            Resolution::P720 => 720,
            Resolution::P1080 => 1080,
            Resolution::P1440 => 1440,
            Resolution::P2160 => 2160,
        };
        (MAX_ENCODE_WIDTH, lines)
    }

    /// Reasonable H.264 bitrate for screen content (Mbps) at a given fps.
    pub fn suggested_mbps(self, fps: u32) -> f32 {
        let base = match self {
            Resolution::P720 => 4.0,
            Resolution::P1080 => 8.0,
            Resolution::P1440 => 14.0,
            Resolution::P2160 => 25.0,
        };
        base * match fps {
            0..=30 => 0.7,
            31..=60 => 1.0,
            _ => 1.6,
        }
    }
}

pub const FPS_OPTIONS: [u32; 3] = [30, 60, 120];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioMode {
    Off,
    /// All system audio except the excluded apps (Discord by default).
    System,
    /// Audio of a single app only.
    SingleApp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_mbps: f32,
    pub audio_mode: AudioMode,
    /// Bundle ids excluded from system audio.
    pub excluded_apps: Vec<String>,
    pub audio_app: Option<String>,
    pub capture_mic: bool,
    /// Public URL of the viewer page (GitHub Pages).
    pub viewer_url: String,
    pub relays: Vec<String>,
    pub turn_url: String,
    pub turn_username: String,
    pub turn_credential: String,
    /// Secret behind the share link (base64url). Kept so the link stays the
    /// same between sessions until the user makes a new one.
    pub room_secret: String,
}

/// App id of Discord: a bundle id on macOS, the executable name on Windows.
#[cfg(target_os = "macos")]
pub const DISCORD: &str = "com.hnc.Discord";
#[cfg(not(target_os = "macos"))]
pub const DISCORD: &str = "discord.exe";

impl Default for Settings {
    fn default() -> Self {
        Settings {
            resolution: Resolution::P1440,
            fps: 60,
            bitrate_mbps: Resolution::P1440.suggested_mbps(60),
            audio_mode: AudioMode::System,
            excluded_apps: vec![DISCORD.into()],
            audio_app: None,
            capture_mic: false,
            viewer_url: DEFAULT_VIEWER_URL.into(),
            relays: crate::signaling::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            turn_url: String::new(),
            turn_username: String::new(),
            turn_credential: String::new(),
            room_secret: String::new(),
        }
    }
}

/// Public viewer page, deployed from the ptp-web repo.
pub const DEFAULT_VIEWER_URL: &str = "https://dimakoval.github.io/ptp-web/";
/// Old default from before the viewer was on GitHub Pages.
const OLD_DEFAULT_VIEWER_URL: &str = "http://localhost:8080/";

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ptp-share").join("settings.json"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s: Settings = path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if s.viewer_url.trim().is_empty() || s.viewer_url.trim() == OLD_DEFAULT_VIEWER_URL {
            s.viewer_url = DEFAULT_VIEWER_URL.into();
        }
        s
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap());
        }
    }
}

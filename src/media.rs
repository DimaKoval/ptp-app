use bytes::Bytes;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
}

/// One encoded unit (H.264 access unit or Opus frame), shared by all viewers.
#[derive(Debug, Clone)]
pub struct MediaPacket {
    pub kind: MediaKind,
    pub data: Bytes,
    pub duration: Duration,
    pub keyframe: bool,
}

#[derive(Default)]
pub struct Stats {
    pub video_bytes: AtomicU64,
    pub video_frames: AtomicU64,
    pub keyframes: AtomicU64,
    pub audio_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    /// PCM chunks dropped because the encoder thread fell behind.
    pub audio_pcm_dropped: AtomicU64,
}

//! A live hosting session: capture → encode once → fan out to every viewer's
//! own WebRTC peer connection. Signaling runs over Nostr relays.

use crate::capture::{AudioCapture, Callbacks, Capture, CaptureConfig, SourceKind, VideoCodec};
use crate::media::{MediaKind, MediaPacket, Stats};
use crate::settings::{AudioMode, MAX_AV1_ENCODE_WIDTH, MAX_ENCODE_WIDTH, Settings};
use crate::signaling::{IceServer, Incoming, RelayStatus, Room, SignalMessage, Signaling};
use crate::{audio, rtcp_forward::RtcpForwarder};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use rtc::interceptor::{Registry, Slot};
use rtc::media::Sample;
use rtc::rtp::packetizer::Payloader;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::Runtime;

const MAX_VIEWERS: usize = 8;
const DEFAULT_STUN: &[&str] = &["stun:stun.l.google.com:19302", "stun:stun.cloudflare.com:3478"];

#[derive(Debug, Clone, PartialEq)]
pub enum ViewerState {
    Negotiating,
    Connecting,
    Connected,
}

#[derive(Debug, Clone)]
pub struct ViewerInfo {
    pub sid: String,
    pub label: String,
    pub state: ViewerState,
    pub since: Instant,
    /// Can receive AV1 (said so in its hello, and accepted it in its answer).
    pub av1: bool,
}

#[derive(Debug, Clone)]
pub struct SourceSelection {
    pub kind: SourceKind,
    pub id: u32,
    pub label: String,
}

/// What the viewer is currently getting, for display.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub source_label: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: VideoCodec,
    /// Why that codec, e.g. "Chrome on macOS can't decode AV1".
    pub codec_reason: String,
}

struct PeerHandle {
    pubkey: String,
    answer_tx: mpsc::UnboundedSender<String>,
    kick_tx: watch::Sender<bool>,
    offer: Arc<Mutex<Option<SignalMessage>>>,
}

type MakeCallbacks = Arc<dyn Fn(u64, u32, VideoCodec) -> Callbacks + Send + Sync>;

struct Shared {
    capture: Mutex<Option<Capture>>,
    /// Generation of the capture whose output is forwarded; output of any
    /// other (old or still-starting) capture is dropped.
    capture_gen: Arc<AtomicU64>,
    /// Config of the running capture; restarts reuse it.
    capture_cfg: Mutex<CaptureConfig>,
    /// Serializes capture swaps (live settings changes vs. the watchdog).
    swap_lock: Mutex<()>,
    make_callbacks: MakeCallbacks,
    has_system_audio: Arc<AtomicBool>,
    media_tx: broadcast::Sender<MediaPacket>,
    signaling: Arc<Signaling>,
    viewers: Mutex<BTreeMap<String, ViewerInfo>>,
    peers: Mutex<HashMap<String, PeerHandle>>,
    shutdown: watch::Receiver<bool>,
    ice_servers: Vec<IceServer>,
    info: Mutex<VideoInfo>,
    last_kf_request: Mutex<Instant>,
    error: Mutex<Option<String>>,
    /// Current encoder bitrate, reused when capture restarts.
    bitrate_kbps: AtomicU32,
    /// Codec every capture (re)start uses: AV1 while every viewer can take it.
    wanted_codec: Mutex<VideoCodec>,
    /// The AV1 encoder failed to start once; stay on H.264.
    av1_failed: AtomicBool,
    codec_check_pending: AtomicBool,
}

impl Shared {
    fn request_keyframe(&self) {
        let mut last = self.last_kf_request.lock().unwrap();
        if last.elapsed() < Duration::from_millis(250) {
            return;
        }
        *last = Instant::now();
        if let Some(c) = self.capture.lock().unwrap().as_ref() {
            c.request_keyframe();
        }
    }

    fn set_viewer_state(&self, sid: &str, state: ViewerState) {
        if let Some(v) = self.viewers.lock().unwrap().get_mut(sid) {
            if v.state != state {
                v.state = state;
                v.since = Instant::now();
            }
        }
    }

    /// Starts a capture with `cfg` next to the running one and switches
    /// viewers over to it once it runs, so they never see a gap or disconnect.
    /// Blocking. On failure the running capture is left as it was.
    fn swap_capture(&self, cfg: CaptureConfig, label: Option<String>) -> Result<()> {
        let _guard = self.swap_lock.lock().unwrap();
        let generation = self.capture_gen.load(Ordering::SeqCst) + 1;
        let mut cfg = cfg;
        cfg.bitrate_kbps = self.bitrate_kbps.load(Ordering::Relaxed);
        cfg.codec = *self.wanted_codec.lock().unwrap();
        cfg.max_width = if cfg.codec == VideoCodec::Av1 { MAX_AV1_ENCODE_WIDTH } else { MAX_ENCODE_WIDTH };
        let cap = Capture::start(&cfg, (self.make_callbacks)(generation, cfg.fps, cfg.codec))?;
        let old = {
            let mut slot = self.capture.lock().unwrap();
            if *self.shutdown.borrow() {
                drop(slot);
                drop(cap);
                return Ok(());
            }
            self.capture_gen.store(generation, Ordering::SeqCst);
            // Its first keyframe may have been dropped before the switch.
            cap.request_keyframe();
            {
                let mut info = self.info.lock().unwrap();
                info.width = cap.width;
                info.height = cap.height;
                info.fps = cfg.fps;
                info.codec = cfg.codec;
                if let Some(l) = label {
                    info.source_label = l;
                }
            }
            slot.replace(cap)
        };
        self.has_system_audio.store(cfg.audio != AudioCapture::None, Ordering::Relaxed);
        *self.capture_cfg.lock().unwrap() = cfg;
        *self.error.lock().unwrap() = None;
        drop(old); // stops the old capture (blocking, outside the slot lock)
        Ok(())
    }

    /// Tells watching viewers the stream's size, frame rate or codec changed.
    fn broadcast_info(&self) {
        let info = self.info.lock().unwrap().clone();
        let msg = SignalMessage::Info { width: info.width, height: info.height, fps: info.fps, codec: info.codec.label().into() };
        let pubkeys: Vec<String> = self.peers.lock().unwrap().values().map(|p| p.pubkey.clone()).collect();
        for pk in &pubkeys {
            self.signaling.send(Some(pk), &msg);
        }
    }

    /// The codec the stream should use now, and why. None: keep the current
    /// one (nobody is watching).
    fn desired_codec(&self) -> Option<(VideoCodec, String)> {
        if !crate::capture::av1_supported() {
            return Some((VideoCodec::H264, "this computer has no hardware AV1 encoder".into()));
        }
        if self.av1_failed.load(Ordering::SeqCst) {
            return Some((VideoCodec::H264, "the AV1 encoder failed to start".into()));
        }
        let viewers = self.viewers.lock().unwrap();
        if viewers.is_empty() {
            return None;
        }
        let blockers: Vec<String> = viewers.values().filter(|v| !v.av1).map(|v| v.label.clone()).collect();
        Some(if blockers.is_empty() {
            (VideoCodec::Av1, "every viewer can decode AV1".into())
        } else {
            (VideoCodec::H264, format!("{} can't decode AV1 smoothly", blockers.join(", ")))
        })
    }

    /// Re-picks the codec after viewers came or went, and switches the
    /// encoder (like a live settings change) if needed. Debounced, so a
    /// viewer reconnecting doesn't flip the codec back and forth.
    fn check_codec(self: &Arc<Self>) {
        if self.codec_check_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = self.clone();
        let _ = std::thread::Builder::new().name("codec-switch".into()).spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            shared.codec_check_pending.store(false, Ordering::SeqCst);
            if *shared.shutdown.borrow() {
                return;
            }
            let Some((codec, reason)) = shared.desired_codec() else { return };
            shared.info.lock().unwrap().codec_reason = reason;
            *shared.wanted_codec.lock().unwrap() = codec;
            let cfg = shared.capture_cfg.lock().unwrap().clone();
            if cfg.codec == codec {
                return;
            }
            eprintln!("switching video codec to {}", codec.label());
            if let Err(e) = shared.swap_capture(cfg, None) {
                eprintln!("{} encoder failed: {e}", codec.label());
                if codec == VideoCodec::Av1 {
                    shared.av1_failed.store(true, Ordering::SeqCst);
                    *shared.wanted_codec.lock().unwrap() = VideoCodec::H264;
                    shared.info.lock().unwrap().codec_reason = "the AV1 encoder failed to start".into();
                }
                return;
            }
            shared.broadcast_info();
        });
    }
}

pub struct HostSession {
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    pub link: String,
    pub stats: Arc<Stats>,
    pub started: Instant,
    rt: tokio::runtime::Handle,
}

fn webrtc_runtime() -> Arc<dyn Runtime> {
    static RT: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    RT.get_or_init(|| webrtc::runtime::default_runtime().expect("tokio runtime feature")).clone()
}

/// Capture settings for a source, taken from the user's settings.
pub fn capture_config(source: &SourceSelection, settings: &Settings) -> Result<CaptureConfig> {
    let (max_w, max_h) = settings.resolution.bounds();
    let audio = match settings.audio_mode {
        AudioMode::Off => AudioCapture::None,
        AudioMode::System => AudioCapture::SystemExcluding,
        AudioMode::SingleApp => AudioCapture::SingleApp,
    };
    if audio == AudioCapture::SingleApp && settings.audio_app.is_none() {
        return Err(anyhow!("Pick an app to capture audio from"));
    }
    Ok(CaptureConfig {
        kind: source.kind,
        id: source.id,
        max_width: max_w,
        max_height: max_h,
        fps: settings.fps,
        bitrate_kbps: (settings.bitrate_mbps * 1000.0) as u32,
        audio,
        audio_app: settings.audio_app.clone(),
        excluded_apps: settings.excluded_apps.clone(),
        mic: settings.capture_mic && crate::capture::mic_supported(),
        // Picked per viewer set; swap_capture fills in the current choice.
        codec: VideoCodec::H264,
    })
}

impl HostSession {
    /// Starts capture, connects to relays and begins accepting viewers.
    /// `room` decides the link; reuse it to keep the same link across sessions.
    /// Blocking (capture start waits on macOS); call off the UI thread.
    pub fn start(source: SourceSelection, settings: &Settings, room: Room, rt: tokio::runtime::Handle) -> Result<HostSession> {
        let _guard = rt.enter();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (media_tx, _) = broadcast::channel::<MediaPacket>(512);
        let stats = Arc::new(Stats::default());
        let capture_cfg = capture_config(&source, settings)?;

        // Audio is always negotiated, so it can be switched on while live.
        let has_system_audio = Arc::new(AtomicBool::new(capture_cfg.audio != AudioCapture::None));
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<audio::Pcm>(64);
        audio::spawn_encoder(pcm_rx, media_tx.clone(), has_system_audio.clone(), stats.clone());

        let error_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let capture_gen = Arc::new(AtomicU64::new(0));
        // Builds the callbacks for one capture generation. Output is forwarded
        // only while that generation is current.
        let make_callbacks: MakeCallbacks = {
            let video_tx = media_tx.clone();
            let stats = stats.clone();
            let err_slot = error_slot.clone();
            let current = capture_gen.clone();
            Arc::new(move |generation: u64, fps: u32, codec: VideoCodec| {
                let last_pts = Mutex::new(None::<i64>);
                let frame_us = 1_000_000 / fps.max(1) as i64;
                let (video_tx, video_stats, audio_stats, pcm_tx, err_slot) =
                    (video_tx.clone(), stats.clone(), stats.clone(), pcm_tx.clone(), err_slot.clone());
                let (cur_v, cur_a, cur_e) = (current.clone(), current.clone(), current.clone());
                Callbacks {
                    video: Box::new(move |data, pts, key| {
                        if cur_v.load(Ordering::SeqCst) != generation {
                            return;
                        }
                        let mut last = last_pts.lock().unwrap();
                        let dur_us = match *last {
                            Some(prev) => (pts - prev).clamp(1_000, 1_000_000),
                            None => frame_us,
                        };
                        *last = Some(pts);
                        video_stats.video_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        video_stats.video_frames.fetch_add(1, Ordering::Relaxed);
                        if key {
                            video_stats.keyframes.fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = video_tx.send(MediaPacket {
                            kind: MediaKind::Video,
                            data: Bytes::copy_from_slice(data),
                            duration: Duration::from_micros(dur_us as u64),
                            keyframe: key,
                            pts_us: pts,
                            codec,
                        });
                    }),
                    audio: Box::new(move |samples, rate, _pts, mic| {
                        if cur_a.load(Ordering::SeqCst) != generation {
                            return;
                        }
                        if pcm_tx.try_send(audio::Pcm { samples: samples.to_vec(), rate, mic }).is_err() {
                            audio_stats.audio_pcm_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }),
                    error: Box::new(move |msg| {
                        eprintln!("capture error (generation {generation}): {msg}");
                        if cur_e.load(Ordering::SeqCst) == generation {
                            *err_slot.lock().unwrap() = Some(msg);
                        }
                    }),
                }
            })
        };

        // Starts on H.264 (every browser plays it); switches to AV1 once
        // viewers that can all decode it are watching.
        let capture = Capture::start(&capture_cfg, make_callbacks(0, capture_cfg.fps, capture_cfg.codec))?;
        let codec_reason = if crate::capture::av1_supported() {
            "AV1 is used once every viewer can decode it".to_string()
        } else {
            "this computer has no hardware AV1 encoder".to_string()
        };
        let info = VideoInfo {
            source_label: source.label,
            width: capture.width,
            height: capture.height,
            fps: capture_cfg.fps,
            codec: capture_cfg.codec,
            codec_reason,
        };

        let mut ice_servers: Vec<IceServer> = vec![IceServer {
            urls: DEFAULT_STUN.iter().map(|s| s.to_string()).collect(),
            username: String::new(),
            credential: String::new(),
        }];
        if !settings.turn_url.trim().is_empty() {
            ice_servers.push(IceServer {
                urls: settings
                    .turn_url
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                username: settings.turn_username.trim().to_string(),
                credential: settings.turn_credential.trim().to_string(),
            });
        }

        let relays: Vec<String> = settings.relays.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let link = room.link(settings.viewer_url.trim(), &relays);
        let (signaling, incoming) = Signaling::start(room, relays, shutdown_rx.clone());

        let shared = Arc::new(Shared {
            capture: Mutex::new(Some(capture)),
            capture_gen,
            capture_cfg: Mutex::new(capture_cfg),
            swap_lock: Mutex::new(()),
            make_callbacks,
            has_system_audio,
            media_tx,
            signaling,
            viewers: Mutex::new(BTreeMap::new()),
            peers: Mutex::new(HashMap::new()),
            shutdown: shutdown_rx,
            ice_servers,
            info: Mutex::new(info),
            last_kf_request: Mutex::new(Instant::now() - Duration::from_secs(1)),
            error: Mutex::new(None),
            bitrate_kbps: AtomicU32::new((settings.bitrate_mbps * 1000.0) as u32),
            wanted_codec: Mutex::new(VideoCodec::H264),
            av1_failed: AtomicBool::new(false),
            codec_check_pending: AtomicBool::new(false),
        });
        spawn_capture_watchdog(shared.clone(), error_slot);
        tokio::spawn(dispatch(shared.clone(), incoming));

        Ok(HostSession { shared, shutdown_tx, link, stats, started: Instant::now(), rt: rt.clone() })
    }

    pub fn info(&self) -> VideoInfo {
        self.shared.info.lock().unwrap().clone()
    }

    /// A handle for changing settings from another thread while live.
    pub fn control(&self) -> Control {
        Control(self.shared.clone())
    }

    pub fn viewers(&self) -> Vec<ViewerInfo> {
        self.shared.viewers.lock().unwrap().values().cloned().collect()
    }

    pub fn relay_status(&self) -> Vec<RelayStatus> {
        self.shared.signaling.status.lock().unwrap().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn set_bitrate(&self, kbps: u32) {
        self.shared.bitrate_kbps.store(kbps, Ordering::Relaxed);
        if let Some(c) = self.shared.capture.lock().unwrap().as_ref() {
            c.set_bitrate(kbps);
        }
    }

    pub fn kick(&self, sid: &str) {
        if let Some(p) = self.shared.peers.lock().unwrap().get(sid) {
            let _ = p.kick_tx.send(true);
            self.shared.signaling.send(Some(&p.pubkey), &SignalMessage::Bye { sid: sid.to_string() });
        }
    }

    /// Tells viewers the stream ended, then tears everything down. Blocking.
    pub fn stop(self) {
        let pubkeys: Vec<String> = self.shared.peers.lock().unwrap().values().map(|p| p.pubkey.clone()).collect();
        for pk in &pubkeys {
            self.shared.signaling.send(Some(pk), &SignalMessage::Ended);
        }
        if !pubkeys.is_empty() {
            std::thread::sleep(Duration::from_millis(400)); // let relays flush
        }
        let _ = self.shutdown_tx.send(true);
        let _guard = self.shared.swap_lock.lock().unwrap();
        drop(self.shared.capture.lock().unwrap().take());
        let _ = self.rt;
    }
}

#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl Control {
    /// Applies new capture settings (resolution, fps, audio, or a different
    /// source) while live: viewers stay connected. Blocking.
    pub fn reconfigure(&self, source: Option<SourceSelection>, settings: &Settings) -> Result<()> {
        let current = self.0.capture_cfg.lock().unwrap().clone();
        let src = source.clone().unwrap_or(SourceSelection {
            kind: current.kind,
            id: current.id,
            label: self.0.info.lock().unwrap().source_label.clone(),
        });
        let cfg = capture_config(&src, settings)?;
        self.0.swap_capture(cfg, source.map(|s| s.label))?;
        // Let watching viewers update their "1440p · 60 fps" label.
        self.0.broadcast_info();
        Ok(())
    }

}

/// Watches for capture dying and restarts it while the session stays live.
/// Viewers keep their connection; the picture freezes until capture is back,
/// and the restarted encoder opens with a keyframe.
fn spawn_capture_watchdog(shared: Arc<Shared>, err_slot: Arc<Mutex<Option<String>>>) {
    // Test hook: PTP_TEST_CAPTURE_LOSS=<secs> fakes a capture failure.
    let fake_loss = std::env::var("PTP_TEST_CAPTURE_LOSS").ok().and_then(|v| v.parse::<u64>().ok());
    let started = Instant::now();
    let _ = std::thread::Builder::new().name("capture-watchdog".into()).spawn(move || {
        let shutdown = shared.shutdown.clone();
        let mut lost = false;
        let mut faked = false;
        let mut next_try = Instant::now();
        let mut attempts = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if *shutdown.borrow() {
                return;
            }
            if let Some(secs) = fake_loss {
                if !faked && started.elapsed() >= Duration::from_secs(secs) {
                    faked = true;
                    *err_slot.lock().unwrap() = Some("Capture stopped: simulated loss".into());
                }
            }
            if let Some(e) = err_slot.lock().unwrap().take() {
                if !lost {
                    eprintln!("capture lost ({e}); will restart");
                    lost = true;
                    attempts = 0;
                    next_try = Instant::now() + Duration::from_millis(500);
                    *shared.error.lock().unwrap() = Some(format!("{e}. Reconnecting automatically…"));
                }
            }
            if !lost || Instant::now() < next_try {
                continue;
            }
            attempts += 1;
            let cfg = shared.capture_cfg.lock().unwrap().clone();
            match shared.swap_capture(cfg, None) {
                Ok(()) => {
                    eprintln!("capture restored after {attempts} attempt(s)");
                    lost = false;
                    // Errors from the dead stream that raced in are stale.
                    err_slot.lock().unwrap().take();
                }
                Err(e) => {
                    // Quick retries first, then settle at every 3 s.
                    let wait = if attempts < 5 { 1000 } else { 3000 };
                    next_try = Instant::now() + Duration::from_millis(wait);
                    *shared.error.lock().unwrap() =
                        Some(format!("Capture lost: {e}. Retrying — you're still live, viewers stay connected."));
                }
            }
        }
    });
}

async fn dispatch(shared: Arc<Shared>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    let mut shutdown = shared.shutdown.clone();
    // Viewer sessions known to be over. With a reused link, relays replay the
    // last minute of messages on start, including hellos from viewers of the
    // previous session; their answer or bye marks them as gone.
    let mut dead: std::collections::HashSet<String> = Default::default();
    let drop_ghost = |sid: &str, dead: &mut std::collections::HashSet<String>| {
        dead.insert(sid.to_string());
        if let Some(p) = shared.peers.lock().unwrap().get(sid) {
            let _ = p.kick_tx.send(true);
        }
    };
    loop {
        let msg = tokio::select! {
            m = incoming.recv() => match m { Some(m) => m, None => return },
            _ = shutdown.changed() => return,
        };
        match msg.msg {
            SignalMessage::Hello { sid, label, av1 } => {
                if dead.contains(&sid) {
                    continue;
                }
                let existing_offer = {
                    let peers = shared.peers.lock().unwrap();
                    match peers.get(&sid) {
                        Some(p) if p.pubkey == msg.from_pubkey => Some(p.offer.lock().unwrap().clone()),
                        Some(_) => continue,
                        None if peers.len() >= MAX_VIEWERS => continue,
                        None => None,
                    }
                };
                match existing_offer {
                    // Viewer retried; resend the offer if we have one.
                    Some(Some(offer)) => shared.signaling.send(Some(&msg.from_pubkey), &offer),
                    Some(None) => {}
                    None => {
                        let (answer_tx, answer_rx) = mpsc::unbounded_channel();
                        let (kick_tx, kick_rx) = watch::channel(false);
                        let offer = Arc::new(Mutex::new(None));
                        shared.peers.lock().unwrap().insert(
                            sid.clone(),
                            PeerHandle { pubkey: msg.from_pubkey.clone(), answer_tx, kick_tx, offer: offer.clone() },
                        );
                        let label = if label.trim().is_empty() { "Viewer".to_string() } else { label.chars().take(40).collect() };
                        shared.viewers.lock().unwrap().insert(
                            sid.clone(),
                            ViewerInfo {
                                sid: sid.clone(),
                                label,
                                state: ViewerState::Negotiating,
                                since: Instant::now(),
                                av1: av1 && crate::capture::av1_supported(),
                            },
                        );
                        // A viewer without AV1 switches the stream to H.264
                        // while it is still negotiating.
                        shared.check_codec();
                        let shared2 = shared.clone();
                        let pubkey = msg.from_pubkey.clone();
                        tokio::spawn(async move {
                            let r = run_peer(shared2.clone(), sid.clone(), pubkey, offer, answer_rx, kick_rx).await;
                            if let Err(e) = r {
                                eprintln!("viewer {sid}: {e}");
                            }
                            shared2.peers.lock().unwrap().remove(&sid);
                            shared2.viewers.lock().unwrap().remove(&sid);
                            shared2.check_codec();
                        });
                    }
                }
            }
            SignalMessage::Answer { sid, sdp } => {
                let ours = {
                    let peers = shared.peers.lock().unwrap();
                    match peers.get(&sid) {
                        Some(p) if p.pubkey == msg.from_pubkey => {
                            // An answer before we sent any offer is a replay
                            // from a previous session.
                            if p.offer.lock().unwrap().is_some() {
                                let _ = p.answer_tx.send(sdp);
                                true
                            } else {
                                false
                            }
                        }
                        Some(_) => true,
                        None => false,
                    }
                };
                if !ours {
                    drop_ghost(&sid, &mut dead);
                }
            }
            SignalMessage::Bye { sid } => {
                let theirs = shared.peers.lock().unwrap().get(&sid).map(|p| p.pubkey == msg.from_pubkey);
                if theirs != Some(false) {
                    drop_ghost(&sid, &mut dead);
                }
            }
            SignalMessage::Offer { .. } | SignalMessage::Ended | SignalMessage::Info { .. } => {}
        }
    }
}

struct PeerEvents {
    gathered: mpsc::UnboundedSender<()>,
    state: mpsc::UnboundedSender<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerEvents {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state.send(state);
    }
}

const AV1_PT: u8 = 45;

fn av1_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_AV1.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "profile=0".to_owned(),
            rtcp_feedback: h264_feedback(),
        },
        payload_type: AV1_PT,
        ..Default::default()
    }
}

fn h264_feedback() -> Vec<RTCPFeedback> {
    [("goog-remb", ""), ("ccm", "fir"), ("nack", ""), ("nack", "pli")]
        .iter()
        .map(|(t, p)| RTCPFeedback { typ: t.to_string(), parameter: p.to_string() })
        .collect()
}

fn video_codecs() -> Vec<RTCRtpCodecParameters> {
    // VideoToolbox produces (constrained) High without B-frames. Browsers differ in
    // which H.264 profile ids they advertise (Chrome: 64001f, Safari: 640c1f, all:
    // 42e01f), and every one of their decoders handles this bitstream, so offer all.
    [(102u8, "64001f"), (104u8, "640c1f"), (106u8, "42e01f")]
        .iter()
        .map(|(pt, profile)| RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"),
                rtcp_feedback: h264_feedback(),
            },
            payload_type: *pt,
            ..Default::default()
        })
        .collect()
}

fn audio_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

fn make_track(kind: RtpCodecKind, codec: RTCRtpCodec) -> Result<Arc<TrackLocalStaticSample>> {
    let ssrc = rand::random::<u32>();
    Ok(Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "ptp-share".to_string(),
            format!("ptp-{kind}"),
            format!("ptp-{kind}"),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                codec,
                ..Default::default()
            }],
        ),
    )?))
}

/// Negotiated payload types for each video codec the viewer accepted.
async fn negotiated_video(sender: &Arc<dyn RtpSender>) -> Result<(Option<u8>, Option<u8>)> {
    let codecs = sender.get_parameters().await?.rtp_parameters.codecs;
    let pt = |mime: &str| codecs.iter().find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(mime)).map(|c| c.payload_type);
    Ok((pt(MIME_TYPE_H264), pt(MIME_TYPE_AV1)))
}

/// Turns encoded frames into RTP packets for one viewer. One SSRC, sequence
/// and timestamp across codec switches: the payload type tells the browser
/// which decoder a frame is for (switches always start at a keyframe).
struct VideoRtp {
    ssrc: u32,
    seq: u16,
    ts_base: u32,
    h264: Option<(u8, Box<dyn Payloader>)>,
    av1: Option<(u8, Box<dyn Payloader>)>,
}

impl VideoRtp {
    fn new(ssrc: u32, h264_pt: Option<u8>, av1_pt: Option<u8>) -> VideoRtp {
        VideoRtp {
            ssrc,
            seq: rand::random(),
            ts_base: rand::random(),
            h264: h264_pt.map(|pt| (pt, Box::<rtc::rtp::codec::h264::H264Payloader>::default() as Box<dyn Payloader>)),
            av1: av1_pt.map(|pt| (pt, Box::<rtc::rtp::codec::av1::Av1Payloader>::default() as Box<dyn Payloader>)),
        }
    }

    fn accepts(&self, codec: VideoCodec) -> bool {
        match codec {
            VideoCodec::H264 => self.h264.is_some(),
            VideoCodec::Av1 => self.av1.is_some(),
        }
    }

    fn packetize(&mut self, codec: VideoCodec, ticks: u64, data: &Bytes) -> Result<Vec<rtc::rtp::Packet>> {
        let (pt, payloader) = match codec {
            VideoCodec::H264 => self.h264.as_mut(),
            VideoCodec::Av1 => self.av1.as_mut(),
        }
        .ok_or_else(|| anyhow!("{} not negotiated", codec.label()))?;
        let payloads = payloader.payload(1200 - 12, data)?;
        let n = payloads.len();
        let timestamp = self.ts_base.wrapping_add(ticks as u32);
        let mut out = Vec::with_capacity(n);
        for (i, payload) in payloads.into_iter().enumerate() {
            self.seq = self.seq.wrapping_add(1);
            out.push(rtc::rtp::Packet {
                header: rtc::rtp::Header {
                    version: 2,
                    marker: i == n - 1,
                    payload_type: *pt,
                    sequence_number: self.seq,
                    timestamp,
                    ssrc: self.ssrc,
                    ..Default::default()
                },
                payload,
            });
        }
        Ok(out)
    }
}

async fn negotiated(sender: &Arc<dyn RtpSender>, track: &Arc<TrackLocalStaticSample>) -> Result<(u32, u8)> {
    let pt = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow!("no negotiated codec"))?;
    let ssrc = *track.ssrcs().await.first().ok_or_else(|| anyhow!("track has no ssrc"))?;
    Ok((ssrc, pt))
}

async fn run_peer(
    shared: Arc<Shared>,
    sid: String,
    pubkey: String,
    offer_slot: Arc<Mutex<Option<SignalMessage>>>,
    mut answer_rx: mpsc::UnboundedReceiver<String>,
    mut kick_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut shutdown = shared.shutdown.clone();

    let mut media_engine = MediaEngine::default();
    for c in video_codecs() {
        media_engine.register_codec(c, RtpCodecKind::Video)?;
    }
    media_engine.register_codec(av1_codec(), RtpCodecKind::Video)?;
    {
        media_engine.register_codec(audio_codec(), RtpCodecKind::Audio)?;
    }
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?
        .with(Slot::from(14_000), RtcpForwarder::default());

    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(
            shared
                .ice_servers
                .iter()
                .map(|s| RTCIceServer { urls: s.urls.clone(), username: s.username.clone(), credential: s.credential.clone() })
                .collect(),
        )
        .build();

    let (gathered_tx, mut gathered_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(PeerEvents { gathered: gathered_tx, state: state_tx }))
        .with_runtime(webrtc_runtime())
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await?;

    let result = drive_peer(
        &shared,
        &pc,
        &sid,
        &pubkey,
        &offer_slot,
        &mut answer_rx,
        &mut kick_rx,
        &mut shutdown,
        &mut gathered_rx,
        &mut state_rx,
    )
    .await;
    let _ = pc.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn drive_peer(
    shared: &Arc<Shared>,
    pc: &impl PeerConnection,
    sid: &str,
    pubkey: &str,
    offer_slot: &Arc<Mutex<Option<SignalMessage>>>,
    answer_rx: &mut mpsc::UnboundedReceiver<String>,
    kick_rx: &mut watch::Receiver<bool>,
    shutdown: &mut watch::Receiver<bool>,
    gathered_rx: &mut mpsc::UnboundedReceiver<()>,
    state_rx: &mut mpsc::UnboundedReceiver<RTCPeerConnectionState>,
) -> Result<()> {
    let video_track = make_track(RtpCodecKind::Video, video_codecs()[0].rtp_codec.clone())?;
    let video_sender = pc.add_track(video_track.clone() as Arc<dyn TrackLocal>).await?;
    // add_track only offers the track's own codec; offer every H.264 profile
    // id, plus AV1 first to viewers that can decode it.
    let offer_av1 = shared.viewers.lock().unwrap().get(sid).is_some_and(|v| v.av1);
    let mut offered = video_codecs();
    if offer_av1 {
        offered.insert(0, av1_codec());
    }
    for t in pc.get_transceivers().await {
        if let Ok(Some(s)) = t.sender().await {
            if s.id() == video_sender.id() {
                t.set_codec_preferences(offered.clone()).await?;
            }
        }
    }
    // Always offered, so audio can be switched on while live.
    let audio = {
        let t = make_track(RtpCodecKind::Audio, audio_codec().rtp_codec)?;
        let s = pc.add_track(t.clone() as Arc<dyn TrackLocal>).await?;
        Some((t, s))
    };

    // Non-trickle ICE: gather everything, then send one offer.
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), gathered_rx.recv()).await;
    let local = pc.local_description().await.ok_or_else(|| anyhow!("no local description"))?;
    let info = shared.info.lock().unwrap().clone();
    let offer_msg = SignalMessage::Offer {
        sid: sid.to_string(),
        sdp: local.sdp.clone(),
        ice_servers: shared.ice_servers.clone(),
        width: info.width,
        height: info.height,
        fps: info.fps,
        codec: info.codec.label().into(),
    };
    *offer_slot.lock().unwrap() = Some(offer_msg.clone());
    shared.signaling.send(Some(pubkey), &offer_msg);
    shared.set_viewer_state(sid, ViewerState::Connecting);

    // Wait for the answer, re-sending the offer in case a relay dropped it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut resend = tokio::time::interval(Duration::from_secs(4));
    resend.tick().await;
    let answer = loop {
        tokio::select! {
            a = answer_rx.recv() => break a.ok_or_else(|| anyhow!("closed"))?,
            _ = resend.tick() => shared.signaling.send(Some(pubkey), &offer_msg),
            _ = tokio::time::sleep_until(deadline) => return Err(anyhow!("viewer never answered")),
            _ = kick_rx.changed() => return Ok(()),
            _ = shutdown.changed() => return Ok(()),
        }
    };
    pc.set_remote_description(RTCSessionDescription::answer(answer)?).await?;

    // Wait for the connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            s = state_rx.recv() => match s {
                Some(RTCPeerConnectionState::Connected) => break,
                Some(RTCPeerConnectionState::Failed) | Some(RTCPeerConnectionState::Closed) | None =>
                    return Err(anyhow!("connection failed (NAT traversal? try a TURN server)")),
                _ => {}
            },
            _ = tokio::time::sleep_until(deadline) => return Err(anyhow!("connection timed out")),
            _ = kick_rx.changed() => return Ok(()),
            _ = shutdown.changed() => return Ok(()),
        }
    }
    shared.set_viewer_state(sid, ViewerState::Connected);

    let vssrc = *video_track.ssrcs().await.first().ok_or_else(|| anyhow!("track has no ssrc"))?;
    let (h264_pt, av1_pt) = negotiated_video(&video_sender).await?;
    if av1_pt.is_none() && offer_av1 {
        // Offered, but the browser's answer left it out.
        if let Some(v) = shared.viewers.lock().unwrap().get_mut(sid) {
            v.av1 = false;
        }
        shared.check_codec();
    }
    let audio = match audio {
        Some((t, s)) => {
            let (ssrc, pt) = negotiated(&s, &t).await?;
            Some((t, ssrc, pt))
        }
        None => None,
    };

    // Keyframe requests (PLI/FIR) from this viewer.
    {
        let shared = shared.clone();
        let track = video_track.clone();
        tokio::spawn(async move {
            while let Some(evt) = track.poll().await {
                if let TrackLocalEvent::OnRtcpPacket(_) = evt {
                    shared.request_keyframe();
                }
            }
        });
    }

    // Video is packetized here rather than with write_sample: the sample API
    // stamps each frame with the sum of the *previous* frames' durations, so
    // unevenly spaced frames (a 24/30 fps video on a 60 Hz screen) got each
    // other's timing and played back with a judder. Here every frame's RTP
    // timestamp is its own capture time.
    let mut vpack = VideoRtp::new(vssrc, h264_pt, av1_pt);
    let mut video_base: Option<i64> = None; // pts of the first frame sent
    let mut video_ticks: u64 = 0; // 90 kHz ticks advanced so far

    let mut media_rx = shared.media_tx.subscribe();
    shared.request_keyframe();
    let mut waiting_for_key = true;
    loop {
        tokio::select! {
            pkt = media_rx.recv() => match pkt {
                Ok(p) => match p.kind {
                    MediaKind::Video => {
                        // A codec this viewer can't take (the stream is
                        // switching to one it can): wait for its keyframe.
                        if !vpack.accepts(p.codec) {
                            waiting_for_key = true;
                            continue;
                        }
                        if waiting_for_key && !p.keyframe {
                            continue;
                        }
                        waiting_for_key = false;
                        let base = *video_base.get_or_insert(p.pts_us);
                        let target = ((p.pts_us - base).max(0) as u64) * 90_000 / 1_000_000;
                        // Never backwards (a new capture's clock may start
                        // slightly behind the old one's).
                        video_ticks = video_ticks.max(target);
                        match vpack.packetize(p.codec, video_ticks, &p.data) {
                            Ok(packets) => {
                                for pkt in packets {
                                    if let Err(e) = video_track.write_rtp(pkt).await {
                                        eprintln!("viewer {sid}: video write: {e}");
                                        break;
                                    }
                                }
                            }
                            Err(e) => eprintln!("viewer {sid}: packetize: {e}"),
                        }
                    }
                    MediaKind::Audio => {
                        if let Some((t, ssrc, pt)) = &audio {
                            let sample = Sample { data: p.data, duration: p.duration, ..Sample::new(Instant::now()) };
                            let _ = t.sample_writer(*ssrc, *pt).write_sample(&sample).await;
                        }
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    waiting_for_key = true;
                    shared.request_keyframe();
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            s = state_rx.recv() => match s {
                Some(RTCPeerConnectionState::Failed) | Some(RTCPeerConnectionState::Closed) | None => return Ok(()),
                Some(RTCPeerConnectionState::Disconnected) => shared.set_viewer_state(sid, ViewerState::Connecting),
                Some(RTCPeerConnectionState::Connected) => shared.set_viewer_state(sid, ViewerState::Connected),
                _ => {}
            },
            _ = kick_rx.changed() => return Ok(()),
            _ = shutdown.changed() => return Ok(()),
        }
    }
}

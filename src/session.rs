//! A live hosting session: capture → encode once → fan out to every viewer's
//! own WebRTC peer connection. Signaling runs over Nostr relays.

use crate::capture::{AudioCapture, Callbacks, Capture, CaptureConfig, SourceKind};
use crate::media::{MediaKind, MediaPacket, Stats};
use crate::settings::{AudioMode, Settings};
use crate::signaling::{IceServer, Incoming, RelayStatus, Room, SignalMessage, Signaling};
use crate::{audio, rtcp_forward::RtcpForwarder};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use rtc::interceptor::{Registry, Slot};
use rtc::media::Sample;
use rtc::rtp::packetizer::Packetizer;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
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
}

struct PeerHandle {
    pubkey: String,
    answer_tx: mpsc::UnboundedSender<String>,
    kick_tx: watch::Sender<bool>,
    offer: Arc<Mutex<Option<SignalMessage>>>,
}

type MakeCallbacks = Arc<dyn Fn(u64, u32) -> Callbacks + Send + Sync>;

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
        let cap = Capture::start(&cfg, (self.make_callbacks)(generation, cfg.fps))?;
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
            Arc::new(move |generation: u64, fps: u32| {
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

        let capture = Capture::start(&capture_cfg, make_callbacks(0, capture_cfg.fps))?;
        let info = VideoInfo { source_label: source.label, width: capture.width, height: capture.height, fps: capture_cfg.fps };

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
        let info = self.0.info.lock().unwrap().clone();
        let msg = SignalMessage::Info { width: info.width, height: info.height, fps: info.fps };
        let pubkeys: Vec<String> = self.0.peers.lock().unwrap().values().map(|p| p.pubkey.clone()).collect();
        for pk in &pubkeys {
            self.0.signaling.send(Some(pk), &msg);
        }
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
            SignalMessage::Hello { sid, label } => {
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
                            ViewerInfo { sid: sid.clone(), label, state: ViewerState::Negotiating, since: Instant::now() },
                        );
                        let shared2 = shared.clone();
                        let pubkey = msg.from_pubkey.clone();
                        tokio::spawn(async move {
                            let r = run_peer(shared2.clone(), sid.clone(), pubkey, offer, answer_rx, kick_rx).await;
                            if let Err(e) = r {
                                eprintln!("viewer {sid}: {e}");
                            }
                            shared2.peers.lock().unwrap().remove(&sid);
                            shared2.viewers.lock().unwrap().remove(&sid);
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
    // add_track only offers the track's own codec; offer every H.264 profile id.
    for t in pc.get_transceivers().await {
        if let Ok(Some(s)) = t.sender().await {
            if s.id() == video_sender.id() {
                t.set_codec_preferences(video_codecs()).await?;
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

    let (vssrc, vpt) = negotiated(&video_sender, &video_track).await?;
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
    let mut vpack = rtc::rtp::packetizer::new_packetizer(
        Instant::now(),
        1200,
        vpt,
        vssrc,
        video_codecs()[0].rtp_codec.payloader()?,
        Box::new(rtc::rtp::sequence::new_random_sequencer()),
        90_000,
    );
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
                        if waiting_for_key && !p.keyframe {
                            continue;
                        }
                        waiting_for_key = false;
                        let base = *video_base.get_or_insert(p.pts_us);
                        let target = ((p.pts_us - base).max(0) as u64) * 90_000 / 1_000_000;
                        if target > video_ticks {
                            vpack.skip_samples((target - video_ticks) as u32);
                            video_ticks = target;
                        }
                        match vpack.packetize(Instant::now(), &p.data, 0) {
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

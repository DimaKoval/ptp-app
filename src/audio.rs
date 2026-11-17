//! Mixes system audio + optional microphone and encodes 20ms Opus frames.

use crate::media::{MediaKind, MediaPacket, Stats};
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use tokio::sync::broadcast;

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20ms @ 48kHz
const MAX_QUEUE: usize = RATE as usize / 4 * 2; // 250ms of stereo samples

pub struct Pcm {
    pub samples: Vec<f32>, // interleaved stereo
    pub rate: u32,
    pub mic: bool,
}

/// Stateful linear resampler for interleaved stereo.
struct Resampler {
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    fn new() -> Self {
        Resampler { pos: 0.0, last: [0.0; 2] }
    }

    fn process(&mut self, input: &[f32], rate: u32, out: &mut VecDeque<f32>) {
        if rate == RATE || rate == 0 {
            out.extend(input.iter().copied());
            return;
        }
        let step = rate as f64 / RATE as f64;
        let frames = input.len() / 2;
        let sample = |i: isize, c: usize, last: &[f32; 2]| -> f32 {
            if i < 0 { last[c] } else { input[i as usize * 2 + c] }
        };
        // pos is relative to the previous buffer's last frame (index -1).
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize - 1;
            let t = (self.pos - self.pos.floor()) as f32;
            for c in 0..2 {
                let a = sample(i, c, &self.last);
                let b = sample(i + 1, c, &self.last);
                out.push_back(a + (b - a) * t);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
        }
    }
}

pub fn spawn_encoder(
    rx: Receiver<Pcm>,
    out: broadcast::Sender<MediaPacket>,
    has_system: bool,
    stats: Arc<Stats>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("opus-encoder".into())
        .spawn(move || {
            let mut enc = match opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("opus init failed: {e}");
                    return;
                }
            };
            // Tuned for music/game/video audio rather than speech: 160 kbps
            // stereo Opus is near-transparent and tiny next to the video.
            let _ = enc.set_bitrate(opus::Bitrate::Bits(160_000));
            let _ = enc.set_signal(opus::Signal::Music);
            let _ = enc.set_bandwidth(opus::Bandwidth::Fullband);
            let _ = enc.set_complexity(10);
            let _ = enc.set_lsb_depth(24);
            let mut sys_q: VecDeque<f32> = VecDeque::new();
            let mut mic_q: VecDeque<f32> = VecDeque::new();
            let mut sys_rs = Resampler::new();
            let mut mic_rs = Resampler::new();
            let mut frame = vec![0f32; FRAME * 2];
            let mut buf = vec![0u8; 4000];
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(pcm) => {
                        if pcm.mic {
                            mic_rs.process(&pcm.samples, pcm.rate, &mut mic_q);
                        } else {
                            sys_rs.process(&pcm.samples, pcm.rate, &mut sys_q);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                for q in [&mut sys_q, &mut mic_q] {
                    while q.len() > MAX_QUEUE {
                        q.drain(..FRAME * 2);
                    }
                }
                loop {
                    // System audio drives timing; mic alone drives it when there is
                    // no system audio or system audio went silent (SCK sends nothing).
                    let sys_ready = sys_q.len() >= FRAME * 2;
                    let mic_only = mic_q.len() >= FRAME * 2 && (!has_system || mic_q.len() >= FRAME * 2 * 3);
                    if !sys_ready && !mic_only {
                        break;
                    }
                    frame.iter_mut().for_each(|s| *s = 0.0);
                    if sys_ready {
                        for (i, s) in sys_q.drain(..FRAME * 2).enumerate() {
                            frame[i] = s;
                        }
                    }
                    let n = mic_q.len().min(FRAME * 2);
                    for (i, s) in mic_q.drain(..n).enumerate() {
                        frame[i] += s;
                    }
                    for s in frame.iter_mut() {
                        *s = s.clamp(-1.0, 1.0);
                    }
                    match enc.encode_float(&frame, &mut buf) {
                        Ok(len) => {
                            stats.audio_bytes.fetch_add(len as u64, Ordering::Relaxed);
                            stats.audio_packets.fetch_add(1, Ordering::Relaxed);
                            let _ = out.send(MediaPacket {
                                kind: MediaKind::Audio,
                                data: Bytes::copy_from_slice(&buf[..len]),
                                duration: Duration::from_millis(20),
                                keyframe: false,
                                pts_us: 0,
                            });
                        }
                        Err(e) => eprintln!("opus encode: {e}"),
                    }
                }
            }
        })
        .expect("spawn audio thread")
}

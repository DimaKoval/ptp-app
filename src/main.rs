mod audio;
mod capture;
mod media;
mod rtcp_forward;
mod session;
mod settings;
mod signaling;
mod ui;

use capture::SourceKind;
use session::{HostSession, SourceSelection};
use settings::Settings;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;

    // Headless modes (no UI), print the room link and live stats:
    //   --test-pattern [viewer_url]  synthetic bars + beep, no permission needed
    //   --display [viewer_url]       main display + system audio (needs permission,
    //                                so launch the .app: open -n ptp-share.app --args --display)
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str());
    if mode == Some("--test-pattern") || mode == Some("--display") {
        let mut settings = Settings::load();
        if let Some(url) = args.get(2) {
            settings.viewer_url = url.clone();
        }
        let source = if mode == Some("--display") {
            let list = capture::list_sources()?;
            let d = list.displays.iter().find(|d| d.is_main).or(list.displays.first()).ok_or_else(|| anyhow::anyhow!("no display"))?;
            SourceSelection { kind: SourceKind::Display, id: d.id, label: d.name.clone() }
        } else {
            SourceSelection { kind: SourceKind::TestPattern, id: 0, label: "Test pattern".into() }
        };
        let session = HostSession::start(source, &settings, rt.handle().clone())?;
        println!("Streaming {}x{}@{}", session.width, session.height, session.fps);
        println!("LINK {}", session.link);
        let mut last = (0u64, 0u64);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let relays = session.relay_status();
            let up = relays.iter().filter(|r| r.connected).count();
            let viewers = session.viewers();
            let st = &session.stats;
            let ab = st.audio_bytes.load(std::sync::atomic::Ordering::Relaxed);
            let ap = st.audio_packets.load(std::sync::atomic::Ordering::Relaxed);
            println!(
                "relays {}/{} | audio {} kbps, {} pkt/s, pcm dropped {} | viewers: {}",
                up,
                relays.len(),
                (ab - last.0) * 8 / 5 / 1000,
                (ap - last.1) / 5,
                st.audio_pcm_dropped.load(std::sync::atomic::Ordering::Relaxed),
                viewers.iter().map(|v| format!("{}={:?}", v.label, v.state)).collect::<Vec<_>>().join(", ")
            );
            last = (ab, ap);
        }
    }

    ui::run(rt)
}

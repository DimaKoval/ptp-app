// Release builds on Windows are a GUI app: no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

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
    #[cfg(windows)]
    unsafe {
        // Real pixel sizes for monitors and windows on scaled displays.
        use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;

    // Headless modes (no UI), print the room link and live stats:
    //   --test-pattern [viewer_url]  synthetic bars + beep, no permission needed
    //   --display [viewer_url]       main display + system audio (needs permission,
    //                                so launch the .app: open -n ptp-share.app --args --display)
    //   --window <app> [viewer_url]  first window of an app whose name contains <app>
    let mut args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).cloned();
    let mode = mode.as_deref();
    let window_app = if mode == Some("--window") && args.len() > 2 { Some(args.remove(2)) } else { None };
    if matches!(mode, Some("--test-pattern" | "--display" | "--window")) {
        let mut settings = Settings::load();
        if let Some(url) = args.get(2) {
            settings.viewer_url = url.clone();
        }
        let source = if let Some(app) = window_app {
            let list = capture::list_sources()?;
            let needle = app.to_lowercase();
            let w = list
                .windows
                .iter()
                .find(|w| w.app_name.to_lowercase().contains(&needle))
                .ok_or_else(|| anyhow::anyhow!("no window for app {app}"))?;
            println!("Window {} \"{}\" ({})", w.id, w.title, w.app_name);
            SourceSelection { kind: SourceKind::Window, id: w.id, label: w.title.clone() }
        } else if mode == Some("--display") {
            let list = capture::list_sources()?;
            let d = list.displays.iter().find(|d| d.is_main).or(list.displays.first()).ok_or_else(|| anyhow::anyhow!("no display"))?;
            SourceSelection { kind: SourceKind::Display, id: d.id, label: d.name.clone() }
        } else {
            SourceSelection { kind: SourceKind::TestPattern, id: 0, label: "Test pattern".into() }
        };
        let session = HostSession::start(source, &settings, rt.handle().clone())?;
        println!("Streaming {}x{}@{}", session.width, session.height, session.fps);
        println!("LINK {}", session.link);
        let mut last = (0u64, 0u64, 0u64);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let relays = session.relay_status();
            let up = relays.iter().filter(|r| r.connected).count();
            let viewers = session.viewers();
            let st = &session.stats;
            let ab = st.audio_bytes.load(std::sync::atomic::Ordering::Relaxed);
            let ap = st.audio_packets.load(std::sync::atomic::Ordering::Relaxed);
            let vf = st.video_frames.load(std::sync::atomic::Ordering::Relaxed);
            println!(
                "video {} fps{} | relays {}/{} | audio {} kbps, {} pkt/s, pcm dropped {} | viewers: {}",
                (vf - last.2) / 5,
                session.error().map(|e| format!(" [{e}]")).unwrap_or_default(),
                up,
                relays.len(),
                (ab - last.0) * 8 / 5 / 1000,
                (ap - last.1) / 5,
                st.audio_pcm_dropped.load(std::sync::atomic::Ordering::Relaxed),
                viewers.iter().map(|v| format!("{}={:?}", v.label, v.state)).collect::<Vec<_>>().join(", ")
            );
            last = (ab, ap, vf);
        }
    }

    ui::run(rt)
}

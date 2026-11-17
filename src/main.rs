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

    // Headless mode: `ptp-host --test-pattern [viewer_url]` streams a synthetic
    // pattern + beep without needing Screen Recording permission.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--test-pattern") {
        let mut settings = Settings::load();
        if let Some(url) = args.get(2) {
            settings.viewer_url = url.clone();
        }
        let source = SourceSelection { kind: SourceKind::TestPattern, id: 0, label: "Test pattern".into() };
        let session = HostSession::start(source, &settings, rt.handle().clone())?;
        println!("Streaming test pattern {}x{}@{}", session.width, session.height, session.fps);
        println!("LINK {}", session.link);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let relays = session.relay_status();
            let up = relays.iter().filter(|r| r.connected).count();
            let viewers = session.viewers();
            println!(
                "relays {}/{} | viewers: {}",
                up,
                relays.len(),
                viewers.iter().map(|v| format!("{}={:?}", v.label, v.state)).collect::<Vec<_>>().join(", ")
            );
        }
    }

    ui::run(rt)
}

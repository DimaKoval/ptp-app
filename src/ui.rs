//! egui front-end: pick a source, tune quality/audio, go live, manage viewers.

use crate::capture::{self, SourceKind, SourceList, Thumbnail};
use crate::session::{HostSession, SourceSelection, ViewerState};
use crate::signaling::Room;
use crate::settings::{AudioMode, FPS_OPTIONS, Resolution, Settings};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, StrokeKind, TextureHandle, Vec2,
};
use egui_phosphor::regular as icon;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

// ───────────────────────────── theme ─────────────────────────────

const BG: Color32 = Color32::from_rgb(13, 15, 20);
const PANEL: Color32 = Color32::from_rgb(20, 23, 30);
const CARD: Color32 = Color32::from_rgb(27, 31, 40);
const CARD_HOVER: Color32 = Color32::from_rgb(34, 39, 50);
const BORDER: Color32 = Color32::from_rgb(42, 47, 60);
const TEXT: Color32 = Color32::from_rgb(230, 233, 240);
const MUTED: Color32 = Color32::from_rgb(140, 147, 165);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const ACCENT_SOFT: Color32 = Color32::from_rgb(52, 42, 96);
const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
const RED: Color32 = Color32::from_rgb(248, 81, 73);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.slider_width = 220.0;
    style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
    style.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(20.0));

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(16, 18, 24);
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    let r = CornerRadius::same(8);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.hovered.bg_fill = CARD_HOVER;
    v.widgets.hovered.weak_bg_fill = CARD_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(70, 76, 95));
    v.widgets.active.bg_fill = ACCENT_SOFT;
    v.widgets.active.weak_bg_fill = ACCENT_SOFT;
    v.window_corner_radius = CornerRadius::same(12);
    // Always dark, whatever the OS theme: egui keeps a separate style per
    // theme and follows the system one, which on a light Windows gave light
    // text-field interiors and a black title on the dark background.
    ctx.set_theme(egui::Theme::Dark);
    // Dark window title bar too (Windows draws it in the OS theme otherwise).
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(16))
}

fn section_title(ui: &mut egui::Ui, ic: &str, text: &str) {
    ui.label(RichText::new(format!("{ic}  {text}")).size(13.0).color(MUTED).strong());
}

/// Segmented control. Returns true when the value changed.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, String)]) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(Color32::from_rgb(16, 18, 24))
        .corner_radius(10)
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let avail = ui.available_width();
            let w = ((avail - 3.0 * (options.len() as f32 - 1.0)) / options.len() as f32).max(40.0);
            ui.horizontal(|ui| {
                for (opt, label) in options {
                    let selected = *value == *opt;
                    let btn = egui::Button::new(RichText::new(label).color(if selected { Color32::WHITE } else { MUTED }))
                        .fill(if selected { ACCENT } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(8)
                        .min_size(Vec2::new(w, 28.0));
                    if ui.add(btn).clicked() && !selected {
                        *value = *opt;
                        changed = true;
                    }
                }
            });
        });
    changed
}

fn toggle(ui: &mut egui::Ui, on: &mut bool, enabled: bool) -> egui::Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let bg = if !enabled {
        Color32::from_rgb(40, 44, 54)
    } else {
        lerp_color(Color32::from_rgb(55, 60, 75), ACCENT, t)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.0, if enabled { Color32::WHITE } else { MUTED });
    resp
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn primary_button(ui: &mut egui::Ui, text: String, color: Color32, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).size(15.0).strong().color(Color32::WHITE))
        .fill(if enabled { color } else { Color32::from_rgb(50, 54, 66) })
        .stroke(Stroke::NONE)
        .corner_radius(10)
        .min_size(Vec2::new(ui.available_width(), 44.0));
    // Button content follows the parent layout's alignment; force centered.
    ui.with_layout(ui.layout().with_cross_align(Align::Center), |ui| ui.add_enabled(enabled, btn)).inner
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(20)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(color).strong());
        });
}

// ───────────────────────────── app ─────────────────────────────

type ThumbKey = (SourceKind, u32);

/// The settings that need a capture restart when changed while live.
#[derive(Clone, PartialEq)]
struct CaptureKey {
    resolution: Resolution,
    fps: u32,
    audio_mode: AudioMode,
    excluded_apps: Vec<String>,
    audio_app: Option<String>,
    capture_mic: bool,
}

impl CaptureKey {
    fn of(s: &Settings) -> CaptureKey {
        CaptureKey {
            resolution: s.resolution,
            fps: s.fps,
            audio_mode: s.audio_mode,
            excluded_apps: s.excluded_apps.clone(),
            audio_app: s.audio_app.clone(),
            capture_mic: s.capture_mic,
        }
    }
}

enum StartResult {
    Ok(HostSession),
    Err(String),
}

struct App {
    rt: tokio::runtime::Runtime,
    settings: Settings,
    sources: SourceList,
    sources_rx: Option<Receiver<Result<SourceList, String>>>,
    selected: Option<(SourceKind, u32, String)>,
    thumbs: HashMap<ThumbKey, TextureHandle>,
    thumb_tx: Sender<(ThumbKey, Thumbnail)>,
    thumb_rx: Receiver<(ThumbKey, Thumbnail)>,
    session: Option<HostSession>,
    start_rx: Option<Receiver<StartResult>>,
    error: Option<String>,
    has_access: bool,
    copied_at: Option<Instant>,
    rate_sample: (Instant, u64, u64, u64),
    rate: (f32, f32),
    /// Live changes: capture settings last applied, and a running reconfigure.
    applied: Option<CaptureKey>,
    applied_bitrate: f32,
    reconfig_rx: Option<Receiver<Result<(), String>>>,
    /// Source the live session is showing, and one picked but not applied yet.
    live_source: Option<(SourceKind, u32, String)>,
    pending_source: Option<(SourceKind, u32, String)>,
    picking_source: bool,
    /// Sources refresh on their own: when the app gets focus, and every
    /// 10 s while the picker is on screen.
    was_focused: bool,
    last_refresh: Instant,
    show_network: bool,
    relays_text: String,
    window_filter: String,
}

pub fn run(rt: tokio::runtime::Runtime) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PTP Share")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 620.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "PTP Share",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new(rt, &cc.egui_ctx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

impl App {
    fn new(rt: tokio::runtime::Runtime, ctx: &egui::Context) -> App {
        let settings = Settings::load();
        let (thumb_tx, thumb_rx) = channel();
        let relays_text = settings.relays.join("\n");
        let mut app = App {
            rt,
            applied: None,
            applied_bitrate: settings.bitrate_mbps,
            reconfig_rx: None,
            live_source: None,
            pending_source: None,
            picking_source: false,
            was_focused: true,
            last_refresh: Instant::now(),
            settings,
            sources: SourceList::default(),
            sources_rx: None,
            selected: None,
            thumbs: HashMap::new(),
            thumb_tx,
            thumb_rx,
            session: None,
            start_rx: None,
            error: None,
            has_access: capture::has_screen_access(),
            copied_at: None,
            rate_sample: (Instant::now(), 0, 0, 0),
            rate: (0.0, 0.0),
            show_network: false,
            relays_text,
            window_filter: String::new(),
        };
        if !app.has_access {
            // Triggers the system prompt the first time.
            capture::request_screen_access();
        }
        app.refresh_sources(ctx);
        app
    }

    fn refresh_sources(&mut self, ctx: &egui::Context) {
        self.last_refresh = Instant::now();
        let (tx, rx) = channel();
        self.sources_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(capture::list_sources().map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }

    fn load_thumbnails(&self, ctx: &egui::Context) {
        let mut keys: Vec<ThumbKey> = self.sources.displays.iter().map(|d| (SourceKind::Display, d.id)).collect();
        keys.extend(self.sources.windows.iter().map(|w| (SourceKind::Window, w.id)));
        let tx = self.thumb_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for key in keys {
                if let Some(t) = capture::thumbnail(key.0, key.1, 480, 300) {
                    if tx.send((key, t)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.sources_rx {
            if let Ok(res) = rx.try_recv() {
                self.sources_rx = None;
                match res {
                    Ok(list) => {
                        self.has_access = true;
                        self.sources = list;
                        if self.selected.is_none() {
                            if let Some(d) = self.sources.displays.iter().find(|d| d.is_main).or(self.sources.displays.first()) {
                                self.selected = Some((SourceKind::Display, d.id, d.name.clone()));
                            }
                        }
                        self.load_thumbnails(ctx);
                    }
                    Err(e) => {
                        // Listing is the real test: the preflight check can report a
                        // grant that only takes effect after the app is relaunched.
                        self.has_access = false;
                        eprintln!("listing sources failed: {e}");
                    }
                }
            }
        }
        while let Ok((key, t)) = self.thumb_rx.try_recv() {
            let img = egui::ColorImage::from_rgba_unmultiplied([t.width as usize, t.height as usize], &t.rgba);
            let tex = ctx.load_texture(format!("thumb-{:?}-{}", key.0, key.1), img, egui::TextureOptions::LINEAR);
            self.thumbs.insert(key, tex);
        }
        if let Some(rx) = &self.start_rx {
            if let Ok(res) = rx.try_recv() {
                self.start_rx = None;
                match res {
                    StartResult::Ok(s) => {
                        self.applied = Some(CaptureKey::of(&self.settings));
                        self.applied_bitrate = self.settings.bitrate_mbps;
                        self.live_source = self.selected.clone();
                        self.rate_sample = (Instant::now(), 0, 0, 0);
                        self.session = Some(s);
                    }
                    StartResult::Err(e) => self.error = Some(e),
                }
            }
        }
        if let Some(s) = &self.session {
            let now = Instant::now();
            let dt = now.duration_since(self.rate_sample.0).as_secs_f32();
            if dt >= 1.0 {
                let vb = s.stats.video_bytes.load(Ordering::Relaxed);
                let ab = s.stats.audio_bytes.load(Ordering::Relaxed);
                let fr = s.stats.video_frames.load(Ordering::Relaxed);
                let bytes = (vb + ab).saturating_sub(self.rate_sample.1 + self.rate_sample.3);
                let frames = fr.saturating_sub(self.rate_sample.2);
                self.rate = (bytes as f32 * 8.0 / dt / 1e6, frames as f32 / dt);
                self.rate_sample = (now, vb, fr, ab);
            }
        }
    }

    fn start(&mut self) {
        let Some((kind, id, label)) = self.selected.clone() else { return };
        self.settings.relays = self.relays_text.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if self.settings.relays.is_empty() {
            self.settings.relays = crate::signaling::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
            self.relays_text = self.settings.relays.join("\n");
        }
        let room = self.room();
        self.settings.save();
        self.error = None;
        let settings = self.settings.clone();
        let handle = self.rt.handle().clone();
        let (tx, rx) = channel();
        self.start_rx = Some(rx);
        std::thread::spawn(move || {
            let res = match HostSession::start(SourceSelection { kind, id, label }, &settings, room, handle) {
                Ok(s) => StartResult::Ok(s),
                Err(e) => StartResult::Err(e.to_string()),
            };
            let _ = tx.send(res);
        });
    }

    /// Keeps the screen/window list current without a Refresh button: on
    /// regaining focus (you probably opened or closed windows meanwhile) and
    /// every 10 s while the picker is visible.
    fn auto_refresh(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let picker_visible = self.session.is_none() || self.picking_source;
        let due = focused && picker_visible && self.last_refresh.elapsed() >= Duration::from_secs(10);
        let regained = focused && !self.was_focused;
        self.was_focused = focused;
        if (regained || due) && self.sources_rx.is_none() {
            self.refresh_sources(ctx);
        }
        if focused && picker_visible {
            ctx.request_repaint_after(Duration::from_secs(10));
        }
    }

    /// The saved room (same link every session); makes one the first time.
    fn room(&mut self) -> Room {
        if let Some(r) = Room::from_saved(&self.settings.room_secret) {
            return r;
        }
        let r = Room::generate();
        self.settings.room_secret = r.saved();
        self.settings.save();
        r
    }

    fn current_link(&mut self) -> String {
        let relays: Vec<String> = self.relays_text.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let relays = if relays.is_empty() {
            crate::signaling::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
        } else {
            relays
        };
        let url = self.settings.viewer_url.trim().to_string();
        self.room().link(&url, &relays)
    }

    /// While live: push changed settings to the running session. Capture
    /// changes restart capture next to the running one (viewers stay
    /// connected); bitrate is applied in place once the slider is released.
    fn apply_live_changes(&mut self, ctx: &egui::Context) {
        let Some(session) = &self.session else { return };
        if let Some(rx) = &self.reconfig_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.reconfig_rx = None;
                    if let Err(e) = res {
                        self.error = Some(e);
                    }
                }
                Err(_) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    return;
                }
            }
        }
        let dragging = ctx.input(|i| i.pointer.any_down());
        if !dragging && (self.settings.bitrate_mbps - self.applied_bitrate).abs() > 0.01 {
            self.applied_bitrate = self.settings.bitrate_mbps;
            session.set_bitrate((self.settings.bitrate_mbps * 1000.0) as u32);
            self.settings.save();
        }
        let key = CaptureKey::of(&self.settings);
        let source = self.pending_source.take();
        if self.applied.as_ref() == Some(&key) && source.is_none() {
            return;
        }
        self.applied = Some(key);
        self.error = None;
        self.settings.save();
        if let Some(s) = &source {
            self.live_source = Some(s.clone());
            self.selected = Some(s.clone());
        }
        let control = session.control();
        let settings = self.settings.clone();
        let (tx, rx) = channel();
        self.reconfig_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let src = source.map(|(kind, id, label)| SourceSelection { kind, id, label });
            let _ = tx.send(control.reconfigure(src, &settings).map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }

    fn stop(&mut self) {
        if let Some(s) = self.session.take() {
            std::thread::spawn(move || s.stop());
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.auto_refresh(&ctx);
        debug_screenshot(&ctx);
        // Debug: PTP_UI_AUTOSTART=1 starts sharing the first screen (with
        // PTP_UI_SHOT, to screenshot the live view); =pick also opens the
        // source switcher.
        if let Ok(mode) = std::env::var("PTP_UI_AUTOSTART") {
            static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !self.sources.displays.is_empty() && !STARTED.swap(true, Ordering::Relaxed) {
                let d = &self.sources.displays[0];
                self.selected = Some((SourceKind::Display, d.id, d.name.clone()));
                self.start();
            }
            if mode == "pick" && self.session.is_some() && !self.picking_source && self.live_source == self.selected {
                self.picking_source = true;
            }
        }
        if self.session.is_some() || self.start_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 12)))
            .show_separator_line(false)
            .show(ui, |ui| self.top_bar(ui));

        if self.session.is_some() {
            egui::Panel::right("settings")
                .exact_size(360.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::same(20)))
                .show(ui, |ui| self.settings_panel(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(20)))
                .show(ui, |ui| self.live_view(ui));
            self.source_modal(&ctx);
            self.apply_live_changes(&ctx);
            return;
        }

        egui::Panel::right("settings")
            .exact_size(360.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::same(20)))
            .show(ui, |ui| self.settings_panel(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(20)))
            .show(ui, |ui| self.source_picker(ui));
    }
}

/// Debug hook: PTP_UI_SHOT=<file.ppm> saves a screenshot of the window after
/// ~2 s and quits. PTP_UI_THEME=light switches egui to its light theme first,
/// as a light OS theme would (the UI must still look the same).
fn debug_screenshot(ctx: &egui::Context) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static FRAME: AtomicU32 = AtomicU32::new(0);
    let Ok(path) = std::env::var("PTP_UI_SHOT") else { return };
    let n = FRAME.fetch_add(1, Ordering::Relaxed);
    if n == 1 && std::env::var("PTP_UI_THEME").as_deref() == Ok("light") {
        ctx.set_theme(egui::Theme::Light);
    }
    ctx.request_repaint_after(Duration::from_millis(50));
    if n == std::env::var("PTP_UI_SHOT_FRAME").ok().and_then(|v| v.parse().ok()).unwrap_or(40) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    }
    let shot = ctx.input(|i| {
        i.raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let mut out = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
        for p in &img.pixels {
            out.extend_from_slice(&[p.r(), p.g(), p.b()]);
        }
        let _ = std::fs::write(&path, out);
        std::process::exit(0);
    }
}

// ───────────────────────────── views ─────────────────────────────

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::BROADCAST).size(22.0).color(ACCENT));
            ui.label(RichText::new("PTP Share").size(18.0).strong());
            ui.label(RichText::new("peer-to-peer screen sharing").size(13.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(s) = &self.session {
                    badge(ui, &format!("{}  LIVE", icon::RECORD), RED);
                    let _ = s;
                } else if self.has_access {
                    badge(ui, &format!("{}  Screen access granted", icon::CHECK_CIRCLE), GREEN);
                } else {
                    badge(ui, &format!("{}  No screen access", icon::WARNING), AMBER);
                }
            });
        });
    }

    fn source_picker(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.has_access {
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{}  Screen Recording permission needed", icon::LOCK_KEY)).size(16.0).strong());
                ui.label(
                    RichText::new(
                        "macOS needs your permission before ptp-share can see screens and windows. \
                         Enable \"ptp-share\" in System Settings → Privacy & Security → Screen & System Audio Recording.",
                    )
                    .color(MUTED),
                );
                if capture::has_screen_access() {
                    ui.label(
                        RichText::new(format!(
                            "{}  Permission is granted but not active yet — macOS applies it after you quit and reopen the app.",
                            icon::INFO
                        ))
                        .color(AMBER),
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button(format!("{}  Open System Settings", icon::GEAR)).clicked() {
                        let _ = std::process::Command::new("open")
                            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                            .spawn();
                    }
                    if ui.button(format!("{}  Check again", icon::ARROWS_CLOCKWISE)).clicked() {
                        self.refresh_sources(&ctx);
                    }
                    if ui.button(format!("{}  Quit & reopen", icon::POWER)).clicked() {
                        relaunch();
                    }
                });
            });
            ui.add_space(12.0);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("Choose what to share").size(20.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.sources_rx.is_some() {
                    ui.spinner();
                }
            });
        });
        ui.add_space(8.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.window_filter)
                .hint_text(format!("{}  Filter screens and windows…", icon::MAGNIFYING_GLASS))
                .margin(egui::Margin::symmetric(12, 9))
                .font(FontId::proportional(14.0))
                .desired_width(360.0),
        );
        ui.add_space(12.0);

        struct Item {
            key: ThumbKey,
            title: String,
            subtitle: String,
            label: String,
        }
        let f = self.window_filter.to_lowercase();
        let screens: Vec<Item> = self
            .sources
            .displays
            .iter()
            .filter(|d| f.is_empty() || d.name.to_lowercase().contains(&f))
            .map(|d| Item {
                key: (SourceKind::Display, d.id),
                title: d.name.clone(),
                subtitle: format!("{} × {}", d.width, d.height),
                label: d.name.clone(),
            })
            .collect();
        let windows: Vec<Item> = self
            .sources
            .windows
            .iter()
            .filter(|w| f.is_empty() || w.title.to_lowercase().contains(&f) || w.app_name.to_lowercase().contains(&f))
            .map(|w| {
                let title = if w.title.is_empty() { w.app_name.clone() } else { w.title.clone() };
                Item {
                    key: (SourceKind::Window, w.id),
                    subtitle: format!("{} · {} × {}", w.app_name, w.width, w.height),
                    label: format!("{} — {}", w.app_name, title),
                    title,
                }
            })
            .collect();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if screens.is_empty() && windows.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(icon::SELECTION_SLASH).size(40.0).color(MUTED));
                    ui.label(RichText::new("Nothing matches").color(MUTED));
                });
            }
            let card_w = 236.0;
            let cols = ((ui.available_width() + 12.0) / (card_w + 12.0)).floor().max(1.0) as usize;
            for (ic, name, items) in [(icon::MONITOR, "Screens", &screens), (icon::APP_WINDOW, "Windows", &windows)] {
                if items.is_empty() {
                    continue;
                }
                ui.label(
                    RichText::new(format!("{ic}  {name}  ·  {}", items.len()))
                        .size(14.0)
                        .strong()
                        .color(MUTED),
                );
                if name == "Windows" {
                    ui.label(
                        RichText::new(format!(
                            "{}  Don't minimize the window you share: macOS stops drawing minimized windows, so viewers \
                             would see a frozen picture. Covering it with other windows is fine.",
                            icon::INFO
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                }
                ui.add_space(6.0);
                for row in items.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for item in row {
                            let selected = self.selected.as_ref().map(|s| (s.0, s.1)) == Some(item.key);
                            let resp = self.source_card(ui, item.key, &item.title, &item.subtitle, selected, card_w);
                            if resp.clicked() {
                                self.selected = Some((item.key.0, item.key.1, item.label.clone()));
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(14.0);
            }
            if cfg!(debug_assertions) || std::env::var("PTP_TEST_PATTERN").is_ok() {
                let selected = matches!(self.selected, Some((SourceKind::TestPattern, ..)));
                if ui.selectable_label(selected, format!("{}  Test pattern (debug)", icon::FLASK)).clicked() {
                    self.selected = Some((SourceKind::TestPattern, 0, "Test pattern".into()));
                }
            }
        });
    }

    fn source_card(
        &self,
        ui: &mut egui::Ui,
        key: ThumbKey,
        title: &str,
        subtitle: &str,
        selected: bool,
        width: f32,
    ) -> egui::Response {
        let thumb_h = width * 9.0 / 16.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, thumb_h + 58.0), Sense::click());
        let painter = ui.painter_at(rect.expand(2.0));
        let fill = if resp.hovered() { CARD_HOVER } else { CARD };
        painter.rect(
            rect,
            CornerRadius::same(12),
            fill,
            Stroke::new(if selected { 2.0 } else { 1.0 }, if selected { ACCENT } else { BORDER }),
            StrokeKind::Inside,
        );
        let thumb_rect = egui::Rect::from_min_size(rect.min + Vec2::splat(8.0), Vec2::new(width - 16.0, thumb_h - 8.0));
        painter.rect_filled(thumb_rect, CornerRadius::same(8), Color32::from_rgb(12, 14, 18));
        if let Some(tex) = self.thumbs.get(&key) {
            let ts = tex.size_vec2();
            let scale = (thumb_rect.width() / ts.x).min(thumb_rect.height() / ts.y);
            let img_rect = egui::Rect::from_center_size(thumb_rect.center(), ts * scale);
            egui::Image::new((tex.id(), img_rect.size()))
                .corner_radius(6)
                .paint_at(ui, img_rect);
        } else {
            let ic = if key.0 == SourceKind::Display { icon::MONITOR } else { icon::APP_WINDOW };
            painter.text(thumb_rect.center(), Align2C, ic, FontId::proportional(32.0), MUTED);
        }
        let text_x = rect.left() + 12.0;
        let title_y = thumb_rect.bottom() + 10.0;
        let mut job = egui::text::LayoutJob::simple_singleline(title.to_string(), FontId::proportional(14.0), TEXT);
        job.wrap = egui::text::TextWrapping::truncate_at_width(width - 24.0);
        let galley = painter.layout_job(job);
        painter.galley(egui::pos2(text_x, title_y), galley, TEXT);
        painter.text(
            egui::pos2(text_x, title_y + 22.0),
            egui::Align2::LEFT_TOP,
            subtitle,
            FontId::proportional(12.0),
            MUTED,
        );
        if selected {
            let c = egui::pos2(thumb_rect.right() - 14.0, thumb_rect.top() + 14.0);
            painter.circle_filled(c, 11.0, ACCENT);
            painter.text(c, Align2C, icon::CHECK, FontId::proportional(13.0), Color32::WHITE);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let starting = self.start_rx.is_some();
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - if self.session.is_some() { 70.0 } else { 200.0 }).show(ui, |ui| {
            ui.label(RichText::new("Stream settings").size(18.0).strong());
            ui.add_space(8.0);

            section_title(ui, icon::FRAME_CORNERS, "RESOLUTION");
            let opts: Vec<(Resolution, String)> = Resolution::ALL.iter().map(|r| (*r, r.label().to_string())).collect();
            if segmented(ui, &mut self.settings.resolution, &opts) {
                self.settings.bitrate_mbps = self.settings.resolution.suggested_mbps(self.settings.fps);
            }
            ui.add_space(6.0);

            section_title(ui, icon::GAUGE, "FRAME RATE");
            let fps_opts: Vec<(u32, String)> = FPS_OPTIONS.iter().map(|f| (*f, format!("{f} fps"))).collect();
            if segmented(ui, &mut self.settings.fps, &fps_opts) {
                self.settings.bitrate_mbps = self.settings.resolution.suggested_mbps(self.settings.fps);
            }
            ui.add_space(6.0);

            section_title(ui, icon::CHART_LINE_UP, "BITRATE");
            ui.add(egui::Slider::new(&mut self.settings.bitrate_mbps, 1.0..=50.0).step_by(0.5).suffix(" Mbps"));
            let suggested = self.settings.resolution.suggested_mbps(self.settings.fps);
            let viewers_note = format!(
                "Upload needed ≈ {:.0} Mbps per viewer. Suggested {:.1}.",
                self.settings.bitrate_mbps, suggested
            );
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(viewers_note).size(12.0).color(MUTED));
                if (self.settings.bitrate_mbps - suggested).abs() > 0.1 && ui.small_button("Use suggested").clicked() {
                    self.settings.bitrate_mbps = suggested;
                }
            });
            ui.add_space(10.0);

            section_title(ui, icon::SPEAKER_HIGH, "AUDIO");
            segmented(
                ui,
                &mut self.settings.audio_mode,
                &[
                    (AudioMode::Off, "Off".to_string()),
                    (AudioMode::System, "System".to_string()),
                    (AudioMode::SingleApp, "One app".to_string()),
                ],
            );
            match self.settings.audio_mode {
                AudioMode::Off => {
                    ui.label(RichText::new("Viewers get video only.").size(12.0).color(MUTED));
                }
                AudioMode::System => {
                    ui.label(RichText::new("All system audio, except these apps:").size(12.0).color(MUTED));
                    let mut remove = None;
                    ui.horizontal_wrapped(|ui| {
                        for (i, bid) in self.settings.excluded_apps.iter().enumerate() {
                            let name = self
                                .sources
                                .apps
                                .iter()
                                .find(|a| &a.bundle_id == bid)
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| pretty_bundle(bid));
                            if ui
                                .add(egui::Button::new(RichText::new(format!("{name}  {}", icon::X)).size(12.0)).corner_radius(14))
                                .on_hover_text(bid)
                                .clicked()
                            {
                                remove = Some(i);
                            }
                        }
                    });
                    if let Some(i) = remove {
                        self.settings.excluded_apps.remove(i);
                    }
                    let mut add: Option<String> = None;
                    egui::ComboBox::from_id_salt("exclude_app")
                        .selected_text(format!("{}  Exclude an app…", icon::PLUS))
                        .width(ui.available_width())
                        .height(300.0)
                        .show_ui(ui, |ui| {
                            for a in &self.sources.apps {
                                if !self.settings.excluded_apps.contains(&a.bundle_id) && ui.selectable_label(false, &a.name).clicked() {
                                    add = Some(a.bundle_id.clone());
                                }
                            }
                        });
                    if let Some(a) = add {
                        self.settings.excluded_apps.push(a);
                    }
                }
                AudioMode::SingleApp => {
                    let current = self
                        .settings
                        .audio_app
                        .as_ref()
                        .map(|bid| {
                            self.sources.apps.iter().find(|a| &a.bundle_id == bid).map(|a| a.name.clone()).unwrap_or_else(|| pretty_bundle(bid))
                        })
                        .unwrap_or_else(|| "Choose an app…".into());
                    egui::ComboBox::from_id_salt("audio_app")
                        .selected_text(current)
                        .width(ui.available_width())
                        .height(300.0)
                        .show_ui(ui, |ui| {
                            for a in &self.sources.apps {
                                let sel = self.settings.audio_app.as_deref() == Some(a.bundle_id.as_str());
                                if ui.selectable_label(sel, &a.name).clicked() {
                                    self.settings.audio_app = Some(a.bundle_id.clone());
                                }
                            }
                        });
                }
            }
            ui.add_space(6.0);
            let mic_ok = capture::mic_supported();
            ui.horizontal(|ui| {
                let resp = toggle(ui, &mut self.settings.capture_mic, mic_ok);
                ui.label(format!("{}  Include my microphone", icon::MICROPHONE));
                if !mic_ok {
                    resp.on_hover_text("Requires macOS 15 or newer");
                }
            });
            ui.label(
                RichText::new(if self.settings.capture_mic {
                    "Your mic will be mixed into the stream."
                } else {
                    "Mic stays off — talk on Discord as usual."
                })
                .size(12.0)
                .color(MUTED),
            );
            ui.add_space(10.0);

            let header = format!(
                "{}  Network & viewer page  {}",
                icon::GLOBE,
                if self.show_network { icon::CARET_UP } else { icon::CARET_DOWN }
            );
            if ui.add(egui::Button::new(RichText::new(header).color(MUTED).strong().size(13.0)).frame(false)).clicked() {
                self.show_network = !self.show_network;
            }
            if self.show_network && self.session.is_some() {
                ui.label(RichText::new("Network settings apply the next time you go live.").size(12.0).color(MUTED));
            }
            if self.show_network {
                ui.add_enabled_ui(self.session.is_none(), |ui| {
                ui.label(RichText::new("Viewer page URL (your GitHub Pages site)").size(12.0).color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut self.settings.viewer_url).desired_width(f32::INFINITY));
                ui.add_space(4.0);
                ui.label(RichText::new("TURN server (optional, for strict NATs)").size(12.0).color(MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.turn_url)
                        .hint_text("turn:turn.example.com:3478")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 8.0) / 2.0;
                    ui.add(egui::TextEdit::singleline(&mut self.settings.turn_username).hint_text("username").desired_width(w));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.turn_credential)
                            .hint_text("credential")
                            .password(true)
                            .desired_width(w),
                    );
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Signaling relays (Nostr, one per line)").size(12.0).color(MUTED));
                ui.add(egui::TextEdit::multiline(&mut self.relays_text).desired_rows(4).desired_width(f32::INFINITY));
                });
            }
        });

        if self.session.is_some() {
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    if self.reconfig_rx.is_some() {
                        ui.spinner();
                        ui.label(RichText::new("Applying…").size(12.0).color(MUTED));
                    } else {
                        ui.label(RichText::new(format!("{}  Changes apply live — viewers stay connected.", icon::LIGHTNING)).size(12.0).color(MUTED));
                    }
                });
            });
            return;
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            let can_start = self.selected.is_some() && !starting;
            let label = if starting {
                format!("{}  Starting…", icon::HOURGLASS)
            } else {
                format!("{}  Start sharing", icon::BROADCAST)
            };
            if primary_button(ui, label, ACCENT, can_start).clicked() {
                self.start();
            }
            if let Some((_, _, label)) = &self.selected {
                ui.label(RichText::new(format!("Sharing: {label}")).size(12.0).color(MUTED));
            }
            ui.add_space(4.0);
            let link = self.current_link();
            let mut new_link = false;
            link_box(ui, &link, &mut self.copied_at, Some(&mut new_link));
            if new_link {
                self.settings.room_secret = Room::generate().saved();
                self.settings.save();
            }
            section_title(ui, icon::LINK, "YOUR LINK — STAYS THE SAME EVERY TIME");
            if let Some(e) = self.error.clone() {
                egui::Frame::new()
                    .fill(RED.gamma_multiply(0.15))
                    .corner_radius(8)
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("{}  {e}", icon::WARNING_CIRCLE)).color(RED));
                        });
                        if ui.small_button("Dismiss").clicked() {
                            self.error = None;
                        }
                    });
            }
        });
    }

    fn live_view(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &self.session else { return };
        let elapsed = session.started.elapsed().as_secs();
        let link = session.link.clone();
        let viewers = session.viewers();
        let relays = session.relay_status();
        let err = session.error();
        let relays_up = relays.iter().filter(|r| r.connected).count();
        let mut stop = false;
        let mut kick: Option<String> = None;
        let mut pick_source = false;

        ui.vertical_centered(|ui| {
            ui.set_max_width(760.0);
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let t = ui.input(|i| i.time);
                    let pulse = (0.6 + 0.4 * (t * 3.0).sin()) as f32;
                    ui.label(RichText::new(icon::RECORD).size(22.0).color(RED.gamma_multiply(pulse)));
                    ui.ctx().request_repaint_after(Duration::from_millis(50));
                    ui.label(RichText::new("You're live").size(22.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{:02}:{:02}:{:02}", elapsed / 3600, (elapsed / 60) % 60, elapsed % 60))
                                .size(18.0)
                                .monospace()
                                .color(MUTED),
                        );
                    });
                });
                let info = session.info();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{}  ·  {}×{} @ {} fps", info.source_label, info.width, info.height, info.fps))
                            .color(MUTED),
                    );
                    if ui
                        .add_enabled(self.reconfig_rx.is_none(), egui::Button::new(format!("{}  Change", icon::SWAP)).small())
                        .clicked()
                    {
                        pick_source = true;
                    }
                });
                ui.add_space(12.0);

                section_title(ui, icon::LINK, "ROOM LINK — SEND THIS TO YOUR FRIENDS");
                link_box(ui, &link, &mut self.copied_at, None);
                ui.label(
                    RichText::new("Anyone with this link can watch. It stays the same next time you go live.")
                        .size(12.0)
                        .color(MUTED),
                );
            });
            ui.add_space(12.0);

            ui.columns(3, |cols| {
                stat_card(&mut cols[0], icon::USERS, "Viewers", &format!("{}", viewers.iter().filter(|v| v.state == ViewerState::Connected).count()));
                stat_card(&mut cols[1], icon::CHART_LINE_UP, "Upload / viewer", &format!("{:.1} Mbps", self.rate.0));
                stat_card(&mut cols[2], icon::FILM_STRIP, "Encoded", &format!("{:.0} fps", self.rate.1));
            });
            ui.add_space(12.0);

            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                section_title(ui, icon::USERS, "VIEWERS");
                if viewers.is_empty() {
                    ui.label(RichText::new("Waiting for someone to open the link…").color(MUTED));
                }
                for v in &viewers {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::USER_CIRCLE).size(20.0).color(MUTED));
                        ui.label(RichText::new(&v.label).strong());
                        match v.state {
                            ViewerState::Negotiating => badge(ui, "Joining", AMBER),
                            ViewerState::Connecting => badge(ui, "Connecting", AMBER),
                            ViewerState::Connected => badge(ui, "Watching", GREEN),
                        }
                        ui.label(RichText::new(fmt_dur(v.since.elapsed())).size(12.0).color(MUTED));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button(format!("{}  Remove", icon::USER_MINUS)).clicked() {
                                kick = Some(v.sid.clone());
                            }
                        });
                    });
                }
            });
            ui.add_space(12.0);

            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("Signaling relays {relays_up}/{}", relays.len())).size(12.0).color(MUTED));
                    for r in &relays {
                        let name = r.url.trim_start_matches("wss://").trim_start_matches("ws://");
                        badge(ui, name, if r.connected { GREEN } else { MUTED });
                    }
                });
            });

            if let Some(e) = err {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("{}  {e}", icon::WARNING_CIRCLE)).color(RED));
            }
            ui.add_space(16.0);
            if primary_button(ui, format!("{}  Stop sharing", icon::STOP_CIRCLE), RED, true).clicked() {
                stop = true;
            }
        });

        if let Some(sid) = kick {
            session.kick(&sid);
        }
        if pick_source {
            self.picking_source = true;
            self.refresh_sources(ui.ctx());
        }
        if stop {
            self.stop();
        }
    }
}

impl App {
    /// While live: pick a different screen or window; switching keeps viewers.
    fn source_modal(&mut self, ctx: &egui::Context) {
        if !self.picking_source {
            return;
        }
        let before = self.selected.clone();
        let screen = ctx.content_rect();
        let resp = egui::Modal::new(egui::Id::new("pick_source"))
            .frame(egui::Frame::new().fill(BG).corner_radius(14).inner_margin(Margin::same(20)).stroke(Stroke::new(1.0, BORDER)))
            .show(ctx, |ui| {
                ui.set_width((screen.width() - 120.0).min(800.0));
                ui.set_height((screen.height() - 160.0).min(640.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Switch to another screen or window — viewers stay connected.").color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(format!("{}  Cancel", icon::X)).clicked() {
                            self.picking_source = false;
                        }
                    });
                });
                ui.add_space(8.0);
                self.source_picker(ui);
            });
        if resp.should_close() {
            self.picking_source = false;
        }
        if self.selected != before {
            self.picking_source = false;
            if self.selected != self.live_source {
                self.pending_source = self.selected.clone();
            }
        }
    }
}

/// The share link with a Copy button (and optionally "New link").
fn link_box(ui: &mut egui::Ui, link: &str, copied_at: &mut Option<Instant>, new_link: Option<&mut bool>) {
    egui::Frame::new()
        .fill(Color32::from_rgb(16, 18, 24))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(10)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let copied = copied_at.map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(flag) = new_link {
                        if ui
                            .button(icon::ARROWS_CLOCKWISE)
                            .on_hover_text("Make a new link. The old one stops working.")
                            .clicked()
                        {
                            *flag = true;
                        }
                    }
                    let txt = if copied { format!("{}  Copied", icon::CHECK) } else { format!("{}  Copy", icon::COPY) };
                    let btn = egui::Button::new(RichText::new(txt).color(Color32::WHITE).strong())
                        .fill(if copied { GREEN.gamma_multiply(0.8) } else { ACCENT })
                        .corner_radius(8);
                    if ui.add(btn).clicked() {
                        ui.ctx().copy_text(link.to_string());
                        *copied_at = Some(Instant::now());
                    }
                    ui.add(egui::Label::new(RichText::new(link).monospace().size(12.5).color(TEXT)).truncate())
                        .on_hover_text(link);
                });
            });
        });
}

#[allow(non_upper_case_globals)]
const Align2C: egui::Align2 = egui::Align2::CENTER_CENTER;

fn stat_card(ui: &mut egui::Ui, ic: &str, label: &str, value: &str) {
    card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(format!("{ic}  {label}")).size(12.0).color(MUTED));
        ui.label(RichText::new(value).size(24.0).strong());
    });
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m {}s", s / 60, s % 60) }
}

/// Reopens the app (needed for a fresh Screen Recording grant to apply).
fn relaunch() {
    let exe = std::env::current_exe().ok();
    // Inside an .app bundle: relaunch the bundle via LaunchServices.
    let bundle = exe
        .as_ref()
        .and_then(|p| p.ancestors().find(|a| a.extension().map(|e| e == "app").unwrap_or(false)))
        .map(|p| p.to_path_buf());
    let spawned = match (&bundle, &exe) {
        (Some(app), _) => std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 1; open -n \"$0\"")
            .arg(app)
            .spawn(),
        (None, Some(exe)) => std::process::Command::new(exe).spawn(),
        _ => return,
    };
    if spawned.is_ok() {
        std::process::exit(0);
    }
}

fn pretty_bundle(bid: &str) -> String {
    if bid == crate::settings::DISCORD {
        return "Discord".into();
    }
    // Windows: "chrome.exe" → "Chrome". macOS: "com.google.Chrome" → "Chrome".
    if let Some(stem) = bid.strip_suffix(".exe").or_else(|| bid.strip_suffix(".EXE")) {
        let mut c = stem.chars();
        return c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    }
    bid.rsplit('.').next().unwrap_or(bid).to_string()
}

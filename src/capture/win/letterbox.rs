//! Black-bar (letterbox) removal, same rules as the macOS backend:
//!
//! * Only while the shared window — or, sharing a screen, the topmost real
//!   window on it — fills its monitor (is fullscreen).
//! * 10 samples a second. Each one copies a few thin strips of the frame to
//!   the CPU: three rows and three columns (at ¼, ½, ¾) to measure the bars,
//!   or, while cropped, the lines just outside the crop.
//! * Crop once both bars of an axis are about equal, at least 2% of the
//!   frame and stable for 0.6 s (0.2 s if they match the last crop at this
//!   size). Undo after 0.3 s of picture past the crop edge, or at once when
//!   the window leaves fullscreen.

use super::sources::{hwnd, monitors, window_rect};
use crate::capture::SourceKind;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::BOOL;

const SAMPLES: usize = 6;
const RECALL: usize = 2;
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
const FS_CHECK_EVERY: Duration = Duration::from_millis(200);
/// Full-range luma at or below this is black (studio-range 24).
const BLACK: u8 = 10;

pub(super) struct Letterbox {
    kind: SourceKind,
    id: u32,
    allowed: bool,
    fs_checked: Instant,
    sampled: Instant,
    ring: [[i32; 4]; SAMPLES], // top, bottom, left, right
    n: usize,
    pos: usize,
    crop: Option<RECT>,
    spill: u32,
    memory: Option<(RECT, u32, u32)>,
    size: (u32, u32),
    rows: Option<(ID3D11Texture2D, u32)>,
    cols: Option<(ID3D11Texture2D, u32)>,
}

impl Letterbox {
    pub fn new(kind: SourceKind, id: u32) -> Letterbox {
        let past = Instant::now() - Duration::from_secs(1);
        Letterbox {
            kind,
            id,
            allowed: false,
            fs_checked: past,
            sampled: past,
            ring: [[0; 4]; SAMPLES],
            n: 0,
            pos: 0,
            crop: None,
            spill: 0,
            memory: None,
            size: (0, 0),
            rows: None,
            cols: None,
        }
    }

    /// Current crop in frame pixels, if any.
    pub fn crop(&self) -> Option<RECT> {
        self.crop
    }

    /// Feeds the latest frame (`w`×`h`). Returns true when the crop changed.
    pub fn update(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, tex: &ID3D11Texture2D, w: u32, h: u32) -> bool {
        let mut changed = false;
        if self.size != (w, h) {
            // New frame size: old samples and the crop no longer apply.
            self.size = (w, h);
            self.n = 0;
            self.pos = 0;
            changed = self.crop.take().is_some();
        }
        if self.fs_checked.elapsed() >= FS_CHECK_EVERY {
            self.fs_checked = Instant::now();
            let fs = self.is_fullscreen();
            if fs != self.allowed {
                self.allowed = fs;
                self.n = 0;
                self.pos = 0;
                if !fs && self.crop.take().is_some() {
                    changed = true;
                }
            }
        }
        if !self.allowed || self.sampled.elapsed() < SAMPLE_EVERY || w < 64 || h < 64 {
            return changed;
        }
        self.sampled = Instant::now();
        let (wi, hi) = (w as i32, h as i32);

        if let Some(c) = self.crop {
            // Undo only when the picture grows past a cropped edge: check a
            // line just outside each edge (controls drawn further out in a
            // bar don't count).
            let mut rows = Vec::new();
            let mut cols = Vec::new();
            if c.top >= 8 {
                rows.push(c.top - 6);
                rows.push(c.bottom + 5);
            }
            if c.left >= 8 {
                cols.push(c.left - 6);
                cols.push(c.right + 5);
            }
            let Some((r, k)) = self.read(device, ctx, tex, w, h, &rows, &cols) else { return changed };
            let spill = r.iter().any(|l| fill(&l[c.left as usize..c.right as usize]) >= 0.5)
                || k.iter().any(|l| fill(&l[c.top as usize..c.bottom as usize]) >= 0.5);
            self.spill = if spill { self.spill + 1 } else { 0 };
            if self.spill >= 3 {
                self.spill = 0;
                self.n = 0;
                self.pos = 0;
                self.crop = None;
                return true;
            }
            return changed;
        }

        let at = [1, 2, 3];
        let rows: Vec<i32> = at.iter().map(|k| hi * k / 4).collect();
        let cols: Vec<i32> = at.iter().map(|k| wi * k / 4).collect();
        let Some((r, k)) = self.read(device, ctx, tex, w, h, &rows, &cols) else { return changed };
        // Columns give top/bottom bars, rows give left/right bars.
        let (mut t, mut b, mut l, mut rr) = (hi, hi, wi, wi);
        for col in &k {
            t = t.min(run(col.iter()) as i32);
            b = b.min(run(col.iter().rev()) as i32);
        }
        for row in &r {
            l = l.min(run(row.iter()) as i32);
            rr = rr.min(run(row.iter().rev()) as i32);
        }
        if t >= hi / 2 || l >= wi / 2 {
            return changed; // black frame: no information
        }
        self.ring[self.pos] = [t, b, l, rr];
        self.pos = (self.pos + 1) % SAMPLES;
        self.n = (self.n + 1).min(SAMPLES);

        let remembered = self.memory.filter(|m| m.1 == w && m.2 == h).map(|m| m.0);
        if self.n < if remembered.is_some() { RECALL } else { SAMPLES } {
            return changed;
        }
        if let Some(m) = remembered {
            let (v, hz) = self.bars(RECALL, hi, wi);
            if (v > 0 || hz > 0) && rect(v, hz, wi, hi) == m {
                self.crop = Some(m);
                return true;
            }
            if self.n < SAMPLES {
                return changed;
            }
        }
        let (v, hz) = self.bars(SAMPLES, hi, wi);
        if v == 0 && hz == 0 {
            return changed;
        }
        let c = rect(v, hz, wi, hi);
        self.memory = Some((c, w, h));
        self.crop = Some(c);
        true
    }

    /// Stable bar sizes (vertical, horizontal) over the last k samples.
    fn bars(&self, k: usize, h: i32, w: i32) -> (i32, i32) {
        let last = |i: usize| -> Vec<i32> {
            (0..k).map(|j| self.ring[(self.pos + SAMPLES - k + j) % SAMPLES][i]).collect()
        };
        (stable(&last(0), &last(1), h), stable(&last(2), &last(3), w))
    }

    /// Is the shared window fullscreen, or (sharing a screen) the topmost
    /// real window on that screen?
    fn is_fullscreen(&self) -> bool {
        match self.kind {
            SourceKind::Window => {
                let h = hwnd(self.id);
                let Some(r) = window_rect(h) else { return false };
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                let m = unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) };
                unsafe { GetMonitorInfoW(m, &mut mi) }.as_bool() && fills(&r, &mi.rcMonitor)
            }
            SourceKind::Display => {
                let Some(m) = monitors().into_iter().nth(self.id.saturating_sub(1) as usize) else { return false };
                top_window_on(&m.rect).is_some_and(|r| fills(&r, &m.rect))
            }
            SourceKind::TestPattern => false,
        }
    }

    /// Copies the given rows and columns of `tex` to the CPU as luma.
    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
        rows: &[i32],
        cols: &[i32],
    ) -> Option<(Vec<Vec<u8>>, Vec<Vec<u8>>)> {
        let rows: Vec<i32> = rows.iter().copied().filter(|&y| y >= 0 && y < h as i32).collect();
        let cols: Vec<i32> = cols.iter().copied().filter(|&x| x >= 0 && x < w as i32).collect();
        let rows_tex = staging(device, &mut self.rows, w, 4, w)?;
        let cols_tex = staging(device, &mut self.cols, 4, h, h)?;
        unsafe {
            for (i, &y) in rows.iter().enumerate() {
                let b = D3D11_BOX { left: 0, top: y as u32, front: 0, right: w, bottom: y as u32 + 1, back: 1 };
                ctx.CopySubresourceRegion(&rows_tex, 0, 0, i as u32, 0, tex, 0, Some(&b));
            }
            for (i, &x) in cols.iter().enumerate() {
                let b = D3D11_BOX { left: x as u32, top: 0, front: 0, right: x as u32 + 1, bottom: h, back: 1 };
                ctx.CopySubresourceRegion(&cols_tex, 0, i as u32, 0, 0, tex, 0, Some(&b));
            }
        }
        let r = map_luma(ctx, &rows_tex, |data, pitch| {
            (0..rows.len()).map(|i| (0..w as usize).map(|x| luma(&data[i * pitch + x * 4..])).collect()).collect()
        })?;
        let k = map_luma(ctx, &cols_tex, |data, pitch| {
            (0..cols.len()).map(|i| (0..h as usize).map(|y| luma(&data[y * pitch + i * 4..])).collect()).collect()
        })?;
        Some((r, k))
    }
}

/// A CPU-readable BGRA texture of `w`×`h`, recreated when `key` changes.
fn staging(device: &ID3D11Device, slot: &mut Option<(ID3D11Texture2D, u32)>, w: u32, h: u32, key: u32) -> Option<ID3D11Texture2D> {
    if slot.as_ref().map(|s| s.1) != Some(key) {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)).ok()? };
        *slot = Some((t?, key));
    }
    slot.as_ref().map(|s| s.0.clone())
}

fn map_luma<T>(ctx: &ID3D11DeviceContext, tex: &ID3D11Texture2D, f: impl FnOnce(&[u8], usize) -> T) -> Option<T> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()? };
    let pitch = m.RowPitch as usize;
    let data = unsafe { std::slice::from_raw_parts(m.pData as *const u8, pitch * desc.Height as usize) };
    let out = f(data, pitch);
    unsafe { ctx.Unmap(tex, 0) };
    Some(out)
}

/// BT.709 luma of a BGRA pixel, full range.
fn luma(p: &[u8]) -> u8 {
    ((p[2] as u32 * 54 + p[1] as u32 * 183 + p[0] as u32 * 19) >> 8) as u8
}

/// Number of black pixels from the start, up to half the line.
fn run<'a>(it: impl ExactSizeIterator<Item = &'a u8>) -> usize {
    let half = it.len() / 2;
    it.take(half).take_while(|&&v| v <= BLACK).count()
}

/// Fraction of 16 evenly spaced points on the line that are not black.
fn fill(line: &[u8]) -> f64 {
    if line.is_empty() {
        return 0.0;
    }
    let lit = (0..16).filter(|k| line[line.len() * (2 * k + 1) / 32] > BLACK).count();
    lit as f64 / 16.0
}

/// A bar size if both sides are similar, at least 2% of the extent and
/// within 4 px across all samples; 0 otherwise.
fn stable(a: &[i32], b: &[i32], extent: i32) -> i32 {
    let (amn, amx) = (*a.iter().min().unwrap(), *a.iter().max().unwrap());
    let (bmn, bmx) = (*b.iter().min().unwrap(), *b.iter().max().unwrap());
    let m = amn.min(bmn);
    if m < extent / 50 || amx - amn > 4 || bmx - bmn > 4 || (amn - bmn).abs() > (extent / 100).max(4) {
        return 0;
    }
    // +2: the first row/column past a bar is often a dark blend of the edge.
    (m + 2) & !1
}

fn rect(v: i32, h: i32, w: i32, hgt: i32) -> RECT {
    RECT { left: h, top: v, right: h + ((w - 2 * h) & !1), bottom: v + ((hgt - 2 * v) & !1) }
}

/// Whether window bounds `r` cover monitor `m` (within 2 px).
fn fills(r: &RECT, m: &RECT) -> bool {
    r.left <= m.left + 2 && r.top <= m.top + 2 && r.right >= m.right - 2 && r.bottom >= m.bottom - 2
}

/// Bounds of the topmost real window on monitor `m`: visible, not cloaked,
/// not click-through (overlays such as GeForce Experience or Discord's), not
/// the desktop or taskbar, and covering at least 10% of the monitor.
fn top_window_on(m: &RECT) -> Option<RECT> {
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HWND>)).push(h) };
        BOOL(1)
    }
    let mut all: Vec<HWND> = Vec::new();
    // EnumWindows walks top-level windows in z-order, topmost first.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
    }
    let area = ((m.right - m.left) as i64 * (m.bottom - m.top) as i64).max(1);
    for h in all {
        unsafe {
            if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                continue;
            }
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if ex & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4);
            if cloaked != 0 {
                continue;
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(h, &mut class);
            let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
            if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
                continue;
            }
            let Some(r) = window_rect(h) else { continue };
            let iw = (r.right.min(m.right) - r.left.max(m.left)).max(0) as i64;
            let ih = (r.bottom.min(m.bottom) - r.top.max(m.top)).max(0) as i64;
            if iw * ih * 10 < area {
                continue;
            }
            return Some(r);
        }
    }
    None
}

use windows::Win32::Foundation::LPARAM;

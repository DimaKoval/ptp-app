//! Screens, windows and apps to choose from, and their thumbnails.

use super::super::*;
use std::collections::BTreeMap;
use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PWSTR;
use windows_core::BOOL;

pub(crate) fn hwnd(id: u32) -> HWND {
    HWND(id as usize as *mut c_void)
}

pub(crate) struct Monitor {
    pub handle: HMONITOR,
    pub rect: RECT,
    pub primary: bool,
}

/// Monitors in system order. A display's id is its index + 1.
pub(crate) fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(h) };
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut handles as *mut _ as isize));
    }
    handles
        .into_iter()
        .filter_map(|h| {
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool().then(|| Monitor {
                handle: h,
                rect: mi.rcMonitor,
                primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
            })
        })
        .collect()
}

/// Executable file name ("chrome.exe") of a process, lowercase.
pub(crate) fn process_exe(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_lowercase())
    }
}

fn pretty_exe(exe: &str) -> String {
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    let mut c = stem.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub(crate) fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut c_void, size_of::<RECT>() as u32)
            .ok()
            .or_else(|| GetWindowRect(h, &mut r).ok())?;
    }
    Some(r)
}

struct Win {
    hwnd: HWND,
    title: String,
    exe: String,
    rect: RECT,
}

fn shareable_windows() -> Vec<Win> {
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HWND>)).push(h) };
        BOOL(1)
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
    }
    let me = unsafe { GetCurrentProcessId() };
    let mut out = Vec::new();
    for h in all {
        unsafe {
            if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                continue;
            }
            if GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
                continue;
            }
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4);
            if cloaked != 0 {
                continue;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(h, &mut buf);
            if n <= 0 {
                continue;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let Some(rect) = window_rect(h) else { continue };
            if rect.right - rect.left < 120 || rect.bottom - rect.top < 80 {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == me {
                continue;
            }
            let exe = process_exe(pid).unwrap_or_default();
            out.push(Win { hwnd: h, title, exe, rect });
        }
    }
    out
}

pub fn list_sources() -> anyhow::Result<SourceList> {
    let displays = monitors()
        .iter()
        .enumerate()
        .map(|(i, m)| DisplayInfo {
            id: i as u32 + 1,
            name: if m.primary { format!("Display {} (main)", i + 1) } else { format!("Display {}", i + 1) },
            width: (m.rect.right - m.rect.left) as u32,
            height: (m.rect.bottom - m.rect.top) as u32,
            is_main: m.primary,
        })
        .collect();
    let wins = shareable_windows();
    let mut apps: BTreeMap<String, AppInfo> = BTreeMap::new();
    for w in &wins {
        if !w.exe.is_empty() {
            apps.entry(w.exe.clone()).or_insert_with(|| AppInfo { bundle_id: w.exe.clone(), name: pretty_exe(&w.exe) });
        }
    }
    let mut apps: Vec<AppInfo> = apps.into_values().collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    let windows = wins
        .into_iter()
        .map(|w| WindowInfo {
            id: w.hwnd.0 as usize as u32,
            title: w.title,
            app_name: pretty_exe(&w.exe),
            bundle_id: w.exe,
            width: (w.rect.right - w.rect.left) as u32,
            height: (w.rect.bottom - w.rect.top) as u32,
        })
        .collect();
    Ok(SourceList { displays, windows, apps })
}

/// A 32-bit top-down DIB section with its pixel pointer.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    unsafe fn new(w: i32, h: i32) -> Option<Dib> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Dib { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

pub fn thumbnail(kind: SourceKind, id: u32, max_w: u32, max_h: u32) -> Option<Thumbnail> {
    unsafe {
        let full = match kind {
            SourceKind::Display => {
                let m = monitors().into_iter().nth(id.checked_sub(1)? as usize)?;
                let (w, h) = (m.rect.right - m.rect.left, m.rect.bottom - m.rect.top);
                let dib = Dib::new(w, h)?;
                let screen = GetDC(None);
                let _ = BitBlt(dib.dc, 0, 0, w, h, Some(screen), m.rect.left, m.rect.top, SRCCOPY | CAPTUREBLT);
                ReleaseDC(None, screen);
                dib
            }
            SourceKind::Window => {
                let h = hwnd(id);
                let mut r = RECT::default();
                GetWindowRect(h, &mut r).ok()?;
                let dib = Dib::new((r.right - r.left).max(1), (r.bottom - r.top).max(1))?;
                // PW_RENDERFULLCONTENT (2): works for GPU-rendered windows too.
                if !PrintWindow(h, dib.dc, PRINT_WINDOW_FLAGS(2)).as_bool() {
                    return None;
                }
                dib
            }
            SourceKind::TestPattern => return None,
        };
        let (tw, th) = super::fit_size(full.w as u32, full.h as u32, max_w, max_h);
        let small = Dib::new(tw as i32, th as i32)?;
        SetStretchBltMode(small.dc, HALFTONE);
        let _ = StretchBlt(small.dc, 0, 0, small.w, small.h, Some(full.dc), 0, 0, full.w, full.h, SRCCOPY);
        let n = (tw * th) as usize;
        let src = std::slice::from_raw_parts(small.bits, n * 4);
        let mut rgba = vec![0u8; n * 4];
        for i in 0..n {
            rgba[i * 4] = src[i * 4 + 2];
            rgba[i * 4 + 1] = src[i * 4 + 1];
            rgba[i * 4 + 2] = src[i * 4];
            rgba[i * 4 + 3] = 255;
        }
        Some(Thumbnail { width: tw, height: th, rgba })
    }
}

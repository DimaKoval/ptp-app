//! Audio capture with WASAPI.
//!
//! System audio uses process loopback (Windows 10 2004+): everything except
//! one app's process tree (the first excluded app that is running, Discord
//! by default), or a single app's tree only. Older Windows falls back to the
//! plain loopback of the default output device (excluded apps can be heard).

use super::{Config, Shared};
use crate::capture::AudioCapture;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, Ref, Result, implement};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const RATE: u32 = 48_000;

pub(super) fn run_system(cfg: Config, shared: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    if let Err(e) = system_inner(&cfg, &shared) {
        (shared.callbacks.error)(format!("System audio capture failed: {e}"));
    }
    unsafe { CoUninitialize() };
}

pub(super) fn run_mic(shared: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let r = (|| -> Result<()> {
        let (client, fmt) = endpoint_client(eCapture, false)?;
        capture_loop(&client, fmt, true, &shared)
    })();
    if let Err(e) = r {
        (shared.callbacks.error)(format!("Microphone capture failed: {e}"));
    }
    unsafe { CoUninitialize() };
}

fn system_inner(cfg: &Config, shared: &Arc<Shared>) -> Result<()> {
    let procs = process_table();
    let (pid, mode) = match cfg.audio {
        AudioCapture::SingleApp => {
            let app = cfg.audio_app.clone().unwrap_or_default().to_lowercase();
            match root_pid(&procs, &app) {
                Some(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
                None => {
                    (shared.callbacks.error)(format!("{app} is not running, so there is no app audio to share"));
                    return Ok(());
                }
            }
        }
        _ => {
            let excluded = cfg.excluded_apps.iter().find_map(|a| root_pid(&procs, &a.to_lowercase()));
            // Nothing to exclude: exclude ourselves (we play no audio anyway).
            (excluded.unwrap_or_else(|| unsafe { GetCurrentProcessId() }), PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)
        }
    };
    match process_loopback_client(pid, mode) {
        Ok(client) => {
            let fmt = Format { rate: RATE, channels: 2, float: true, bits: 32 };
            capture_loop(&client, fmt, false, shared)
        }
        Err(e) if cfg.audio != AudioCapture::SingleApp => {
            eprintln!("process loopback unavailable ({e}); capturing all system audio");
            let (client, fmt) = endpoint_client(eRender, true)?;
            capture_loop(&client, fmt, false, shared)
        }
        Err(e) => Err(e),
    }
}

/// pid → (parent pid, exe name lowercase)
fn process_table() -> HashMap<u32, (u32, String)> {
    let mut map = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase();
                map.insert(e.th32ProcessID, (e.th32ParentProcessID, exe));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    map
}

/// The top process of an app (its parent is not the same app), whose tree
/// covers the helper processes that actually play the sound.
fn root_pid(procs: &HashMap<u32, (u32, String)>, exe: &str) -> Option<u32> {
    if exe.is_empty() {
        return None;
    }
    let mut roots: Vec<u32> = procs
        .iter()
        .filter(|(_, (parent, name))| name == exe && procs.get(parent).map(|(_, p)| p != exe).unwrap_or(true))
        .map(|(pid, _)| *pid)
        .collect();
    roots.sort();
    roots.first().copied()
}

// windows-rs `implement` objects are free-threaded (agile), as required here.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

fn process_loopback_client(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
            },
        };
        let prop = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        };
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&prop), &handler)?;
        // The PROPVARIANT only borrows `params`; don't let its Drop free it.
        std::mem::forget(prop);
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| windows::core::Error::new(windows::core::HRESULT(-1), "audio activation timed out"))?;
        let mut hr = windows::core::HRESULT(0);
        let mut iface = None;
        op.GetActivateResult(&mut hr, &mut iface)?;
        hr.ok()?;
        let client: IAudioClient = iface
            .ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "no audio client"))?
            .cast()?;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            200_000,
            0,
            &fmt,
            None,
        )?;
        Ok(client)
    }
}

#[derive(Clone, Copy)]
struct Format {
    rate: u32,
    channels: u16,
    float: bool,
    bits: u16,
}

/// Default output (loopback) or input device, in its own mix format.
fn endpoint_client(flow: EDataFlow, loopback: bool) -> Result<(IAudioClient, Format)> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(flow, eConsole)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let m = &*mix;
        let float = m.wFormatTag == WAVE_FORMAT_IEEE_FLOAT
            || (m.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16
                && std::ptr::read_unaligned(std::ptr::addr_of!((*(mix as *const WAVEFORMATEXTENSIBLE)).SubFormat))
                    == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        let fmt = Format { rate: m.nSamplesPerSec, channels: m.nChannels, float, bits: m.wBitsPerSample };
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        r?;
        Ok((client, fmt))
    }
}

fn capture_loop(client: &IAudioClient, fmt: Format, is_mic: bool, shared: &Arc<Shared>) -> Result<()> {
    unsafe {
        let event: HANDLE = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let ch = fmt.channels.max(1) as usize;
        let mut stereo: Vec<f32> = Vec::new();
        while !shared.stop.load(Ordering::SeqCst) {
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let n = cap.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let frames_us = frames as usize;
                stereo.clear();
                stereo.resize(frames_us * 2, 0.0);
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                    for f in 0..frames_us {
                        for c in 0..2 {
                            let src_c = c.min(ch - 1);
                            let idx = f * ch + src_c;
                            stereo[f * 2 + c] = match (fmt.float, fmt.bits) {
                                (true, 32) => *(data as *const f32).add(idx),
                                (false, 16) => *(data as *const i16).add(idx) as f32 / 32768.0,
                                (false, 32) => *(data as *const i32).add(idx) as f32 / 2_147_483_648.0,
                                (false, 24) => {
                                    let p = data.add(idx * 3);
                                    ((*p as i32) << 8 | (*p.add(1) as i32) << 16 | (*p.add(2) as i32) << 24) as f32 / 2_147_483_648.0
                                }
                                _ => 0.0,
                            };
                        }
                    }
                }
                cap.ReleaseBuffer(frames)?;
                if !shared.stop.load(Ordering::SeqCst) {
                    (shared.callbacks.audio)(&stereo, fmt.rate, super::qpc_us(), is_mic);
                }
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}

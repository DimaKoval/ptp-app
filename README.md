# ptp-app

The native host app for **PTP Share**: peer-to-peer screen sharing for a few
friends, with no server to run. Viewers watch in a browser using the viewer
page from [ptp-web](https://github.com/DimaKoval/ptp-web), which is served at
https://dimakoval.github.io/ptp-web/.

- **Capture:** ScreenCaptureKit captures a screen or window, and VideoToolbox
  encodes H.264 on the GPU. Audio is encoded as Opus.
- **Streaming:** each viewer gets a direct WebRTC connection (`webrtc-rs`).
  The video is encoded once and sent to everyone.
- **Signaling:** the offer and answer travel over public Nostr relays as
  encrypted, ephemeral messages. The room secret lives only in the link's
  `#fragment`.

```
host app ──(encrypted offer/answer via public Nostr relays)── browser
    └──────────── direct WebRTC (DTLS-SRTP): video + audio ──────────┘
```

## Building and running (macOS 13+)

You need Rust (`rustup`) and `cmake`, which is used to build libopus
statically.

```bash
scripts/run-macos.sh
```

This builds `target/release/ptp-share.app`, signs it, and launches it. Launch
it this way rather than with `cargo run`, so that macOS asks for Screen
Recording permission for **ptp-share** itself instead of for your terminal.

Signing uses `$PTP_SIGN_IDENTITY` if it's set. Otherwise it uses an
"Apple Development" certificate (create one in Xcode → Settings → Accounts).
If neither exists, it signs ad-hoc. With a real certificate, the permission
survives rebuilds.

If macOS keeps refusing access, reset the permission and grant it again:

```bash
tccutil reset ScreenCapture dev.ptpshare.host
```

## Sharing the Mac app with friends

```bash
./scripts/package-macos.sh
```

Builds `dist/ptp-share-<version>-mac.zip`: one app for Apple Silicon and Intel
Macs (macOS 13+), signed with your Apple Development certificate (the free one
Xcode creates). A stable signature means friends grant Screen Recording once
and it survives updates.

It is not notarized (that needs the paid Apple Developer Program), so your
friend does this once per version:

1. Unzip and drag `ptp-share.app` into Applications.
2. Open it; macOS says it can't verify the app. Click **Done**.
3. System Settings › Privacy & Security › scroll down › **Open Anyway** next to
   the ptp-share message (on macOS 15+, right-click › Open no longer works).
4. Start sharing, allow **Screen Recording**, then quit and reopen the app.

Or, instead of steps 2–3: `xattr -dr com.apple.quarantine /Applications/ptp-share.app`

Bump `version` in `Cargo.toml` before packaging a new release.

## Windows (10 2004+ / 11)

Download `ptp-share.exe` from the latest
[Windows build](https://github.com/DimaKoval/ptp-app/actions/workflows/windows.yml)
(or a [release](https://github.com/DimaKoval/ptp-app/releases)) and run it.
It's a single self-contained file: no installer, no runtime to install.

- Video: Windows.Graphics.Capture → GPU colour conversion → Media
  Foundation H.264 (hardware encoder when available, software otherwise).
- Audio: WASAPI process loopback: system audio minus Discord, or one app.
- Windows SmartScreen may warn the first time, because the exe isn't
  code-signed: click "More info" → "Run anyway".

Build it yourself on Windows with `cargo build --release`
(`target/release/ptp-share.exe`), or cross-compile from macOS:

```bash
brew install mingw-w64 && rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Settings

- **Resolution:** 720p, 1080p, 1440p (default) or 4K. The picture is scaled
  to fit and never upscaled.
- **Frame rate:** 30, 60 or 120 fps.
- **Bitrate:** your upload needs about bitrate × viewers.
- **Audio:** off, system audio minus excluded apps (Discord is excluded by
  default), or a single app. The microphone is used only if you turn on
  "Include my microphone" (macOS 15+).
- **Network:** viewer page URL (default:
  `https://dimakoval.github.io/ptp-web/`), an optional TURN server, and the
  signaling relays.

**Changing things while live:** resolution, frame rate, bitrate, audio and
the shared screen or window (the **Change** button) all apply immediately.
The app starts the new capture next to the running one and switches over, so
viewers stay connected and see no gap. Network settings apply next time.

**Black bars** around a video (letterboxing) are cropped off automatically,
but only while the shared window, or the top window on a shared screen, is
fullscreen. The crop lands about 0.6 s after the bars are stable (player
controls hidden), and goes away as soon as the window leaves fullscreen.

**Your link** stays the same every time you go live, so friends can bookmark
it; a viewer page left open reconnects by itself when you go live again. The
refresh button next to the link makes a new one (the old link stops working).

Settings are saved to `~/Library/Application Support/ptp-share/settings.json`.

## Testing without screen permission

```bash
cargo run -- --test-pattern                         # uses the viewer URL from settings
cargo run -- --test-pattern http://localhost:8080/  # local viewer
```

This streams moving color bars plus a beep, and prints the room link.

To stream your real main display and system audio without the UI (useful for
checking quality), launch the bundle so it uses the app's Screen Recording
permission:

```bash
open -n --stdout /dev/stdout target/release/ptp-share.app --args --display http://localhost:8080/
```

`--window <app name>` does the same for the first window of that app (handy for
testing resizes). It prints the room link plus audio bitrate, packets per second and dropped
audio chunks every 5 seconds.

## Layout

| Path | What |
|---|---|
| `native/macos/` | Objective-C layer: ScreenCaptureKit capture + VideoToolbox encoder, exposed through a C ABI (`ptp_capture.h`) |
| `src/capture/macos.rs` | Safe Rust wrapper for that layer |
| `src/capture/win/` | Windows backend: sources, WGC video, Media Foundation encoder, WASAPI audio |
| `src/session.rs` | One WebRTC peer per viewer, sending the same encoded stream to all |
| `src/signaling.rs` | Nostr relay pool, AES-GCM encryption, room links |
| `src/audio.rs` | Mixing, resampling, Opus encoding |
| `src/ui.rs` | egui interface |
| `scripts/` | Build and launch an `.app` bundle |

## Known limitations

- There's no TURN server by default. Strict NATs (CGNAT, mobile hotspots)
  need one set in settings.
- Every viewer gets the same bitrate.
- Windows: only one excluded app is left out of system audio (the first one
  running, Discord by default), and only if it was running when sharing
  started.

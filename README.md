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

## Settings

- **Resolution:** 720p, 1080p or 1440p. The picture is scaled to fit and
  never upscaled.
- **Frame rate:** 30, 60 or 120 fps.
- **Bitrate:** adjustable while live. Your upload needs about bitrate ×
  viewers.
- **Audio:** off, system audio minus excluded apps (Discord is excluded by
  default), or a single app. The microphone is used only if you turn on
  "Include my microphone" (macOS 15+).
- **Network:** viewer page URL (default:
  `https://dimakoval.github.io/ptp-web/`), an optional TURN server, and the
  signaling relays.

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

It prints the room link plus audio bitrate, packets per second and dropped
audio chunks every 5 seconds.

## Layout

| Path | What |
|---|---|
| `native/macos/` | Objective-C layer: ScreenCaptureKit capture + VideoToolbox encoder, exposed through a C ABI (`ptp_capture.h`) |
| `src/capture.rs` | Safe Rust wrapper for that layer |
| `src/session.rs` | One WebRTC peer per viewer, sending the same encoded stream to all |
| `src/signaling.rs` | Nostr relay pool, AES-GCM encryption, room links |
| `src/audio.rs` | Mixing, resampling, Opus encoding |
| `src/ui.rs` | egui interface |
| `scripts/` | Build and launch an `.app` bundle |

## Known limitations

- There's no TURN server by default. Strict NATs (CGNAT, mobile hotspots)
  need one set in settings.
- Every viewer gets the same bitrate.
- The Windows host isn't built yet. It needs a Windows implementation of the
  `ptp_capture.h` layer (Windows.Graphics.Capture + Media Foundation).

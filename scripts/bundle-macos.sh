#!/usr/bin/env bash
# Builds "ptp-share.app" (release) so macOS attributes Screen Recording /
# Microphone permissions to the app instead of your terminal.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

# PTP_UNIVERSAL=1 builds one binary for Apple Silicon + Intel Macs.
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
if [ "${PTP_UNIVERSAL:-0}" = "1" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  OUT="target/universal"
  mkdir -p "$OUT"
  lipo -create -output "$OUT/ptp-share" \
    target/aarch64-apple-darwin/release/ptp-share \
    target/x86_64-apple-darwin/release/ptp-share
else
  cargo build --release
  OUT="target/release"
fi

APP="$OUT/ptp-share.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$OUT/ptp-share" "$APP/Contents/MacOS/ptp-share"
cp assets/icon.icns "$APP/Contents/Resources/ptp-share.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ptp-share</string>
  <key>CFBundleDisplayName</key><string>ptp-share</string>
  <key>CFBundleIdentifier</key><string>dev.ptpshare.host</string>
  <key>CFBundleExecutable</key><string>ptp-share</string>
  <key>CFBundleIconFile</key><string>ptp-share</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Only used if you turn on "Include my microphone".</string>
</dict>
</plist>
PLIST

# Ad-hoc signature. macOS ties permissions to the signature, so after a
# rebuild you may need to re-enable Screen Recording for the app.
# Prefer a real signing identity so the Screen Recording grant survives rebuilds:
#   $PTP_SIGN_IDENTITY, else an "Apple Development" cert (Xcode), else a
#   self-signed "ptp-share dev" cert (Keychain Access), else ad-hoc.
IDENTITY="${PTP_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
  IDS=$(security find-identity -p codesigning 2>/dev/null || true)
  IDENTITY=$(echo "$IDS" | grep -o '"Apple Development: [^"]*"' | head -1 | tr -d '"' || true)
  if [ -z "$IDENTITY" ] && echo "$IDS" | grep -q '"ptp-share dev"'; then
    IDENTITY="ptp-share dev"
  fi
fi
if [ -n "$IDENTITY" ]; then
  codesign --force --deep --sign "$IDENTITY" "$APP"
  echo "Signed with: $IDENTITY"
else
  codesign --force --deep --sign - "$APP"
  echo "Signed ad-hoc (no certificate found; Screen Recording may need re-enabling after rebuilds)"
fi
echo "Built: $APP"

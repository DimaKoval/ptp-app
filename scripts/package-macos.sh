#!/usr/bin/env bash
# Builds a zip of ptp-share.app to send to friends: one app for Apple Silicon
# and Intel Macs (macOS 13+), signed with your Apple Development certificate
# so their Screen Recording permission survives updates. Not notarized (that
# needs a paid Apple Developer account), so on first launch they approve it
# in System Settings › Privacy & Security › Open Anyway.
#   Output: dist/ptp-share-<version>-mac.zip
set -euo pipefail
cd "$(dirname "$0")/.."
PTP_UNIVERSAL=1 ./scripts/bundle-macos.sh

APP="target/universal/ptp-share.app"
codesign --verify --strict "$APP"
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
mkdir -p dist
ZIP="dist/ptp-share-$VERSION-mac.zip"
rm -f "$ZIP"
# ditto keeps the signature intact (plain zip can break it).
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
echo "Architectures: $(lipo -archs "$APP/Contents/MacOS/ptp-share")"
echo "Package: $ZIP ($(du -h "$ZIP" | cut -f1))"

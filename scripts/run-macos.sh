#!/usr/bin/env bash
# Builds ptp-share.app and launches it as its own app, so macOS asks for
# Screen Recording permission for "ptp-share" (not for your terminal).
# Logs: ~/Library/Logs/ptp-share.log
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/bundle-macos.sh
LOG="$HOME/Library/Logs/ptp-share.log"
pkill -x ptp-share 2>/dev/null || true
open -n --stdout "$LOG" --stderr "$LOG" "target/release/ptp-share.app"
echo "Launched. Logs: $LOG"

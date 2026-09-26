#!/usr/bin/env bash
# Build the iOS app and run it on the simulator. Reuses an already-booted
# simulator if one is running; only boots a fallback device when none are.
#
# Run ./ios/build-core.sh first if you changed the Rust app-core.
# The server should be running separately:  cargo run -p server
set -euo pipefail
cd "$(dirname "$0")/ScoreboardApp"

PROJ=ScoreboardApp.xcodeproj
BUNDLE=com.markschmidt.scoreone
FALLBACK_SIM=7A2B776B-D436-4EFB-AB0F-8E2770F303DB # iPhone 17 Pro

# Build the Rust core if its xcframework is missing (fresh checkout).
if [ ! -d "../AppCore.xcframework" ]; then
    echo "==> AppCore.xcframework missing; building core"
    ../build-core.sh
fi

echo "==> building app"
if ! xcodebuild -project "$PROJ" -target ScoreboardApp \
    -sdk iphonesimulator -configuration Debug -arch arm64 \
    CODE_SIGNING_ALLOWED=NO build >/tmp/sb_xcodebuild.log 2>&1; then
    tail -40 /tmp/sb_xcodebuild.log
    exit 1
fi

APP=$(xcodebuild -project "$PROJ" -target ScoreboardApp -sdk iphonesimulator \
    -configuration Debug -arch arm64 -showBuildSettings 2>/dev/null \
    | awk -F' = ' '/^[[:space:]]*CODESIGNING_FOLDER_PATH = /{print $2; exit}')
echo "==> app: $APP"

# Reuse a booted simulator if one is running; otherwise boot the fallback.
SIM=$(xcrun simctl list devices booted | grep -iEo '[A-F0-9-]{36}' | head -1 || true)
if [ -z "$SIM" ]; then
    echo "==> no simulator booted; booting fallback"
    SIM=$FALLBACK_SIM
    xcrun simctl bootstatus "$SIM" -b
else
    echo "==> reusing booted simulator"
fi
echo "==> simulator: $SIM"

open -a Simulator
sleep 2

launch_app() {
    xcrun simctl install "$SIM" "$APP" || return 1
    xcrun simctl terminate "$SIM" "$BUNDLE" 2>/dev/null || true
    xcrun simctl launch "$SIM" "$BUNDLE" || return 1
}

# A long-idle simulator can have dead services (Mach -308); reboot + retry once.
if ! launch_app; then
    echo "==> simulator unresponsive; rebooting and retrying"
    xcrun simctl shutdown "$SIM" 2>/dev/null || true
    xcrun simctl bootstatus "$SIM" -b
    launch_app
fi
echo "==> launched $BUNDLE (ensure the server is running: cargo run -p server)"

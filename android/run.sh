#!/usr/bin/env bash
# Build, install, and launch on whatever is attached — the emulator if one is
# running, your phone if it is plugged in with USB debugging on.
#
#   ./run.sh            build + install + launch
#   ./run.sh --emu      boot the "scoreboard" AVD first (creates it if needed)
#   ./run.sh --logs     follow the app's logcat after launching
#
# The Rust core is NOT rebuilt here — run ./build-core.sh after any change to
# crates/app-core or crates/protocol.
set -euo pipefail
cd "$(dirname "$0")"

export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
APP=com.markschmidt.scoreboard

# Gradle needs a JDK 17-21; the brew default is newer than AGP accepts and
# fails with a bare version number for an error message.
java -version 2>&1 | head -1

if [[ "${1:-}" == "--emu" ]]; then
    if ! adb devices | grep -q emulator; then
        "$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" list avd | grep -q "Name: scoreboard" || \
            echo no | "$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd \
                -n scoreboard -k "system-images;android-35;google_apis;arm64-v8a" --force
        nohup emulator -avd scoreboard -no-snapshot -no-boot-anim -gpu swiftshader_indirect >/tmp/emu.log 2>&1 &
        adb wait-for-device
        until [[ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]; do sleep 3; done
    fi
    shift
fi

adb get-state >/dev/null 2>&1 || { echo "no device: plug in a phone (USB debugging on) or run ./run.sh --emu" >&2; exit 1; }

echo "sdk.dir=$ANDROID_HOME" > local.properties
./gradlew assembleDebug -q
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n "$APP/.MainActivity" >/dev/null
echo "launched on $(adb devices | sed -n 2p | cut -f1)"

if [[ "${1:-}" == "--logs" ]]; then
    adb logcat --pid="$(adb shell pidof "$APP")"
fi

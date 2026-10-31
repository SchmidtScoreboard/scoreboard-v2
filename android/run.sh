#!/usr/bin/env bash
# Build, install, and launch on a device.
#
#   ./run.sh                 the attached phone, else a running emulator
#   ./run.sh --emu           force the emulator (booting it if needed)
#   ./run.sh --device SERIAL a specific one (see: adb devices)
#   ./run.sh --logs          follow the app's log after launching
#   ./run.sh --clean         wipe build/ first (see the dex note below)
#   ./run.sh --devices       what adb can see (no PATH setup needed)
#   ./run.sh --release       R8-optimized build — how performance should be
#                            judged; debug Compose is several times slower
#
# The Rust core is NOT rebuilt here — run ./build-core.sh after changing
# crates/app-core or crates/protocol.
set -euo pipefail
cd "$(dirname "$0")"

# AGP accepts JDK 17-21 and reports a newer one as a bare version number with
# no other text, so pin it rather than inherit whatever `java` is.
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
# The installed app id (Play's package name) differs from the Kotlin package
# the classes live in, so the activity is named in full when launching.
APP=com.markschmidt.openscoreboard
ACTIVITY=com.markschmidt.scoreboard.MainActivity

WANT_EMU=0; WANT_LOGS=0; WANT_CLEAN=0; WANT_RELEASE=0; SERIAL=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        # adb lives in the SDK, which is not on a normal PATH — so asking
        # "what does adb see?" has to be possible without setting one up.
        --devices) adb devices -l; exit 0 ;;
        --emu) WANT_EMU=1 ;;
        --logs) WANT_LOGS=1 ;;
        --clean) WANT_CLEAN=1 ;;
        --release) WANT_RELEASE=1 ;;
        --device) SERIAL="${2:-}"; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ "$WANT_EMU" == 1 && -z "$SERIAL" ]] && ! adb devices | grep -q "^emulator-"; then
    AVDM="$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager"
    "$AVDM" list avd 2>/dev/null | grep -q "Name: scoreboard" || \
        echo no | "$AVDM" create avd -n scoreboard \
            -k "system-images;android-35;google_apis;arm64-v8a" --force >/dev/null
    nohup emulator -avd scoreboard -no-snapshot -no-boot-anim -gpu swiftshader_indirect \
        >/tmp/emu.log 2>&1 &
    echo "booting the emulator…"
    adb wait-for-device
    until [[ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]; do sleep 3; done
fi

# Pick a target. `adb install` refuses to guess when several are attached, so
# resolve the serial HERE and pass -s to everything.
# bash 3.2 (what macOS ships) has no mapfile — read the list portably.
DEVICES=()
while IFS= read -r line; do
    [[ -n "$line" ]] && DEVICES+=("$line")
done < <(adb devices | awk '/\tdevice$/ {print $1}')
UNAUTH="$(adb devices | awk '/\tunauthorized$/ {print $1}')"
if [[ -n "$SERIAL" ]]; then
    :
elif [[ "$WANT_EMU" == 1 ]]; then
    SERIAL="$(printf '%s\n' "${DEVICES[@]}" | grep '^emulator-' | head -1 || true)"
elif [[ ${#DEVICES[@]:-0} -eq 1 ]]; then
    SERIAL="${DEVICES[0]}"
elif [[ ${#DEVICES[@]:-0} -gt 1 ]]; then
    # A phone outranks an emulator: if one is plugged in, it is what you meant.
    SERIAL="$(printf '%s\n' "${DEVICES[@]}" | grep -v '^emulator-' | head -1 || true)"
    [[ -n "$SERIAL" ]] || SERIAL="${DEVICES[0]}"
    echo "several devices attached; using $SERIAL (override with --device)"
fi

if [[ -z "${SERIAL:-}" ]]; then
    echo "no usable device." >&2
    if [[ -n "$UNAUTH" ]]; then
        echo "  $UNAUTH is UNAUTHORIZED — unlock the phone and accept the" >&2
        echo "  'Allow USB debugging?' prompt (tick 'always allow')." >&2
    else
        cat >&2 <<'HELP'
  For a phone, all four must be true:
    1. Developer options on   (tap Build number 7x in Settings > About phone)
    2. USB debugging on       (Settings > System > Developer options)
    3. a DATA cable, into the Mac directly (many cables are charge-only,
       and a charger port will never enumerate)
    4. the USB mode set to File transfer / MTP, not "No data transfer"
  Then: ./run.sh --devices   — it should list the phone (adb is inside the
  SDK, so it is not on a normal PATH; that flag needs no setup).
  Or run against the emulator instead:  ./run.sh --emu
HELP
    fi
    exit 1
fi

[[ "$WANT_CLEAN" == 1 ]] && ./gradlew clean -q

echo "sdk.dir=$ANDROID_HOME" > local.properties
# iCloud syncs this repo (it is in ~/Documents) and resolves races by
# duplicating files as "foo 2.ext". Done to a build tree, that surfaces as
# "defined multiple times" or "Failed file name validation" — neither of which
# is a code error. The build tree therefore lives in ~/Library/Caches, which
# iCloud never touches; see android/build.gradle.kts. Any stragglers from
# before that change get swept here.
find . -name "* [0-9].*" -delete 2>/dev/null || true

VARIANT=Debug; APKDIR=debug; APKNAME=app-debug.apk
if [[ "$WANT_RELEASE" == 1 ]]; then VARIANT=Release; APKDIR=release; APKNAME=app-release.apk; fi
if ! ./gradlew "assemble$VARIANT" -q 2>/tmp/gradle-err.txt; then
    cat /tmp/gradle-err.txt >&2
    exit 1
fi

APK="$HOME/Library/Caches/scoreboard-android/app/outputs/apk/$APKDIR/$APKNAME"
[[ -f "$APK" ]] || { echo "no APK at $APK" >&2; exit 1; }
adb -s "$SERIAL" install -r "$APK"
adb -s "$SERIAL" shell am start -n "$APP/$ACTIVITY" >/dev/null
echo "launched on $SERIAL"

if [[ "$WANT_LOGS" == 1 ]]; then
    adb -s "$SERIAL" logcat --pid="$(adb -s "$SERIAL" shell pidof "$APP" | tr -d '\r')"
fi

#!/usr/bin/env bash
# Build + flash the CURRENT firmware (firmware-rs) to the MatrixPortal S3 over USB.
#
# (The old v1 PlatformIO project in ../firmware is retired — flashing it leaves
# the panel dark: it joins Wi-Fi with baked-in creds and waits on a server that
# no longer exists.)
#
# Frees the serial port first (a running monitor locks it -> connect errors),
# then builds and uploads. NOTE: this native-USB S3 does NOT auto-boot after
# flashing, so TAP RESET when it finishes. If it can't enter the bootloader to
# flash, hold BOOT + tap RESET on the board, then re-run this.
set -euo pipefail
cd "$(dirname "$0")/../firmware-rs"

PORT="${1:-$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)}"
if [[ -z "$PORT" ]]; then
    echo "No /dev/cu.usbmodem* port found — is the board plugged in?" >&2
    exit 1
fi

echo "==> building"
# xtensa-esp32s3-elf-gcc must be on PATH for the link step.
[[ -f "$HOME/export-esp.sh" ]] && source "$HOME/export-esp.sh"
cargo build --release

echo "==> freeing serial port (closing any monitors)"
pkill -f "device monitor" 2>/dev/null || true
lsof -t /dev/cu.usbmodem* 2>/dev/null | xargs -r kill 2>/dev/null || true
sleep 1

echo "==> flashing via $PORT"
espflash flash -p "$PORT" target/xtensa-esp32s3-none-elf/release/firmware-rs

echo
echo "=============================================================="
echo "  Flash complete -> now TAP THE RESET button to boot it."
echo "=============================================================="

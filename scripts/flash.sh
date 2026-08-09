#!/usr/bin/env bash
# Build + flash the CURRENT firmware (firmware-rs) to the MatrixPortal S3 over USB.
#
# (The old v1 PlatformIO project in ../firmware is retired — flashing it leaves
# the panel dark: it joins Wi-Fi with baked-in creds and waits on a server that
# no longer exists.)
#
# Frees the serial port first (a running monitor locks it -> connect errors),
# then builds and uploads. espflash defaults to `--after hard-reset`, so the
# board normally reboots into the new image on its own; if the panel stays dark,
# tap RESET (most likely after you had to enter the bootloader by hand). If it
# can't enter the bootloader to flash at all, hold BOOT + tap RESET, then re-run.
#
# WHICH SERVER the image talks to is a BUILD-TIME choice (src/config.rs), so it
# is set here and cannot be changed on the device:
#
#   scripts/flash.sh                     -> dev: your LAN server ($SCOREBOARD_DEV_HOST),
#                                           needs SCOREBOARD_DEV_TLS_PIN
#   FEATURES=prod SCOREBOARD_TLS_PIN=… scripts/flash.sh
#                                        -> prod: the hosted server, i.e. an image
#                                           identical to what OTA ships. This is what
#                                           you want for a spare/test board that
#                                           should behave like a real one.
#
# Re-derive the prod pin any time from the deployed certificate:
#   railway variables -s live --json | jq -r .TLS_CERT_PEM > cert.pem
#   cargo run -p server --bin gen-tls-cert -- --pin cert.pem
# Either way a TLS build without its pin fails at COMPILE time, on purpose —
# there is no safe default and a wrong pin is unrecoverable over the air.
set -euo pipefail
cd "$(dirname "$0")/../firmware-rs"

FEATURES="${FEATURES:-}"

PORT="${1:-$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)}"
if [[ -z "$PORT" ]]; then
    echo "No /dev/cu.usbmodem* port found — is the board plugged in?" >&2
    exit 1
fi

echo "==> building (features: ${FEATURES:-<none, dev server>})"
# xtensa-esp32s3-elf-gcc must be on PATH for the link step.
[[ -f "$HOME/export-esp.sh" ]] && source "$HOME/export-esp.sh"
cargo build --release ${FEATURES:+--features "$FEATURES"}

echo "==> freeing serial port (closing any monitors)"
pkill -f "device monitor" 2>/dev/null || true
lsof -t /dev/cu.usbmodem* 2>/dev/null | xargs -r kill 2>/dev/null || true
sleep 1

echo "==> flashing via $PORT"
# --partition-table: the OTA layout (nvs kept at 0x9000 so creds/brightness/
#   identity survive; two 3MB app slots for over-the-air updates).
# --target-app-partition ota_0 + --erase-parts otadata: write the app into
#   slot 0 and blank the OTA selector so the bootloader boots exactly that
#   slot. This is both the first-time migration AND the USB recovery path
#   after a bad OTA.
espflash flash -p "$PORT" \
    --partition-table partitions.csv \
    --target-app-partition ota_0 \
    --erase-parts otadata \
    target/xtensa-esp32s3-none-elf/release/firmware-rs

echo
echo "=============================================================="
echo "  Flash complete -> it should reboot itself; tap RESET if the panel stays dark."
echo "=============================================================="

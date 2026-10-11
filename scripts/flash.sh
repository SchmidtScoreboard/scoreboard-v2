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
#
# IDENTITY=<image.bin> also writes an authorized identity (MAR-90) minted by
# `cargo run -p server --bin bless-board -- <image.bin>` into the identity
# sector, so the board can use a hosted server that requires authorization.
# It REPLACES whatever identity the board had, so the board becomes a new
# board to the server and its phones. Delete the image afterwards: it IS the
# board.
set -euo pipefail
cd "$(dirname "$0")/../firmware-rs"

FEATURES="${FEATURES:-}"

# Pick up machine-local settings (SCOREBOARD_DEV_HOST, SCOREBOARD_DEV_TLS_PIN)
# without making the caller remember `set -a; source ...; set +a`. Anything
# already exported wins, so `SCOREBOARD_TLS_PIN=… FEATURES=prod scripts/flash.sh`
# is unaffected.
if [[ -f local.env ]]; then
    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*# || -z "${line// }" ]] && continue
        name="${line%%=*}"
        [[ -n "${!name:-}" ]] || export "${line?}"
    done < local.env
fi

IDENTITY="${IDENTITY:-}"
if [[ -n "$IDENTITY" ]]; then
    # Relative to where the caller ran this, not firmware-rs/.
    [[ "$IDENTITY" = /* ]] || IDENTITY="$OLDPWD/$IDENTITY"
    if [[ ! -f "$IDENTITY" || "$(wc -c < "$IDENTITY" | tr -d ' ')" != 4096 ]]; then
        echo "IDENTITY must be a 4096-byte image from bless-board: $IDENTITY" >&2
        exit 1
    fi
fi

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

if [[ -n "$IDENTITY" ]]; then
    # 0xB000 is the identity sector (firmware-rs/src/identity.rs), inside the
    # nvs partition that the app flash below leaves alone.
    echo "==> writing the authorized identity via $PORT"
    espflash write-bin -p "$PORT" 0xB000 "$IDENTITY"
fi

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

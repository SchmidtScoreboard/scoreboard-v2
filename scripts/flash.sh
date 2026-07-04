#!/usr/bin/env bash
# Flash the MatrixPortal S3 firmware over USB.
#
# Frees the serial port first (a running monitor locks it -> esptool Errno 35),
# then uploads. NOTE: this native-USB S3 does NOT auto-boot after flashing, so
# you must TAP RESET when it finishes. If it can't enter the bootloader to flash,
# hold BOOT + tap RESET on the board, then re-run this.
set -euo pipefail
cd "$(dirname "$0")/../firmware"

echo "==> freeing serial port (closing any monitors)"
pkill -f "device monitor" 2>/dev/null || true
lsof -t /dev/cu.usbmodem* 2>/dev/null | xargs -r kill 2>/dev/null || true
sleep 1

echo "==> flashing"
pio run -e matrixportal_s3 -t upload

echo
echo "=============================================================="
echo "  Flash complete -> now TAP THE RESET button to boot it."
echo "=============================================================="

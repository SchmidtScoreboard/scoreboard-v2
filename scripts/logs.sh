#!/usr/bin/env bash
# Watch the board's serial logs (the [status]/[ws] lines).
# Ctrl+C to quit -- which frees the port so you can flash again.
set -euo pipefail
cd "$(dirname "$0")/../firmware"
pkill -f "device monitor" 2>/dev/null || true   # don't stack monitors / lock the port
sleep 0.5
exec pio device monitor -e matrixportal_s3

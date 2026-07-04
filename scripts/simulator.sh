#!/usr/bin/env bash
# Run the desktop board simulator (opens a 64x32 window).
# Use --headless for no window (prints frame stats).
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run -p simulator -- "$@"

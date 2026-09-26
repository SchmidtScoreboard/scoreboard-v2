#!/usr/bin/env bash
# Capture both F1 feeds and the live server's frames during a session.
# Everything after the script name goes to capture-f1.py, e.g.:
#
#   scripts/capture-f1.sh --out design/f1-logs/race-baku-20260926 \
#     --session race --board <board id> --start 2026-09-26T10:40:00Z
#
# The OpenF1 credentials come from the two export lines in ~/.zshrc, loaded
# into this process only (never the whole profile, never written anywhere).
# caffeinate keeps the Mac out of idle sleep until the capture exits; a
# CLOSED LID still sleeps a laptop, so leave it open.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
if [[ -f "$HOME/.zshrc" ]]; then
    eval "$(grep -E '^[[:space:]]*export[[:space:]]+OPENF1_(EMAIL|KEY)=' "$HOME/.zshrc" || true)"
fi
exec caffeinate -is python3 "$here/capture-f1.py" "$@"

#!/usr/bin/env bash
# Hot-reload the server: rebuild + restart on any source change (via cargo-watch).
# Pass flags through, e.g.:  scripts/dev-server.sh --demo
# One-time install:  cargo install cargo-watch
set -euo pipefail
cd "$(dirname "$0")/.."
if ! command -v cargo-watch >/dev/null 2>&1; then
    echo "cargo-watch not found. Install it once with:  cargo install cargo-watch" >&2
    exit 1
fi
exec cargo watch -w crates -x "run -p server -- $*"

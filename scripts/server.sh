#!/usr/bin/env bash
# Run the scoreboard server. Binds 0.0.0.0:7878 so a board on your LAN can reach it.
# Pass flags through, e.g.:  scripts/server.sh --fetch-catalog
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run -p server -- "$@"

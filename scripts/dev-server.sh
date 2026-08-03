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
# TLS for the dev loop. Firmware built with default features REQUIRES TLS and
# pins a specific certificate, so a dev server that serves only plaintext means
# a freshly flashed dev board can never connect — and the tempting workaround
# (--no-default-features) is exactly the build you must never ship. So: mint a
# dev certificate once, cache it, and print the pin to put in local.env.
DEV_TLS_DIR=".dev-tls"   # gitignored
if [[ ! -f "$DEV_TLS_DIR/cert.pem" ]]; then
    echo "==> minting a dev TLS certificate (once)"
    mkdir -p "$DEV_TLS_DIR"
    cargo run -q -p server --bin gen-tls-cert -- localhost "$(hostname)" \
        > "$DEV_TLS_DIR/gen.out"
    # gen-tls-cert prints PEM blocks and the pin; split them out.
    awk '/BEGIN CERTIFICATE/,/END CERTIFICATE/' "$DEV_TLS_DIR/gen.out" > "$DEV_TLS_DIR/cert.pem"
    awk '/BEGIN PRIVATE KEY/,/END PRIVATE KEY/' "$DEV_TLS_DIR/gen.out" > "$DEV_TLS_DIR/key.pem"
    grep -oE '[0-9a-f]{64}' "$DEV_TLS_DIR/gen.out" | head -1 > "$DEV_TLS_DIR/pin.txt"
fi
DEV_PIN="$(cat "$DEV_TLS_DIR/pin.txt")"
export TLS_CERT_PEM="$(cat "$DEV_TLS_DIR/cert.pem")"
export TLS_KEY_PEM="$(cat "$DEV_TLS_DIR/key.pem")"
if ! grep -q "$DEV_PIN" firmware-rs/local.env 2>/dev/null; then
    cat >&2 <<EOF

  Dev TLS pin (put this in firmware-rs/local.env, then re-flash):

      SCOREBOARD_DEV_TLS_PIN=$DEV_PIN

EOF
fi
exec cargo watch -w crates -x "run -p server -- $*"

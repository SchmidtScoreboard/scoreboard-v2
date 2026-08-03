//! Build-time defaults for the board. TRACKED, and deliberately free of
//! secrets — a fresh clone must compile without hand-editing anything, which
//! is why this file is in git rather than the copy-me-first `config.example.rs`
//! it replaced (that arrangement left `cargo build` failing on `mod config;`
//! for anyone who cloned the repo).
//!
//! Anything private or machine-local comes in through the environment at build
//! time, so nobody has to edit — and risk committing — a tracked file:
//!
//! ```sh
//! SCOREBOARD_DEV_HOST=192.168.4.45 cargo build --release   # your LAN server
//! ```
//!
//! Keep a `firmware-rs/local.env` (gitignored) with your own values and
//! `set -a; source firmware-rs/local.env; set +a` before building.
#![allow(dead_code)]

/// Legacy Wi-Fi fallback, kept only for parity with the retired C++ firmware:
/// this build gets its credentials from BLE provisioning (see `creds.rs`), so
/// these constants are unreferenced. Real credentials NEVER belong in a tracked
/// file — override via the environment if you ever wire this path back up.
pub const WIFI_SSID: &str = match option_env!("SCOREBOARD_WIFI_SSID") {
    Some(s) => s,
    None => "your-network",
};
pub const WIFI_PASSWORD: &str = match option_env!("SCOREBOARD_WIFI_PASSWORD") {
    Some(s) => s,
    None => "your-password",
};

// Where the board talks when BLE provisioning didn't supply a host (see
// main.rs). Plain `ws://` to a host:port — no DNS-over-TLS, no URL parsing.

/// Dev builds (the default, what `scripts/flash.sh` produces) point at a LAN
/// server. The tracked value is a placeholder: set `SCOREBOARD_DEV_HOST` to
/// your own machine rather than editing this line, so your address never rides
/// along in a commit.
#[cfg(not(feature = "prod"))]
pub const SERVER_HOST: &str = match option_env!("SCOREBOARD_DEV_HOST") {
    Some(h) => h,
    None => "192.168.1.100",
};
#[cfg(not(feature = "prod"))]
pub const SERVER_PORT: u16 = 7878;

/// Prod builds (`--features prod`) — what OTA releases ship, so a released
/// image can never fall back to somebody's LAN IP. This is the Railway TCP
/// proxy in front of the real server; public information, and the OTA fallback
/// genuinely needs it baked in.
#[cfg(feature = "prod")]
pub const SERVER_HOST: &str = "hayabusa.proxy.rlwy.net";
#[cfg(feature = "prod")]
pub const SERVER_PORT: u16 = 47958;

/// Vestigial: the board's identity is its Ed25519 pubkey (`identity.rs`), not
/// this name.
pub const BOARD_ID: &str = "matrixportal-01";

// ---------------------------------------------------------------------------
// TLS pins
// ---------------------------------------------------------------------------
//
// The board talks to exactly ONE server, so it pins that server's public key
// rather than validating a certificate chain — see `src/tls.rs` for why (no
// CA bundle to ship, and no wall clock to check expiry with).
//
//     pin = lowercase hex SHA-256 of the certificate's DER SubjectPublicKeyInfo
//
// A public key is not a secret, so these live in a tracked file exactly like
// the prod hostname does. Re-derive one for a certificate you already have:
//
//     cargo run -p server --bin gen-tls-cert -- --pin path/to/cert.pem
//
// `SERVER_PIN_NEXT` is the staged successor, and exists so a key rotation is a
// rollout rather than a cliff: ship firmware that trusts current + successor,
// swap the server's key, then ship firmware that trusts only the new one. Set
// it to `None` when no rotation is in flight. It must never be an all-zero
// digest — `tls::pin_from_hex` refuses that at compile time so a "disabled"
// pin can't turn into a wildcard.
//
// A wrong pin fails CLOSED: the board rejects the handshake and retries
// forever, which is the intended outcome and is covered by the OTA revert
// watchdog (an unconfirmed image that can't auth within five minutes rolls
// back).
//
// These are the DEFAULT, not the only possible pins. A board can be
// provisioned with a pin of its own over BLE, exactly as it is provisioned
// with a host and port (`creds.rs`, record v3), and when it carries one that
// pin REPLACES the pair below — which is what lets someone run their own
// server against their own certificate. Boards on the hosted server provision
// nothing and use these. See docs/cli.md, "Running your own server".

/// Dev server pin — the build's DEFAULT pin, REQUIRED at build time and
/// deliberately without a default of its own.
///
/// "Default" means what an unprovisioned board trusts: a board carrying a
/// BLE-provisioned pin ignores this one entirely (`link::Link::open`). It is
/// still required, because most boards never provision one and a TLS build
/// with no pin at all could not talk to anything.
///
/// There is no such thing as a safe placeholder here. A pin that belongs to no
/// certificate anyone holds still compiles, still passes the "is it set" check,
/// and produces a board that fails every handshake forever. The failure is
/// silent at build time and expensive at run time, so the build refuses
/// instead: run `cargo run -p server --bin gen-tls-cert`, put the printed pin
/// in `firmware-rs/local.env` as `SCOREBOARD_DEV_TLS_PIN`, and source it
/// (`scripts/dev-server.sh` does this for you).
#[cfg(all(feature = "tls", not(feature = "prod")))]
pub const SERVER_PIN: &str = match option_env!("SCOREBOARD_DEV_TLS_PIN") {
    Some(p) => p,
    None => panic!(
        "SCOREBOARD_DEV_TLS_PIN is not set. A dev TLS build needs the pin of YOUR dev \
         server's certificate — there is no safe default. Generate one with \
         `cargo run -p server --bin gen-tls-cert`, add it to firmware-rs/local.env, and \
         `set -a; source firmware-rs/local.env; set +a` before building. \
         (Or build with --no-default-features for a plaintext bring-up image.)"
    ),
};
#[cfg(all(feature = "tls", not(feature = "prod")))]
pub const SERVER_PIN_NEXT: Option<&str> = option_env!("SCOREBOARD_DEV_TLS_PIN_NEXT");

/// Prod server pin — the hosted server's pin, REQUIRED at build time and
/// deliberately without a default of its own.
///
/// This is the single most dangerous constant in the firmware: it ships to
/// every board over the air, and a board whose image is already CONFIRMED and
/// whose pin does not match the live certificate cannot be recovered remotely
/// — the update offer travels over the very connection that no longer opens.
/// A default value would let `scripts/release-firmware.sh` mint and sign a
/// hosted image pinned to a key nobody holds, which is exactly the mistake
/// this refuses to allow. The release script exports `SCOREBOARD_TLS_PIN` from
/// the deployed certificate; see docs/cli.md for the rotation procedure.
#[cfg(all(feature = "tls", feature = "prod"))]
pub const SERVER_PIN: &str = match option_env!("SCOREBOARD_TLS_PIN") {
    Some(p) => p,
    None => panic!(
        "SCOREBOARD_TLS_PIN is not set. A prod TLS build must pin the certificate the \
         LIVE server is actually serving — there is no safe default, and a wrong pin \
         bricks the fleet's link. Export the pin of the deployed cert (re-derive it any \
         time with `cargo run -p server --bin gen-tls-cert -- --pin cert.pem`) before \
         running scripts/release-firmware.sh."
    ),
};
#[cfg(all(feature = "tls", feature = "prod"))]
pub const SERVER_PIN_NEXT: Option<&str> = option_env!("SCOREBOARD_TLS_PIN_NEXT");

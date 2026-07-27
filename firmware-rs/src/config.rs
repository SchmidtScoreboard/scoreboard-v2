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

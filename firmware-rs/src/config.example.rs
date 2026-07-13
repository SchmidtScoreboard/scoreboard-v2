//! Copy to `config.rs` (gitignored) and fill in. Same idea as the C++ `config.h`.
//! The board talks plain `ws://` to a LAN IP (no DNS/TLS), so the server is an IP.
//! Flash dev with `cargo run --release`, prod with `cargo run --release --features prod`.
#![allow(dead_code)]
pub const WIFI_SSID: &str = "your-network";
pub const WIFI_PASSWORD: &str = "your-password";

// Dev server (default build).
#[cfg(not(feature = "prod"))]
pub const SERVER_HOST: &str = "192.168.1.100"; // hostname or IP literal
#[cfg(not(feature = "prod"))]
pub const SERVER_PORT: u16 = 7878;

// Prod server (built with `--features prod` — what OTA releases ship).
#[cfg(feature = "prod")]
pub const SERVER_HOST: &str = "your-prod-host.example.com";
#[cfg(feature = "prod")]
pub const SERVER_PORT: u16 = 7878;

pub const BOARD_ID: &str = "matrixportal-01";

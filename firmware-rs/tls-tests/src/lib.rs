//! Firmware modules that decide whether a board can reach its server,
//! compiled for the host.
//!
//! `#[path]` rather than a copy: `tests/pinning.rs` and `tests/creds.rs`
//! exercise the exact bytes that go on the board, so the tests cannot silently
//! drift away from the firmware.

// `creds.rs` is `no_std` + `alloc`, like the firmware it belongs to.
extern crate alloc;

#[path = "../../src/tls.rs"]
pub mod tls;

#[path = "../../src/creds.rs"]
pub mod creds;

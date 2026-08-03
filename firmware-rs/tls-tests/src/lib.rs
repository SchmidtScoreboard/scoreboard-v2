//! The firmware's pinned-TLS verifier, compiled for the host.
//!
//! `#[path]` rather than a copy: `tests/pinning.rs` exercises the exact bytes
//! that go on the board, so the tests cannot silently drift away from the
//! firmware.

#[path = "../../src/tls.rs"]
pub mod tls;

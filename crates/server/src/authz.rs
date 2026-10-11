//! Which boards may use this server (MAR-90).
//!
//! A board proves who it is with its Ed25519 identity, and anyone can mint
//! one. On the hosted server that used to be the whole check, so any board
//! built against it got polling, rendering and OTA for free. Now an operator
//! can require that a board also be AUTHORIZED: its public key signed by the
//! owner's key (a "blessing", `scoreboard_protocol::verify_blessing`), written
//! to the board at flash time and presented in `ClientMsg::Auth`.
//!
//! Configured entirely from the environment, and off unless asked for, so a
//! self-hosted server needs nothing:
//!
//! - `BOARD_BLESSING_PUBKEY`: the owner's public key, hex. Unset = no check
//!   at all. Malformed = the server refuses to start, because a typo here
//!   must not quietly open the server, or lock every board out.
//! - `BOARD_BLESSING_ENFORCE=1`: refuse unauthorized boards. Otherwise the
//!   check only LOGS them, which is how it is rolled out: watch the log and
//!   `/health`, then enforce once every board you mean to serve passes.
//!
//! There is deliberately no list of exempt boards: a board that predates this
//! is authorized like any other, by flashing it a blessing.
//!
//! Only DEVICES are checked. Apps are not boards and never carry a blessing.

use std::collections::HashSet;
use std::sync::Mutex;

pub struct BoardAuthz {
    owner: [u8; 32],
    enforce: bool,
    /// Board ids refused (or, in log-only mode, that would have been) since
    /// boot. Logs each one once rather than on every redial, and is the
    /// count `/health` reports.
    refused: Mutex<HashSet<String>>,
}

impl BoardAuthz {
    /// The policy the environment asks for: None when `BOARD_BLESSING_PUBKEY`
    /// is unset. Panics on a malformed key, on purpose (see the module doc).
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("BOARD_BLESSING_PUBKEY").ok().filter(|k| !k.trim().is_empty())?;
        let enforce = std::env::var("BOARD_BLESSING_ENFORCE").is_ok_and(|v| v.trim() == "1");
        let authz = Self::new(&key, enforce).unwrap_or_else(|e| panic!("BOARD_BLESSING_PUBKEY: {e}"));
        tracing::info!("board authorization: {}", authz.mode());
        Some(authz)
    }

    pub fn new(owner_pubkey_hex: &str, enforce: bool) -> Result<Self, String> {
        let bytes = hex::decode(owner_pubkey_hex.trim()).map_err(|_| "not hex".to_string())?;
        let owner: [u8; 32] = bytes.try_into().map_err(|b: Vec<u8>| format!("{} bytes, expected 32", b.len()))?;
        Ok(Self { owner, enforce, refused: Mutex::new(HashSet::new()) })
    }

    /// Is this device authorized? It has already proved it holds
    /// `board_id`'s secret; this asks whether the owner signed that id.
    pub fn authorized(&self, board_id: &str, blessing: Option<&str>) -> bool {
        blessing.is_some_and(|b| scoreboard_protocol::verify_blessing(&self.owner, board_id, b))
    }

    /// Record a refusal; true the first time this board is refused since boot.
    pub fn note_refused(&self, board_id: &str) -> bool {
        self.refused.lock().unwrap().insert(board_id.to_string())
    }

    pub fn enforced(&self) -> bool {
        self.enforce
    }

    /// For `/health` and the boot log.
    pub fn mode(&self) -> &'static str {
        if self.enforce { "enforced" } else { "log-only" }
    }

    /// Distinct boards refused (or that would have been) since boot.
    pub fn refused_count(&self) -> usize {
        self.refused.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scoreboard_protocol::{bless, pubkey_bytes, pubkey_hex};

    const OWNER: [u8; 32] = [11u8; 32];

    fn owner_pubkey() -> String {
        pubkey_hex(&OWNER)
    }

    #[test]
    fn only_the_owners_blessing_of_this_board_authorizes_it() {
        let board = [22u8; 32];
        let id = pubkey_hex(&board);
        let blessing = hex::encode(bless(&OWNER, &pubkey_bytes(&board)));
        let authz = BoardAuthz::new(&owner_pubkey(), true).unwrap();

        assert!(authz.authorized(&id, Some(&blessing)));
        assert!(!authz.authorized(&id, None), "no blessing");
        assert!(!authz.authorized(&pubkey_hex(&[44u8; 32]), Some(&blessing)), "another board's blessing");
        let forged = hex::encode(bless(&[99u8; 32], &pubkey_bytes(&board)));
        assert!(!authz.authorized(&id, Some(&forged)), "blessed by someone else");
    }

    #[test]
    fn refusals_are_counted_once_per_board() {
        let authz = BoardAuthz::new(&owner_pubkey(), false).unwrap();
        assert_eq!(authz.mode(), "log-only");
        assert!(authz.note_refused("aa"));
        assert!(!authz.note_refused("aa"), "a redial is not news");
        assert!(authz.note_refused("bb"));
        assert_eq!(authz.refused_count(), 2);
    }

    #[test]
    fn a_malformed_key_is_an_error_not_an_open_server() {
        assert!(BoardAuthz::new("not hex", true).is_err());
        assert!(BoardAuthz::new("abcd", true).is_err());
    }
}

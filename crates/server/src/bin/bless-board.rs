//! Authorize boards for the hosted server (MAR-90).
//!
//! ```text
//! The owner (holds the owner key):
//!   bless-board --print-pubkey            create the owner key if needed, print its public key
//!   bless-board <out.bin>                 mint an authorized board in one step (your own bench)
//!   bless-board --sign <board id>         authorize a BYOD board: print its authorization code
//!
//! A BYOD builder (needs no key; their board's secret never leaves their machine):
//!   bless-board --new-identity <out.bin>  create the board's identity, print its board id
//!   bless-board --attach <code> <file>    add the owner's authorization code to that identity
//!
//! Anyone:
//!   bless-board --check <file>            which board an image is, and whether it's authorized
//! ```
//!
//! The owner's Ed25519 seed lives at ~/.config/scoreboard/board-blessing.key
//! (64 hex chars, chmod 0600). Only `--print-pubkey` creates it; minting and
//! signing refuse to run without it, because a fresh key on another machine
//! would hand out authorizations the server rejects. It is NEVER committed or
//! put on a server: the server gets only the public key, as
//! `BOARD_BLESSING_PUBKEY`. Back it up. Without it no new board can be
//! authorized (existing ones keep working), and anyone holding it can
//! authorize boards of their own.
//!
//! An image is one flash sector holding an identity record
//! (`scoreboard_protocol::IdentityRecord`): the board's secret seed, plus the
//! owner's blessing of its public key once authorized. Write it to the board's
//! identity sector at 0xB000 with `IDENTITY=<file> scripts/flash.sh`. The
//! image IS the board: anyone with it can be that board, so keep it private
//! and delete it once flashed.
//!
//! An authorization code is `auth1-` + the owner's public key + the blessing,
//! in hex. It is not secret: without the board's seed it is useless. It
//! carries the owner's key so `--attach` can tell a mistyped code, or one
//! made for a different board, before anything is flashed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::exit;

use scoreboard_protocol::{bless, parse_board_id, pubkey_bytes, pubkey_hex, verify_blessing, IdentityRecord};

const USAGE: &str = "usage: bless-board --print-pubkey
       bless-board <out.bin>
       bless-board --sign <board id>
       bless-board --new-identity <out.bin>
       bless-board --attach <code> <identity.bin>
       bless-board --check <identity.bin>

Authorizes boards for a server that requires it (BOARD_BLESSING_PUBKEY).
The owner key is ~/.config/scoreboard/board-blessing.key.";

/// One flash sector: what `espflash write-bin` writes at 0xB000.
const SECTOR: usize = 4096;
const CODE_PREFIX: &str = "auth1-";

fn key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| fail("$HOME is not set; cannot locate the owner key"));
    Path::new(&home).join(".config").join("scoreboard").join("board-blessing.key")
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}

/// Create `path` 0600 from its first byte and write `bytes`; never overwrite.
fn write_private(path: &Path, bytes: &[u8]) {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).unwrap_or_else(|e| fail(format!("creating {}: {e}", path.display())));
    f.write_all(bytes).unwrap_or_else(|e| fail(format!("writing {}: {e}", path.display())));
}

fn random_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    seed
}

/// The owner seed, or None when this machine has none.
fn read_owner_seed() -> Option<[u8; 32]> {
    let path = key_path();
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("reading {}: {e}", path.display())));
    let bytes = hex::decode(text.trim())
        .unwrap_or_else(|_| fail(format!("{} is not valid hex (expected 64 hex chars)", path.display())));
    Some(bytes.try_into().unwrap_or_else(|b: Vec<u8>| {
        fail(format!("{} holds {} bytes, expected 32", path.display(), b.len()))
    }))
}

/// The owner seed, for minting and signing: never created here.
fn owner_seed() -> [u8; 32] {
    read_owner_seed().unwrap_or_else(|| {
        fail(format!(
            "no owner key at {} — restore your backup, or run `bless-board --print-pubkey` once to create one \
             (a new key invalidates nothing already flashed, but the server must be given its public key)",
            key_path().display()
        ))
    })
}

/// `--print-pubkey`: the one place the owner key is created.
fn print_pubkey() {
    let seed = read_owner_seed().unwrap_or_else(|| {
        let path = key_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(format!("creating {}: {e}", dir.display())));
        }
        let seed = random_seed();
        write_private(&path, format!("{}\n", hex::encode(seed)).as_bytes());
        eprintln!(
            "generated a new owner key at {} — back it up: it is the only way to authorize new boards",
            path.display()
        );
        eprintln!("set BOARD_BLESSING_PUBKEY={} on the server", pubkey_hex(&seed));
        seed
    });
    // stdout is EXACTLY the public key hex, for BOARD_BLESSING_PUBKEY.
    println!("{}", pubkey_hex(&seed));
}

fn sector_image(record: &IdentityRecord) -> Vec<u8> {
    let mut sector = vec![0xFFu8; SECTOR];
    let bytes = record.encode();
    sector[..bytes.len()].copy_from_slice(&bytes);
    sector
}

fn read_identity(path: &Path) -> IdentityRecord {
    let bytes = std::fs::read(path).unwrap_or_else(|e| fail(format!("reading {}: {e}", path.display())));
    IdentityRecord::decode(&bytes).unwrap_or_else(|| fail(format!("{} holds no board identity", path.display())))
}

/// `<out.bin>`: mint an authorized board in one step, for boards flashed on
/// the owner's own bench (the secret is created where it is flashed).
fn mint(out: &Path) {
    let owner = owner_seed();
    let secret = random_seed();
    let record = IdentityRecord { secret, blessing: Some(bless(&owner, &pubkey_bytes(&secret))) };
    write_private(out, &sector_image(&record));
    let id = pubkey_hex(&secret);
    // stdout is exactly the board id; the rest is for the person at the bench.
    println!("{id}");
    eprintln!("minted authorized board {id} into {}", out.display());
    eprintln!("flash it with:  IDENTITY={} scripts/flash.sh", out.display());
    eprintln!("then delete {} (it IS the board)", out.display());
}

/// `--new-identity`: a BYOD builder's first step. Needs no owner key.
fn new_identity(out: &Path) {
    let secret = random_seed();
    write_private(out, &sector_image(&IdentityRecord { secret, blessing: None }));
    let id = pubkey_hex(&secret);
    println!("{id}");
    eprintln!("created board {id} in {}", out.display());
    eprintln!("send ONLY the board id above to get an authorization code; keep {} private", out.display());
}

/// The authorization code for `board_id`, from the owner's seed.
fn authorization_code(owner: &[u8; 32], board_id: &str) -> Result<String, String> {
    let board_pk = parse_board_id(board_id).ok_or("not a board id (64 hex characters, from --new-identity)")?;
    Ok(format!("{CODE_PREFIX}{}{}", pubkey_hex(owner), hex::encode(bless(owner, &board_pk))))
}

/// Attach `code` to `record`: the authorized record, or why not.
fn attach_code(record: &IdentityRecord, code: &str) -> Result<IdentityRecord, String> {
    let hexpart = code.trim().strip_prefix(CODE_PREFIX).ok_or("not an authorization code (they start with auth1-)")?;
    let bytes = hex::decode(hexpart).map_err(|_| "the code is damaged (not hex); copy it again")?;
    if bytes.len() != 96 {
        return Err("the code is the wrong length; copy it again".into());
    }
    let owner: [u8; 32] = bytes[..32].try_into().unwrap();
    let blessing: [u8; 64] = bytes[32..].try_into().unwrap();
    let id = pubkey_hex(&record.secret);
    if !verify_blessing(&owner, &id, &hex::encode(blessing)) {
        return Err(format!("this code is not for board {id} (made for another board, or damaged)"));
    }
    Ok(IdentityRecord { secret: record.secret, blessing: Some(blessing) })
}

/// `--attach`: rewrite the identity file in place, authorized.
fn attach(code: &str, path: &Path) {
    let record = read_identity(path);
    let authorized = attach_code(&record, code).unwrap_or_else(|e| fail(e));
    // Write beside it, then swap in: a crash mid-write must not destroy the
    // only copy of the board's secret.
    let tmp = path.with_extension("bin.tmp");
    let _ = std::fs::remove_file(&tmp);
    write_private(&tmp, &sector_image(&authorized));
    std::fs::rename(&tmp, path).unwrap_or_else(|e| fail(format!("replacing {}: {e}", path.display())));
    println!("{}", pubkey_hex(&record.secret));
    eprintln!("authorized; flash it with:  IDENTITY={} FEATURES=prod SCOREBOARD_TLS_PIN=<pin> scripts/flash.sh", path.display());
}

/// `--check`: needs no key. With the owner key on this machine it also says
/// whether the authorization is this owner's.
fn check(path: &Path) {
    let record = read_identity(path);
    let id = pubkey_hex(&record.secret);
    println!("{id}");
    let verdict = match (record.blessing, read_owner_seed()) {
        (None, _) => "not authorized (no authorization code attached)",
        (Some(_), None) => "carries an authorization (no owner key here to check whose)",
        (Some(b), Some(owner)) if verify_blessing(&pubkey_bytes(&owner), &id, &hex::encode(b)) => {
            "authorized by this owner key"
        }
        (Some(_), Some(_)) => "carries an authorization this owner key did not make",
    };
    eprintln!("{verdict}");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => println!("{USAGE}"),
        [flag] if flag == "--print-pubkey" => print_pubkey(),
        [flag, id] if flag == "--sign" => {
            println!("{}", authorization_code(&owner_seed(), id).unwrap_or_else(|e| fail(e)));
        }
        [flag, out] if flag == "--new-identity" => new_identity(Path::new(out)),
        [flag, code, file] if flag == "--attach" => attach(code, Path::new(file)),
        [flag, file] if flag == "--check" => check(Path::new(file)),
        [out] if !out.starts_with('-') => mint(Path::new(out)),
        _ => {
            eprintln!("{USAGE}");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: [u8; 32] = [7u8; 32];

    /// The BYOD flow end to end, minus the files: the builder's identity, the
    /// owner's code made from its id alone, attached, and the result is what
    /// the server accepts.
    #[test]
    fn a_code_made_from_the_board_id_authorizes_that_board() {
        let record = IdentityRecord { secret: [3u8; 32], blessing: None };
        let id = pubkey_hex(&record.secret);
        let code = authorization_code(&OWNER, &id).unwrap();
        assert!(code.starts_with("auth1-") && code.len() == 6 + 192, "{code}");

        let authorized = attach_code(&record, &code).unwrap();
        assert_eq!(authorized.secret, record.secret, "the board keeps its own secret");
        let blessing = hex::encode(authorized.blessing.unwrap());
        assert!(verify_blessing(&pubkey_bytes(&OWNER), &id, &blessing), "the server's own check passes");
        // And what gets flashed decodes to the same thing.
        assert_eq!(IdentityRecord::decode(&sector_image(&authorized)), Some(authorized));
    }

    #[test]
    fn attach_refuses_another_boards_code_and_damaged_codes() {
        let record = IdentityRecord { secret: [3u8; 32], blessing: None };
        let other = authorization_code(&OWNER, &pubkey_hex(&[4u8; 32])).unwrap();
        assert!(attach_code(&record, &other).unwrap_err().contains("not for board"));

        let good = authorization_code(&OWNER, &pubkey_hex(&record.secret)).unwrap();
        let mut typo = good.clone().into_bytes();
        let last = typo.len() - 1;
        typo[last] = if typo[last] == b'0' { b'1' } else { b'0' };
        assert!(attach_code(&record, std::str::from_utf8(&typo).unwrap()).is_err(), "one changed character");
        assert!(attach_code(&record, &good[..good.len() - 2]).unwrap_err().contains("length"), "truncated");
        assert!(attach_code(&record, &good[6..]).unwrap_err().contains("auth1-"), "the board id pasted instead");
        assert!(attach_code(&record, &format!("  {good}\n")).is_ok(), "surrounding whitespace is fine");
    }

    #[test]
    fn sign_refuses_what_is_not_a_board_id() {
        assert!(authorization_code(&OWNER, "abc").is_err());
        assert!(authorization_code(&OWNER, &"zz".repeat(32)).is_err());
        assert!(authorization_code(&OWNER, &format!("  {}  ", pubkey_hex(&[3u8; 32]))).is_ok());
    }
}

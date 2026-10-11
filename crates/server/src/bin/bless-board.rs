//! Mint authorized board identities for the hosted server (MAR-90).
//!
//! ```text
//! bless-board --print-pubkey     ensure the owner key exists, print its public key
//! bless-board <out.bin>          mint one board: write its identity sector image
//! bless-board --check <in.bin>   say which board an image is, and if it's authorized
//! ```
//!
//! The owner's Ed25519 seed lives at ~/.config/scoreboard/board-blessing.key
//! (64 hex chars, chmod 0600), generated on first use and NEVER committed or
//! put on a server. The server gets only the public key, as
//! `BOARD_BLESSING_PUBKEY`. Back the seed up: without it no new board can be
//! authorized (existing ones keep working), and anyone holding it can
//! authorize boards of their own.
//!
//! A minted image is one flash sector holding a v2 identity record (the
//! board's secret seed plus the owner's blessing of its public key,
//! `scoreboard_protocol::IdentityRecord`). Write it to the board's identity
//! sector at 0xB000, e.g. `IDENTITY=<out.bin> scripts/flash.sh`. The image IS
//! the board: anyone with it can be that board, so delete it once flashed,
//! or hand it only to the BYOD owner it was minted for.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::exit;

use scoreboard_protocol::{bless, pubkey_hex, verify_blessing, IdentityRecord};

const USAGE: &str = "usage: bless-board --print-pubkey\n       bless-board <out.bin>\n       bless-board --check <in.bin>\n\nMints authorized board identities. The owner key is read from\n~/.config/scoreboard/board-blessing.key (generated on first use).";

/// One flash sector: what `espflash write-bin` writes at 0xB000.
const SECTOR: usize = 4096;

fn key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| {
        eprintln!("error: $HOME is not set; cannot locate the owner key");
        exit(1);
    });
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

/// The owner seed, generated (and persisted, 0600) on first use.
fn owner_seed() -> [u8; 32] {
    let path = key_path();
    if !path.exists() {
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
        return seed;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("reading {}: {e}", path.display())));
    let bytes = hex::decode(text.trim())
        .unwrap_or_else(|_| fail(format!("{} is not valid hex (expected 64 hex chars)", path.display())));
    bytes.try_into().unwrap_or_else(|b: Vec<u8>| fail(format!("{} holds {} bytes, expected 32", path.display(), b.len())))
}

fn owner_pubkey(seed: &[u8; 32]) -> [u8; 32] {
    hex::decode(pubkey_hex(seed)).unwrap().try_into().unwrap()
}

/// Mint a board: fresh seed, the owner's blessing, one sector image.
fn mint(out: &Path) {
    let owner = owner_seed();
    let secret = random_seed();
    let record = IdentityRecord { secret, blessing: Some(bless(&owner, &secret)) };
    let mut sector = vec![0xFFu8; SECTOR];
    sector[..record.encode().len()].copy_from_slice(&record.encode());
    write_private(out, &sector);
    let id = pubkey_hex(&secret);
    // stdout is exactly the board id; the rest is for the person at the bench.
    println!("{id}");
    eprintln!("minted authorized board {id} into {}", out.display());
    eprintln!("flash it with:  IDENTITY={} scripts/flash.sh", out.display());
    eprintln!("then delete {} (it IS the board)", out.display());
}

fn check(input: &Path) {
    let bytes = std::fs::read(input).unwrap_or_else(|e| fail(format!("reading {}: {e}", input.display())));
    let Some(record) = IdentityRecord::decode(&bytes) else {
        fail(format!("{} holds no identity record", input.display()));
    };
    let id = pubkey_hex(&record.secret);
    let authorized = record.blessing.is_some_and(|b| {
        verify_blessing(&owner_pubkey(&owner_seed()), &id, &hex::encode(b))
    });
    println!("{id}");
    eprintln!(
        "{}",
        match (record.blessing.is_some(), authorized) {
            (false, _) => "self-minted: not authorized",
            (true, true) => "authorized by this owner key",
            (true, false) => "carries a blessing this owner key did not make",
        }
    );
    if !authorized {
        exit(2);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => println!("{USAGE}"),
        // stdout is EXACTLY the public key hex, for BOARD_BLESSING_PUBKEY.
        [flag] if flag == "--print-pubkey" => println!("{}", pubkey_hex(&owner_seed())),
        [flag, input] if flag == "--check" => check(Path::new(input)),
        [out] if !out.starts_with('-') => mint(Path::new(out)),
        _ => {
            eprintln!("{USAGE}");
            exit(1);
        }
    }
}

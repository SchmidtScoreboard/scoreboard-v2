//! Sign a firmware image for OTA distribution.
//!
//! ```text
//! sign-firmware <image.bin> <version>   sign image, write firmware.json next to it
//! sign-firmware --print-pubkey          ensure the key exists, print pubkey hex
//! ```
//!
//! The Ed25519 signing seed lives at ~/.config/scoreboard/firmware-signing.key
//! (64 hex chars = 32 bytes, chmod 0600) and is generated on first use — it is
//! NEVER committed to the repo. The matching public key is baked into the
//! firmware as `firmware-rs/src/fw_pubkey.rs` (written by
//! scripts/release-firmware.sh from this tool's output), and the board accepts
//! only images whose signature verifies against it.
//!
//! firmware.json (served by the server as GET /firmware/{channel}/latest):
//!   { "version": "2026.7.0", "size": <bytes>, "sha256": "<hex of raw image
//!     bytes>", "sig": "<hex Ed25519 signature over the raw image bytes>" }

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::exit;

use scoreboard_protocol::{pubkey_hex, sha256_hex, sign_hex, verify_hex};

const USAGE: &str = "usage: sign-firmware <image.bin> <version> <channel>\n       sign-firmware --print-pubkey\n\nSigns an OTA firmware image: writes firmware.json (version/size/sha256/sig)\nnext to the image. The Ed25519 seed is read from\n~/.config/scoreboard/firmware-signing.key (generated on first use).";

fn key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| {
        eprintln!("error: $HOME is not set; cannot locate the signing key");
        exit(1);
    });
    Path::new(&home)
        .join(".config")
        .join("scoreboard")
        .join("firmware-signing.key")
}

/// Read the 32-byte seed from `path`, generating (and persisting, 0600) a
/// fresh one if the file doesn't exist yet.
fn load_or_generate_seed(path: &Path) -> [u8; 32] {
    if path.exists() {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("error: reading {}: {e}", path.display());
            exit(1);
        });
        let bytes = hex::decode(text.trim()).unwrap_or_else(|_| {
            eprintln!(
                "error: {} is not valid hex (expected 64 hex chars = 32-byte Ed25519 seed)",
                path.display()
            );
            exit(1);
        });
        let seed: [u8; 32] = bytes.try_into().unwrap_or_else(|b: Vec<u8>| {
            eprintln!(
                "error: {} holds {} bytes, expected exactly 32",
                path.display(),
                b.len()
            );
            exit(1);
        });
        return seed;
    }

    // First use: mint a seed from the OS RNG and store it locked-down.
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| {
            eprintln!("error: creating {}: {e}", dir.display());
            exit(1);
        });
    }
    // 0600 from the very first byte — a write-then-chmod leaves a window where
    // the seed every board trusts is world-readable.
    {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(path).unwrap_or_else(|e| {
            eprintln!("error: creating {}: {e}", path.display());
            exit(1);
        });
        writeln!(f, "{}", hex::encode(seed)).unwrap_or_else(|e| {
            eprintln!("error: writing {}: {e}", path.display());
            exit(1);
        });
    }
    eprintln!(
        "generated new firmware signing key at {} (keep it: boards only accept \
         images signed by it)",
        path.display()
    );
    eprintln!("derived public key: {}", pubkey_hex(&seed));
    seed
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}");
        }
        // Key bootstrap / lookup for release-firmware.sh: stdout is EXACTLY the
        // pubkey hex (generation chatter goes to stderr).
        [flag] if flag == "--print-pubkey" => {
            let seed = load_or_generate_seed(&key_path());
            println!("{}", pubkey_hex(&seed));
        }
        [image_path, version, channel] => {
            let seed = load_or_generate_seed(&key_path());
            sign(Path::new(image_path), version, channel, &seed);
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

fn sign(image: &Path, version: &str, channel: &str, seed: &[u8; 32]) {
    if !matches!(channel, "stable" | "beta" | "dev") {
        eprintln!("error: channel {channel:?} is not one of stable/beta/dev");
        exit(1);
    }
    // The wire contract sends versions around as CalVer "YYYY.M.P"; refuse to
    // publish a dist the server/app/board would never treat as an update.
    if scoreboard_protocol::parse_calver(version).is_none() {
        eprintln!("error: version {version:?} is not CalVer (\"YYYY.M.P\", e.g. \"2026.7.0\")");
        exit(1);
    }

    let bytes = std::fs::read(image).unwrap_or_else(|e| {
        eprintln!("error: reading {}: {e}", image.display());
        exit(1);
    });
    let size: u32 = bytes.len().try_into().unwrap_or_else(|_| {
        eprintln!("error: image is {} bytes — larger than u32", bytes.len());
        exit(1);
    });
    if size == 0 {
        eprintln!("error: {} is empty", image.display());
        exit(1);
    }

    let sha256 = sha256_hex(&bytes);
    // The signature binds version and size ALONGSIDE the image bytes (the
    // firmware feeds its verifier the same prefix): without this, anyone on
    // the plain-HTTP path could replay an old signed release under a new
    // version string. The board also refuses non-newer versions outright.
    // Channel rides inside the signed message too: an on-path attacker can
    // replay offers but can never RELABEL one — a dev image only verifies as
    // channel=dev on the device.
    let mut message = format!("scoreboard-fw:{version}:{size}:{channel}:").into_bytes();
    message.extend_from_slice(&bytes);
    let sig = sign_hex(seed, &message);
    let pubkey = pubkey_hex(seed);
    // Belt and braces: never publish a signature the board would reject.
    assert!(
        verify_hex(&pubkey, &message, &sig),
        "self-check failed: fresh signature does not verify"
    );

    let json = serde_json::json!({
        "version": version,
        "channel": channel,
        "size": size,
        "sha256": sha256,
        "sig": sig,
    });
    let out = image.with_file_name("firmware.json");
    let mut f = std::fs::File::create(&out).unwrap_or_else(|e| {
        eprintln!("error: creating {}: {e}", out.display());
        exit(1);
    });
    writeln!(f, "{:#}", json).unwrap_or_else(|e| {
        eprintln!("error: writing {}: {e}", out.display());
        exit(1);
    });

    println!("signed   {}", image.display());
    println!("version  {version}");
    println!("size     {size} bytes");
    println!("sha256   {sha256}");
    println!("sig      {sig}");
    println!("pubkey   {pubkey}");
    println!("wrote    {}", out.display());
}

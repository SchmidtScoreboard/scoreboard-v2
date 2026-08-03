//! Mint the long-lived, self-signed certificate the boards pin.
//!
//! ```text
//! gen-tls-cert [hostname ...]     mint a new Ed25519 identity, print cert + key + pin
//! gen-tls-cert --pin <cert.pem>   print the pin of an EXISTING certificate
//! ```
//!
//! Run by hand, once, by the owner — NOT at boot. A certificate minted per
//! deploy would change the pin on every deploy and lock out every board in the
//! field. The two PEMs go into Railway as the `TLS_CERT_PEM` / `TLS_KEY_PEM`
//! secrets; the pin goes into the firmware. Rotation procedure is in
//! docs/cli.md.
//!
//! The private key is printed to stdout and stored nowhere: paste it into the
//! secret store and let the terminal buffer be the only other copy. There is no
//! recovery — losing it means minting a new identity, which is a firmware
//! rotation.

// This crate has no lib target, so the module the server itself uses is pulled
// in by path. That is deliberate: the pin printed here and the pin the server
// logs at boot come from the same `spki_sha256_hex`, and cannot drift.
#[path = "../tls.rs"]
mod tls;

use std::process::exit;

const USAGE: &str = "\
usage: gen-tls-cert [hostname ...]
       gen-tls-cert --pin <cert.pem>

Mints a self-signed Ed25519 certificate for the board<->server TLS link and
prints the PEM certificate, the PEM private key, and the pin the firmware
carries. Run once, by hand; the server loads the PEMs from TLS_CERT_PEM /
TLS_KEY_PEM and never generates its own.

Default hostnames: hayabusa.proxy.rlwy.net localhost 127.0.0.1";

/// Where boards actually connect, plus the loopback names that make `curl` and
/// `openssl s_client` usable against a local build. Hostnames are advisory
/// here: the board authenticates by pin, not by name.
const DEFAULT_NAMES: &[&str] = &["hayabusa.proxy.rlwy.net", "localhost", "127.0.0.1"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => println!("{USAGE}"),

        // Re-derive the pin of a certificate that already exists — the
        // "what is prod actually presenting?" and "did the rotation land?" tool.
        [flag, path] if flag == "--pin" => {
            let pem = std::fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("error: reading {path}: {e}");
                exit(1);
            });
            println!("{}", pin_of_pem(&pem));
        }
        [flag, ..] if flag.starts_with('-') => {
            eprintln!("{USAGE}");
            exit(2);
        }

        names => {
            let names: Vec<String> = if names.is_empty() {
                DEFAULT_NAMES.iter().map(|s| s.to_string()).collect()
            } else {
                names.to_vec()
            };
            generate(&names);
        }
    }
}

fn pin_of_pem(pem: &str) -> String {
    let der = rustls_pemfile::certs(&mut pem.as_bytes())
        .next()
        .unwrap_or_else(|| {
            eprintln!("error: no CERTIFICATE block in that file");
            exit(1);
        })
        .unwrap_or_else(|e| {
            eprintln!("error: certificate PEM did not parse: {e}");
            exit(1);
        });
    tls::spki_sha256_hex(&der).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        exit(1);
    })
}

fn generate(names: &[String]) {
    let gen = tls::generate_self_signed(names).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        exit(1);
    });

    // Refuse to hand out an identity the server could not actually load.
    let loaded = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap_or_else(|e| {
        eprintln!("error: self-check failed, the minted identity does not load: {e}");
        exit(1);
    });
    if loaded.pin != gen.pin {
        eprintln!("error: self-check failed, pin is not stable across a load round-trip");
        exit(1);
    }

    println!("# ---------------------------------------------------------------");
    println!("# TLS_CERT_PEM  (Railway secret — safe to share, it is public)");
    println!("# ---------------------------------------------------------------");
    print!("{}", gen.cert_pem);
    println!();
    println!("# ---------------------------------------------------------------");
    println!("# TLS_KEY_PEM   (Railway secret — SECRET, stored nowhere else)");
    println!("# ---------------------------------------------------------------");
    print!("{}", gen.key_pem);
    println!();
    println!("# ---------------------------------------------------------------");
    println!("# The pin the firmware carries.");
    println!("# Definition: lowercase hex SHA-256 of the certificate's DER");
    println!("# SubjectPublicKeyInfo (the whole SPKI SEQUENCE, not the raw");
    println!("# Ed25519 point and not the whole certificate). Cross-check:");
    println!("#   openssl x509 -in cert.pem -pubkey -noout \\");
    println!("#     | openssl pkey -pubin -outform der | openssl dgst -sha256");
    println!("# ---------------------------------------------------------------");
    println!("{}", gen.pin);
    println!();
    println!("# Subject alt names: {}", names.join(" "));
    println!("# Key type: Ed25519. Max TLS record the server will emit: {} bytes on the",
        tls::MAX_RECORD_WIRE);
    println!("# wire ({} bytes of record body) — size the board's receive buffer to it.",
        tls::MAX_RECORD_BODY);
}

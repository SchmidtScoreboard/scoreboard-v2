# Rust firmware (esp-rs) — setup + display spike

This is the new all-Rust firmware for the Adafruit MatrixPortal S3, replacing the
C++ Arduino firmware in `../firmware`. We keep the C++ one as a working fallback
until this reaches parity.

**Right now this is only the display spike** — it proves `esp-hub75` can drive the
panel in Rust (HUB75 + WiFi coexistence is what bit us in C++). Nothing else yet.

> ✅ This **builds clean** with the esp toolchain (verified on this machine). I can't
> *flash* it (that's your hardware) — so flash it and report what the panel shows.

## 1. One-time toolchain install

```sh
cargo install espup espflash
espup install                 # installs the xtensa "esp" Rust toolchain
. $HOME/export-esp.sh         # adds it to PATH (add to your shell profile)
```

## 2. Build + flash the spike

Plug the board in over USB-C (data cable). Then **source the esp env first**
(it doesn't persist between shells), and run:

```sh
. ~/export-esp.sh
cd firmware-rs
cargo run --release
```

`cargo run` builds, flashes, and opens the serial monitor (the `runner` in
`.cargo/config.toml`).

**Native-USB S3 flashing dance (needed every flash):**
1. If espflash says *"Error while connecting"*: **hold BOOT, tap RESET, release BOOT**
   to drop into download mode, then re-run `cargo run --release`.
2. After *"Flashing has completed!"* the board sits in download mode
   (`boot:0x0 … waiting for download`). **Physically tap RESET** (not BOOT) to boot the
   app — the USB auto-reset is a no-op on this board.

✅ Confirmed: with this dance, the spike lights the panel on the real MatrixPortal S3.

## 3. What you should see

Top of the panel: four 16px-wide bars — **RED, GREEN, BLUE, WHITE** (left→right) —
and **"RUST OK"** in yellow below.

Please report:
- Did the panel light up at all?
- Are the colors in that order, or swapped (tells us if R/B are flipped for this panel)?
- Any flicker? (If so, we add the `iram` feature to `esp-hub75` — built for exactly that.)
- Paste any serial output / build errors.

## Pin map (MatrixPortal S3, from the working C++ firmware)

| HUB75 | GPIO | | HUB75 | GPIO |
|---|---|---|---|---|
| R1 | 42 | | A (addr0) | 45 |
| G1 | 41 | | B (addr1) | 36 |
| B1 | 40 | | C (addr2) | 48 |
| R2 | 38 | | D (addr3) | 35 |
| G2 | 39 | | E (addr4) | 21 (unused on 64x32) |
| B2 | 37 | | CLK | 2 |
| OE (blank) | 14 | | LAT | 47 |

## Fallback: if the build config / versions fight you

The esp-hal crate versions are tightly coupled and move fast. If `cargo build` errors
on versions or the linker, regenerate a correct base and drop in our code:

```sh
cargo install esp-generate
esp-generate --chip esp32s3 firmware-rs2     # accept defaults (alloc on)
# then in firmware-rs2:
#   - add to Cargo.toml:  esp-hub75 = { version = "0.11", features = ["esp32s3"] }
#                         embedded-graphics = "0.8"
#   - replace src/main.rs with our firmware-rs/src/main.rs
cargo run --release
```

Tell me which esp-hal version `esp-generate` picked and I'll align everything to it.

# firmware-dist

The over-the-air firmware release the server hosts, built and signed by
`scripts/release-firmware.sh`:

- `firmware.bin` — the flashable ESP32-S3 app image (espflash `save-image`
  output of the firmware-rs release build).
- `firmware.json` — `{ "version", "size", "sha256", "sig" }`: CalVer version,
  byte count, hex sha256 of the raw image bytes, and hex Ed25519 signature over
  the raw image bytes (key: `~/.config/scoreboard/firmware-signing.key`, never
  committed; the matching pubkey is baked into the firmware as
  `firmware-rs/src/fw_pubkey.rs`).

Both files are committed so the server's Docker image ships them (the
Dockerfile copies this directory and sets `FIRMWARE_DIR=/firmware-dist`).
The server serves `GET /firmware/latest` (the JSON) and `GET /firmware.bin`
(the bytes); with no valid dist present the feature is quietly off — no update
offers, 404s on both routes. This README also keeps the directory non-empty so
the Dockerfile `COPY` never fails.

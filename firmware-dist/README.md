# firmware-dist

The over-the-air firmware releases the server hosts, one directory per release
channel — `stable/`, `beta/`, `dev/` — each built and signed by
`scripts/release-firmware.sh <channel>` (default `stable`). A channel directory
is optional: absent just means that channel is off. Each present channel holds:

- `firmware.bin` — the flashable ESP32-S3 app image (espflash `save-image`
  output of the firmware-rs release build).
- `firmware.json` — `{ "version", "size", "sha256", "sig" }`: CalVer version,
  byte count, hex sha256 of the raw image bytes, and hex Ed25519 signature over
  the raw image bytes (key: `~/.config/scoreboard/firmware-signing.key`, never
  committed; the matching pubkey is baked into the firmware as
  `firmware-rs/src/fw_pubkey.rs`).

Versions share ONE CalVer space across all channels — the release script
refuses a version any channel has already published, so a version string always
names a unique image.

Everything here is committed so the server's Docker image ships it (the
Dockerfile copies this directory and sets `FIRMWARE_DIR=/firmware-dist`).
The server validates every present channel at startup (a broken one refuses
startup) and serves `GET /firmware/<channel>/latest` (the JSON) and
`GET /firmware/<channel>.bin` (the bytes). Boards are offered the newest
version among the channels their configured `fw_channel` allows: stable boards
see only `stable/`; beta adds `beta/`; dev adds `dev/`. This README also keeps
the directory non-empty so the Dockerfile `COPY` never fails.

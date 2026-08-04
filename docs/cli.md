# The server's command line

Everything the `server` binary can do besides serving boards: run modes,
environment variables, and the built-in pipeline debugger (`server tui`).
Run everything from the repo root with `cargo run -p server --` (or invoke a
release binary directly — flags are identical).

## Run modes

| Invocation | What it does |
|---|---|
| `server` | The real thing: polls ESPN, renders frames, serves boards + apps over one websocket endpoint. |
| `server tui` | Interactive pipeline debugger — see below. |
| `server tui --feed <id> --dump` | Headless one-shot capture of a feed — see below. |
| `server team-names` | Headless name-fitting report: every team, in every name style its league offers, measured in the board's own fonts against the team row, plus an OVERFLOW section for anything that still doesn't fit after shortening. Fetches live catalogs (golf/F1 names come from the leaderboard feeds), prints, exits. The "notify me for correction" tool — run it after touching shortening rules. |
| `server --preview` | Renders a fixed set of sample games to 8×-scaled PNGs in `/tmp/sb_preview` and exits. Quick eyeball of layout changes without any network. |
| `server --fetch-catalog` | Fetches the full team catalog from ESPN and caches it to disk, then exits. Run this once so the demo slate has real team names/colors to draw from. |

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `7878` | HTTP/websocket listen port. Railway injects `8080` for its HTTP edge, which is why both prod services pin an explicit `PORT=7878` to match their TCP proxies (the boards' `ws://`/`wss://` path). Serves plaintext and TLS on the same port — see [TLS on the board link](#tls-on-the-board-link). |
| `TLS_CERT_PEM` | unset | PEM certificate for the board link. Unset = plaintext only. Minted by hand with `gen-tls-cert`; never generated at boot. |
| `TLS_KEY_PEM` | unset | PEM private key matching `TLS_CERT_PEM`. Secret. Both variables must be set together; anything else logs an error and degrades to plaintext rather than crash-looping. |
| `POLL_SECS` | `180` | ESPN poll interval. Polling skips entirely while nothing is connected. |
| `STATE_DIR` | `.` | Where `scoreboard.db` (pairings, board configs) and the catalog cache live. |
| `FIRMWARE_DIR` | `./firmware-dist` | Where the OTA release channels live (`{stable,beta,dev}/firmware.{bin,json}`). Loaded once at startup and integrity-checked: a missing channel is quietly off, a *broken* one refuses to start (a board must never download an image that won't verify). The Dockerfile ships `firmware-dist/` inside the server image and points here at `/firmware-dist`. |

## Debug-friendly HTTP routes

All curl-able; the app uses the same ones.

- `GET /render/{game_id}.png?board={id}` — the game's current frame as a PNG,
  spoiler-scoped to the board. Unknown ids render a "NO GAME" error frame
  (deliberately not a 404, so a stale card shows *something* diagnosable).
- `GET /game/{game_id}?board={id}` — the full processed `Game` as pretty JSON,
  spoiler-redacted per the board's rules (`"spoiled": true` when it was).
  Omitting `board` serves the raw data. Unknown id → plain-text 404.
- `GET /sample/{off|normal|strict}.png` — the fixed sample game rendered at
  each spoiler level (drives the app's spoiler-settings preview rows).
- `GET /offmode/{id}.png` — what an off mode looks like on the panel, rendered
  at a fixed time so the bytes are stable (`max-age=86400`, same rule as
  `/guide`). Drives the app's "When off" rows. 404 on an id this server does
  not offer — a wrong picture would misrepresent what the board will do.
- `GET /demo/games` — the synthetic slate as JSON. The whole demo set, on the
  same poll cadence as live data; this is how you learn the `demo-*` ids
  without pairing a board. Not board-scoped (synthetic games carry no spoiler
  risk), and `no-store` because the slate advances.
- `GET /demo/render/{id}.png` — one demo game's 64x32 frame. Unknown id gets a
  visible `NO GAME` frame rather than a 404, same as `/render`.
- `GET /guide/{joining-wifi|finding-server|setup|pair-code}.png` — the board's
  firmware-local screens (connect progress on either leg, BLE setup, the
  pairing screen with a fake `000000` code), rendered by the same board-core
  code the device runs — the app's troubleshooting sheet fetches these so its
  "if your board shows THIS" images can never drift from the panel. Served
  with `Cache-Control: public, max-age=86400` (they change only with
  releases). Unknown name → plain-text 404.
- `GET /firmware/{stable|beta|dev}/latest` — that channel's signed
  `firmware.json` (version, size, sha256, sig). Plain-text 404 for an unknown
  channel or one with no dist hosted.
- `GET /firmware/{stable|beta|dev}.bin` — the image itself. `?u={token}` is the
  one-time nonce a board's update offer carries, scoping the download to that
  board (the token retires when the stream drains); without it (curl, tests)
  it just serves the file. Either way the bytes are identical and the
  board verifies the signature before installing.
- `GET /firmware/notes/{version}` — that release's user-facing patch notes as
  plain text (`- ` bullets), read from `FIRMWARE_DIR/notes/{version}.txt` at
  request time. `{version}` is strictly digits-and-dots; anything else (and
  any version without a notes file) is a plain-text 404.

## `server tui` — the pipeline debugger

One screen showing the whole data pipeline side by side: **raw feed JSON →
parsed `Game`s → rendered 64×32 frames** (drawn in the terminal with
half-blocks). Every parse goes through `espn::parse_feed`, the same functions
the poll loop uses — what this tool shows is what the server would serve.
Built for debugging live-data issues: when a card renders wrong, this answers
"is it the feed, the parse, or the render?" in one place.

### Interactive mode

```
server tui                        # all feeds available, fetch on demand
server tui --feed f1              # preselect + fetch one feed on launch
server tui --feed f1 --input x.json   # load a saved capture instead of fetching
server tui --out captures/        # where s/S write (default ./sbtui-out)
```

Three panes, cycled with `Tab` / `Shift-Tab`: **feeds** (left), **events**
(middle), **detail** (right). The detail pane has three tabs.

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | Next / previous pane |
| `↑`/`↓` or `k`/`j` | Move selection (or scroll, in the detail pane) |
| `Enter` (feeds pane) | Fetch the selected feed |
| `r` | Re-fetch the selected feed |
| `1` / `2` / `3` | Detail tab: raw event JSON / parsed `Game` / rendered frame (focus follows, so `j`/`k` scroll it immediately) |
| `v` | Cycle the frame variant: normal → spoiled-normal → spoiled-strict |
| `s` | Save the selected **event**'s artifacts to the out dir |
| `S` | Save the whole **feed** (full raw doc + every event) |
| `g` | Jump the detail scroll back to the top |
| `PgUp` / `PgDn` | Page the detail pane |
| `q` / `Esc` / `Ctrl-C` | Quit |

### Headless mode (`--dump`)

The scriptable path — used by agents, CI, and quick captures. Fetches (or
loads) one feed, writes every artifact, prints the paths, exits:

```
server tui --feed pga --dump                # live fetch → ./sbtui-out/pga/
server tui --feed pga --dump --out /tmp/x   # elsewhere
server tui --feed pga --dump --input saved-raw.json   # re-parse a capture
```

`--input` replays a previously saved `raw.json` through the current parser —
the regression loop for feed bugs: capture once while the problem is live,
then iterate on the parser against the frozen input.

### Capture layout

`s`, `S`, and `--dump` all write the same shape:

```
sbtui-out/
  pga/
    raw.json                 # the full feed document, pretty-printed
    url.txt                  # where it came from ("(file) …" for --input)
    {event-id}/
      raw.json               # just this event's subtree
      game.txt               # the parsed Game struct, pretty-debug
      frame-normal.png             # 8×-scaled renders of the board frame
      frame-spoiled-normal.png     #   …under each spoiler mode
      frame-spoiled-strict.png
```

### Feed ids

`nhl` `nba` `mlb` `nfl` `cfb` `cbb` `wcbb` `wnba` `mls` `epl` `wc` `pga`
`lpga` `f1`, plus `nhl-aux` (the api-web.nhle.com situations feed that powers
power-play display) and `demo` (the generated demo slate — no network).

## Scripts

| Script | What it does |
|---|---|
| `scripts/dev-server.sh` | Local demo server under cargo-watch (rebuild on save). |
| `scripts/server.sh` | Local server against real ESPN. |
| `scripts/simulator.sh` | The desktop board simulator. |
| `scripts/flash.sh [port]` | Build + flash `firmware-rs` over USB (sources `export-esp.sh`). espflash hard-resets after flashing, so it normally reboots itself; tap RESET only if the panel stays dark. |
| `scripts/release-firmware.sh [stable\|beta\|dev]` | Build + sign an OTA release into `firmware-dist/<channel>/` — see below. |
| `scripts/logs.sh` | Tail the local server log. |

A dev firmware build's fallback server address comes from
`firmware-rs/src/config.rs`, which is tracked and secret-free (a fresh clone
builds as-is). Point it at your own machine without editing it — and without
risking a commit — with `SCOREBOARD_DEV_HOST=192.168.x.y`, or keep your values
in the gitignored `firmware-rs/local.env` and `set -a; source
firmware-rs/local.env; set +a` first. Provisioned boards get their server over
BLE anyway; this is only the fallback. Prod builds (`--features prod`, what
releases ship) always use the prod host.

## Release channels (OTA)

Three channels — `stable`, `beta`, `dev` — each a directory under
`FIRMWARE_DIR` holding one `firmware.bin` + signed `firmware.json`. A board is
enrolled in one (`SetFwChannel` from the app) and may be offered dists from its
channel **and every channel below it**: stable boards see stable only, beta
sees stable+beta, dev sees all three. Within that set the newest CalVer wins,
and only if it's strictly newer than what the board reports running.

```
scripts/release-firmware.sh            # stable (the default)
scripts/release-firmware.sh beta
FEATURES="" scripts/release-firmware.sh dev    # dev-server build, dev channel
```

The script reads the version from `firmware-rs/Cargo.toml` (`[package].version`,
CalVer `YYYY.M.P`), regenerates `firmware-rs/src/fw_pubkey.rs` from the signing
key at `~/.config/scoreboard/firmware-signing.key`, builds with `--features
prod` (override with `FEATURES=`), converts the ELF to an app image sized
against the 3MB `ota_0` slot, signs it, and appends to
`firmware-dist/RELEASES.log`.

That ledger is append-only and the reason releases are safe: a release
*overwrites* its channel's `firmware.json`, so checking only the current dists
would let a superseded version be republished as different bytes — which no
board could ever disambiguate. Every version ever published is remembered, and
versions live in ONE space across all channels. A repeat is refused; bump the
patch instead.

User-facing patch notes live beside the channels in `firmware-dist/notes/`
(`<version>.txt`, short `- ` bullets) and are served at
`GET /firmware/notes/<version>`; the release script warns — without failing —
when the version being released has no notes file.

Commit `firmware-dist/` afterwards (and `fw_pubkey.rs` if the key changed) —
the Dockerfile ships it inside the server image — then deploy. Boards see the
update on their next `BoardState`.

## TLS on the board link

The board talks to exactly **one** server, so it authenticates that server by
**pinning its public key** rather than validating a PKI chain. That drops two
things an ESP32-S3 cannot afford: a CA bundle in flash, and a wall clock. The
board has no RTC and no NTP, so it could never check `notAfter` — PKI
validation would be theatre. A pin is checkable with a SHA-256 and nothing else.

Plaintext is **not** removed. Port 7878 keeps serving it, because boards in the
field speak plain `ws://` and Railway's HTTPS edge terminates the app's TLS and
forwards *plaintext* HTTP to this same port. The listener sniffs the first byte
of every connection instead: `0x16` (the TLS handshake ContentType) gets wrapped
in rustls, anything else goes down the old plaintext path untouched. The peek is
a real `MSG_PEEK`, so neither hyper nor rustls can tell the byte was inspected.

OTA firmware downloads deliberately **stay plain HTTP**. Images are Ed25519
signed and verified on-device with a downgrade guard, so transport adds
nothing: an on-path attacker can stall a download or replay the version already
running, but cannot inject an image. Keeping them plain also avoids running a
TLS record layer *during* the download — the tightest memory moment on the
board, and the one path where a failed allocation aborts rather than returning
an error.

Note what this does **not** claim: the TLS read/write buffers (9,344 B) live
outside the connect loop and stay resident across the download, so they are
part of that peak. They are deliberately allocated once rather than per
reconnect — re-minting them every time the Railway proxy recycles a socket
would fragment the heap far more than it would save. Watch `free_heap_bytes` on
the DEBUG INFO screen after an update; that is the empirical check.

### The numbers the firmware must match

| | |
|---|---|
| Protocol | **TLS 1.3 only.** The `tls12` feature is not compiled in. |
| Certificate key | **Ed25519.** The only type rustls signs with, embedded-tls verifies (its `ed25519` feature), *and* the firmware already links (ed25519-dalek, for OTA) — so the handshake costs the board no new crypto code. P-384 is the only alternative both ends support and would drag the whole `p384` crate onto the device; embedded-tls has no P-256 *certificate* support. |
| Key exchange | ECDHE P-256 — embedded-tls's only named group. |
| **Pin value** | **Lowercase hex SHA-256 of the certificate's DER-encoded `SubjectPublicKeyInfo`.** 64 hex characters. |
| `max_fragment_size` | **4096.** rustls counts the 5-byte record header inside this number, so: plaintext ≤ **4091** B/record, record body (ciphertext) ≤ **4108** B, whole record on the wire ≤ **4113** B. Size the board's receive buffer against 4108 (embedded-tls reads the 5-byte header into its own stack buffer). |

"SubjectPublicKeyInfo" means the whole SPKI `SEQUENCE` — `AlgorithmIdentifier`
plus the `BIT STRING` — exactly as those bytes appear inside the certificate.
**Not** the raw 32-byte Ed25519 point, and **not** the whole certificate. For
Ed25519 the SPKI is always 44 bytes: the fixed prefix
`30 2a 30 05 06 03 2b 65 70 03 21 00` followed by the 32-byte key.

This is the same value as HPKP / `curl --pinnedpubkey`, so it cross-checks
against stock tooling (needs an OpenSSL that knows Ed25519 — macOS's LibreSSL
does not):

```
openssl x509 -in cert.pem -pubkey -noout \
  | openssl pkey -pubin -outform der | openssl dgst -sha256
```

### Both halves of the check are load-bearing

`verify_certificate` (pin match) establishes **identity** — this is the key we
expect. `verify_signature` (the TLS 1.3 CertificateVerify check) establishes
**possession** — the peer actually holds the matching private key. A certificate
is public: anyone on-path can copy the server's and replay it, so a pin check
with `verify_signature` stubbed to `Ok(())` is defeated outright. Both must be
genuinely implemented. `crates/server/src/serve.rs` has the same verifier pair
written against rustls, as the reference for the firmware's `TlsVerifier`.

### Minting the certificate

Run once, by hand, by the owner — **not** at boot. A certificate minted per
deploy would change the pin on every deploy and lock out every board in the
field.

```
cargo run -p server --bin gen-tls-cert                      # default SANs
cargo run -p server --bin gen-tls-cert -- board.example.com # or your own
cargo run -p server --bin gen-tls-cert -- --pin cert.pem    # pin of an existing cert
```

It prints the PEM certificate, the PEM private key, and the pin. Paste the two
PEMs into Railway as `TLS_CERT_PEM` / `TLS_KEY_PEM` (both services), and the pin
into the firmware. The key is stored nowhere else — there is no recovery, and
losing it means minting a new identity, which is a firmware rotation.

That last step is the *hosted* server's route. Running your own means the pin
goes into the board over BLE instead of into the build — see
[Running your own server](#running-your-own-server).

Default SANs are `hayabusa.proxy.rlwy.net localhost 127.0.0.1`, and validity is
10 years. Both are conveniences for `curl`/`openssl`: the board authenticates by
pin, and cannot read dates or names anyway.

The server logs the pin of whatever it loaded at startup —

```
scoreboard server on 0.0.0.0:7878 — plaintext + TLS 1.3 (board pin 51f5f6…, max TLS record 4113 bytes on the wire)
```

— so "does the deployed certificate match what shipped in firmware?" is one
`railway logs` away.

### Rotating the key

The firmware carries **two** pins (current + successor) from day one, so a
rotation is a staged rollout rather than a cliff. Each step is safe on its own,
and boards keep working throughout:

1. `cargo run -p server --bin gen-tls-cert` — mint the successor identity. Keep
   the PEMs somewhere safe; do **not** deploy them yet.
2. Add its pin to the firmware as the **second** pin, leaving the current one
   first. Ship that firmware and wait for the fleet to take it — a board that
   has not updated yet still trusts the pin the server is presenting.
3. Swap `TLS_CERT_PEM` / `TLS_KEY_PEM` on Railway to the successor and redeploy.
   Confirm the pin in the startup log is the successor's. Every updated board
   accepts it via its second pin.
4. Next release, drop the retired pin and promote the successor to first.

Never skip step 2's soak: swapping the server certificate before the fleet
carries the successor pin locks out every board that has not updated, and they
cannot be recovered over the network — the connection they would recover over is
the one that just broke.

This staged dance is for the **hosted** fleet, whose pins arrive in firmware.
A self-hosted board carries one provisioned pin and rotates by being
re-provisioned — see below.

## Running your own server

Two supported routes to a working board: run this server yourself, or pay once
for access to the hosted one. The firmware is the same either way. The hosted
server's pin is compiled in as the **default**; self-hosting **replaces** it
with yours, over the same BLE provisioning that already carries the Wi-Fi
credentials and the server address.

**1. Run the server.** Anywhere the board can reach — a box on your LAN, a VPS,
a container host. `PORT` is the port the board will dial and `STATE_DIR` decides
where `scoreboard.db` and the catalog cache land; step 2 adds the TLS pair.

```sh
cargo run -p server --fetch-catalog     # once, so teams have names and colors
PORT=7878 STATE_DIR=~/scoreboard cargo run -p server --release
```

**2. Mint a certificate, once.** By hand, and kept forever — not per deploy,
and never at boot. The board authenticates by pin, so a fresh certificate every
deploy would change the pin every deploy and lock out your own board.

```sh
cargo run -p server --bin gen-tls-cert -- board.lan
```

It prints a PEM certificate, a PEM private key, and a 64-hex-character **pin**.
Give the server the two PEMs as `TLS_CERT_PEM` / `TLS_KEY_PEM`. The pin is the
part you keep — it is public, so a note on your phone is fine. Lost it?
Re-derive it from the certificate (`gen-tls-cert --pin cert.pem`) or read it off
the server's startup log (`board pin 51f5f6…`).

**3. Provision the board with all three facts together.** In the app's setup
flow: the Wi-Fi network, the server as `host:port`, and the pin. They travel
over BLE in one go and land in one flash record, then the board reboots and
dials your server. Leave the pin blank and the board keeps the built-in
default, which is the hosted server.

A provisioned pin is the **only** pin that board trusts. It does not join the
built-in one, it replaces it — so a board pointed at your server cannot be
talked into accepting the hosted server's key, or anyone else's who holds a
copy of that certificate.

Four things worth knowing before you type a pin:

- **It has to be exact**: 64 hex characters, no `0x`, no spaces. Anything else
  is refused at the BLE layer — the write fails and *nothing* is saved. That is
  deliberate: a board that accepted a broken pin would save, reboot, and never
  connect again, with no clue on the panel as to why.
- **A wrong-but-well-formed pin fails closed, forever.** The board rejects the
  handshake, retries, and sits on FINDING SERVER. There is no plaintext
  fallback, and no remote fix — the connection you would fix it over is the one
  that is broken. `[tls] pinned to …` on the serial console names the pin the
  board is actually using, and whether it came from provisioning or the build.
- **Hold-to-reset is the way out.** Holding the knob wipes the whole record —
  Wi-Fi, server, pin, and the board's identity — and returns it to BLE setup,
  where you can enter the right pin. It needs no server, which is exactly why
  it is the recovery path for a wrong pin.
- **Rotating your own key is re-provisioning.** Swap the certificate, then
  hold-to-reset and set the board up again with the new pin. (Reissuing a
  certificate for the *same* key keeps the pin: the pin is over the key, not
  the certificate.)

Provisioning is plain GATT if you would rather not use the app — service
`21ba0000-5363-6f72-6562-6f6172640001`, all writes, with the password written
**last** because it is what triggers the save and reboot:

| Characteristic | Value |
|---|---|
| `…0002` | Wi-Fi SSID |
| `…0004` | server `host:port` (optional) |
| `…0006` | server TLS pin, 64 hex characters (optional) |
| `…0003` | Wi-Fi password — **write this last** |

## Recipes

**A card renders wrong on the board.**
`server tui --feed <league> --dump`, then look at the three stages: is the
data wrong in `raw.json` (feed problem), wrong in `game.txt` (parse problem),
or right in both but wrong in `frame-*.png` (render problem)?

**Capture a fixture while a bug is live.**
`server tui --feed nhl --dump --out captures/ot-bug/` — the raw JSON is now
frozen; iterate on the parser with `--input captures/ot-bug/nhl/raw.json`.

**Check spoiler safety of a layout change.**
Open the frame tab interactively and cycle `v` through the three variants —
or diff the three `frame-*.png` files from a dump.

**What is prod actually serving for this game?**
`curl 'http://hayabusa.proxy.rlwy.net:47958/game/<id>?board=<board-id>'` —
the exact JSON the app's Info sheet shows, spoiler rules included. Still plain
`http://`: the port serves both, and the sniff routes this to the same handler
a board's TLS connection would reach.

**Does the deployed certificate match what shipped in firmware?**
Read the pin off the startup log (`railway logs | grep 'board pin'`) and
compare it to the firmware's. To re-derive it from a PEM you have on disk:
`cargo run -p server --bin gen-tls-cert -- --pin cert.pem`.

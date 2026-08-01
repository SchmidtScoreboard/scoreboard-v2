# The server's command line

Everything the `server` binary can do besides serving boards: run modes,
environment variables, and the built-in pipeline debugger (`server tui`).
Run everything from the repo root with `cargo run -p server --` (or invoke a
release binary directly — flags are identical).

## Run modes

| Invocation | What it does |
|---|---|
| `server` | The real thing: polls ESPN, renders frames, serves boards + apps over one websocket endpoint. |
| `server --demo` | Same server, synthetic slate — 21 generated games exercising every feature (situations, leaderboards, spoilers, scrolling). Never contacts ESPN. The Railway `dummy` service is exactly this (it sets `DEMO=1`). |
| `server tui` | Interactive pipeline debugger — see below. |
| `server tui --feed <id> --dump` | Headless one-shot capture of a feed — see below. |
| `server team-names` | Headless name-fitting report: every team, in every name style its league offers, measured in the board's own fonts against the team row, plus an OVERFLOW section for anything that still doesn't fit after shortening. Fetches live catalogs (golf/F1 names come from the leaderboard feeds), prints, exits. The "notify me for correction" tool — run it after touching shortening rules. |
| `server --preview` | Renders a fixed set of sample games to 8×-scaled PNGs in `/tmp/sb_preview` and exits. Quick eyeball of layout changes without any network. |
| `server --fetch-catalog` | Fetches the full team catalog from ESPN and caches it to disk, then exits. Run this once so `--demo` has real team names/colors to draw from. |

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `7878` | HTTP/websocket listen port. Railway injects `8080` for its HTTP edge, which is why both prod services pin an explicit `PORT=7878` to match their TCP proxies (the boards' plain-`ws://` path). |
| `DEMO` | unset | Any value = demo mode (same as `--demo`). |
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
| `scripts/flash.sh [port]` | Build + flash `firmware-rs` over USB (sources `export-esp.sh`; **tap RESET after** — this board parks in the bootloader post-flash). |
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
the exact JSON the app's Info sheet shows, spoiler rules included.

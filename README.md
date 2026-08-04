# Scoreboard

A friends-and-family LED-matrix sports scoreboard: a small wall device that
shows live scores for your favorite teams and turns itself on and off around
game time. This is a ground-up rebuild of an earlier Raspberry-Pi project
([SchmidtScoreboard](https://github.com/SchmidtScoreboard)), redesigned now that
the goals are clearer.

## Vision: smart server, dumb client, thin app

- **Server** does all the thinking — pulls live data from ESPN, decides what each
  board should show, and renders ready-to-display image frames.
- **Device** is a dumb pixel display — it receives frames over WiFi and blits them
  to a 64×32 RGB LED matrix. No game logic on-device, so changing how anything
  *looks* never requires a firmware update.
- **App** is a thin remote — pick favorite teams, set brightness, toggle power.
  The "turn on when my team plays" intelligence lives on the server, so it works
  with the app closed.

**Account-free.** No logins. Identity is a keypair exchanged when you pair your
phone to a board (SSH / Tailscale style). This is built for a handful of devices
among friends and family — not a commercial product, and no subscriptions.

## How it fits together

```
          ESPN ──poll──>  SERVER  (Rust + axum, hosted on Railway)
                            │   renders 64x32 frames (content-addressed)
              WebSocket ────┴──── WebSocket
                  │                       │
       DEVICE (ESP32 + HUB75)        APP (native iOS / Android)
       receive frame, blit          pick teams, brightness, power
       firmware owns only           shares a Rust core (uniffi) with the
       boot/setup/clock screens     server protocol — no duplicated logic
```

- Frames are **content-addressed** (a hash of their pixels): the device caches
  them and only fetches new ones, and every board watching the same game shares a
  single render.
- A "screen" is a manifest the device rotates through locally, so the connection
  stays idle until something actually changes.
- Commands are **server-enforced** against a per-board allow-list that the board
  syncs from BLE pairing — the keypairs are the account-free identity layer.

## Repo layout

```
crates/
  protocol/    shared wire types + data model (server, device, and app core all use it)
  board-core/  sans-IO device logic (frame cache, rotation, framebuffer) — shared by the simulator and, later, the firmware
  server/      axum server: ESPN data, rendering, websocket, HTTP catalog/schedule
  simulator/   desktop board simulator (a window) — runs board-core, no hardware needed
  app-core/    the app's brain (websocket client) exported to Swift/Kotlin via uniffi
ios/
  ScoreboardApp/  the SwiftUI app + hand-rolled Xcode project
  build-core.sh   regenerates the Swift bindings + AppCore.xcframework
```

## Current state

Working and verified end-to-end:

- **Server** pulls **live ESPN games** (NHL / NBA / MLB / NFL) behind a swappable
  `SportsDataSource` trait, normalizes team colors + status, and **renders in the
  original scoreboard style** (two team-colored rows, accent stripe, name + score,
  period/clock line). Serves `/catalog`, `/schedule`, and a `/ws` websocket.
- **Simulator** shows the live rotation in a window — the exact `board-core` the
  real firmware will run.
- **iOS app** (SwiftUI) connects through the shared Rust core and controls a board
  (power, brightness) — verified running in the iOS Simulator against the local
  server.

Stubbed / not done yet:

- Ed25519 auth is currently accept-any (real signatures next); the **BLE pairing**
  flow isn't built.
- **Sport-specific extras** (baseball bases/outs, football possession + down,
  hockey power-play, golf leaderboard) aren't rendered yet.
- The real team **catalog** the app shows is still mock data.
- **Richer app screens** (team picker, schedule, dashboard) — only basic controls
  so far.
- **Android** app — the core is ready, the Kotlin UI is not.
- **Hardware** — see below.

## Running it

```bash
# Terminal 1 — server (fetches live ESPN data)
cargo run -p server

# Terminal 2 — board simulator (opens a 64x32 window)
cargo run -p simulator

# Render sample boards to PNGs (no hardware or server needed)
cargo run -p server -- --preview        # writes /tmp/sb_preview/*.png

# Pipeline debugger: raw feed JSON -> parsed games -> rendered frames.
# Interactive TUI or headless captures — see docs/cli.md for the full tour.
cargo run -p server -- tui
cargo run -p server -- tui --feed pga --dump

# iOS app
./ios/build-core.sh    # build the Rust core -> xcframework + Swift bindings (after app-core changes)
./ios/run.sh           # build the app + launch it (reuses a booted simulator, else boots one)
# or open ios/ScoreboardApp/ScoreboardApp.xcodeproj to run from Xcode
```

## Hardware

An **Adafruit MatrixPortal S3** driving a **64×32 HUB75** RGB panel, with an
Adafruit seesaw **rotary encoder** for on-device control (brightness, sleep, and
a settings menu with diagnostics and updates). Powered over USB-C at **5V 3A**.

- **BLE provisioning** — no WiFi-hotspot dance. Wi-Fi credentials go phone →
  Bluetooth → flash and never touch the network.
- **Signed OTA updates** over Ed25519 with A/B slots and automatic revert, so a
  bad image can't brick a board.
- **TLS to the server**, with the server's public key pinned on the device.

**[DIY.md](DIY.md) walks the whole build**: parts, toolchain, running your own
server, flashing, and setup.

## Roadmap

1. Sport-specific render extras (complete the exact original look).
2. Richer iOS app screens (teams & big games, schedule / "watch a game", dashboard).
3. Real Ed25519 auth + the BLE pairing flow.
4. Hardware bring-up (ESP32-S3 + panel) and firmware that reuses `board-core`.
5. Android app (Kotlin UI over the shared core).
6. Deploy the server to Railway.

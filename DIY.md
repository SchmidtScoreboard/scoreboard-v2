# Build your own scoreboard

This walks the whole path: parts on a desk → a panel showing live scores, running
against **your own server**. Nothing here depends on the hosted service.

There are two ways to run a scoreboard:

1. **Your own server** — anywhere: a cloud box, a Raspberry Pi, or your laptop.
   This guide. You own the data, the keys, and the uptime.
2. **The hosted server** — a one-time fee, no server to run. You still build the
   hardware and flash it; you just skip [Part 2](#part-2--run-a-server). The
   hosted server only serves boards we've authorized: you send us your board's
   id, we send back an authorization code, and you flash it with the firmware
   (see [BYOD](docs/provisioning.md#byod--your-hardware-our-server)).

Everything below is the same for both except Part 2 and one field during setup.

---

## What you'll need

**Hardware** (~$70–90):

| Part | Notes |
|---|---|
| Adafruit **MatrixPortal S3** | The ESP32-S3 board that drives the panel. Plugs straight into the panel's IDC header. |
| **64×32 RGB LED matrix**, HUB75 | Any pitch — P3 (~19cm wide) is a good desk size, P4/P5 are larger. |
| **5V 3A USB-C power supply** | See [Power](#power) — this one matters more than it looks. |
| Adafruit **rotary encoder** (seesaw, I²C `0x36`) | Optional but recommended: brightness, sleep, and the on-device settings menu. STEMMA QT cable to the MatrixPortal. |
| USB-C **data** cable | For flashing. Many cheap cables are charge-only and will waste an hour of your life. |

**Software:** a Mac or Linux box with [Rust](https://rustup.rs), and Xcode if you
want the iOS app (an Android app doesn't exist yet).

### Power

The panel is the load, not the microcontroller. Measured across real rendered
frames, the worst case is about **1.3A** — but LED matrices draw in spikes, and
Wi-Fi transmit bursts land on top. **5V 3A** is the right supply.

Don't reach for "5V 4A" adapters: **3A is the maximum the USB-C spec allows at
5V**, so anything above it is out of spec. 3A gives better than 2× headroom over
the worst frame this thing renders.

---

## Part 1 — Toolchain

The ESP32-S3 is an Xtensa chip, which needs Espressif's Rust fork. One time:

```sh
cargo install espup espflash
espup install
```

`espup` writes `~/export-esp.sh`. **It does not persist between shells** — source
it in any terminal where you build firmware, or add it to your shell profile:

```sh
. ~/export-esp.sh
```

Then clone and check the host side builds:

```sh
git clone <your-fork-url> scoreboard && cd scoreboard
cargo test -p server
```

---

## Part 2 — Run a server

*(Skip this if you bought hosted access.)*

The server polls ESPN, renders every 64×32 frame, and pushes them to boards. The
board is a dumb display — all the intelligence lives here.

### 2.1 Fetch the team catalog (once)

```sh
cargo run -p server -- --fetch-catalog
```

Names, abbreviations, and team colors, cached to disk.

### 2.2 Mint a TLS certificate (once)

Boards require TLS and authenticate the server by **pinning its public key** —
no certificate authority involved, so nothing to renew and no Let's Encrypt
dance:

```sh
cargo run -p server --bin gen-tls-cert -- yourserver.example localhost 127.0.0.1
```

It prints three things: a **certificate**, a **private key**, and a **pin** (64
hex characters). Save all three.

> **Keep the private key safe and backed up.** It's the only thing your boards
> will trust. Losing it means re-provisioning every board by hand.

Hostnames are advisory — the board authenticates by pin, not by name — but
listing them makes `curl` and `openssl s_client` usable against your server.

**You never have to rotate this certificate on a schedule.** The board has no
clock, so it cannot evaluate expiry, and it pins the key rather than trusting a
CA. Expiry is a non-event (there's a test in the suite asserting exactly that).
Rotate only if the private key is compromised.

### 2.3 Run it

```sh
export TLS_CERT_PEM="$(cat cert.pem)"
export TLS_KEY_PEM="$(cat key.pem)"
cargo run -p server --release
```

Configuration, all optional:

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `7878` | Listen port. |
| `TLS_CERT_PEM` / `TLS_KEY_PEM` | unset | The PEMs from 2.2. **Unset = plaintext only, and no board will connect.** |
| `STATE_DIR` | `.` | Where `scoreboard.db` (pairings, board configs) and the catalog cache live. |
| `DEFAULT_TIMEZONE` | server local | Fallback zone for rendering clocks and start times. The app sets each board's zone when you pair, so this is a backstop. |
| `POLL_SECS` | `180` | ESPN poll interval. Polling pauses entirely while nothing is connected. |
| `FIRMWARE_DIR` | `./firmware-dist` | Where OTA release channels live, if you host your own updates. |

For hot-reload while hacking, `scripts/dev-server.sh` mints a dev certificate on
first run, exports it, and writes its pin into `firmware-rs/local.env` for you —
so step 3's build already has the pin it needs.

### 2.4 Reachable from your board

Your board needs to reach the server over TCP on `PORT`. On a home LAN that's
just your machine's address. If you host it publicly, a **raw TCP path** is what
you want — the board speaks its own TLS, so an HTTPS-terminating proxy is the
wrong shape. On Railway that means a **TCP proxy**, not the HTTP domain.

---

## Part 3 — Flash the firmware

Assemble first: MatrixPortal onto the panel's IDC header, power to the panel's
screw terminals, encoder on STEMMA QT if you have one.

Point the build at your server. Create `firmware-rs/local.env` (gitignored):

```sh
SCOREBOARD_DEV_HOST=192.168.1.50
SCOREBOARD_DEV_TLS_PIN=<the 64-hex pin from step 2.2>
```

Then, with the board plugged in over USB-C:

```sh
bash scripts/flash.sh
```

(`flash.sh` reads `local.env` itself; anything you export by hand still wins.)

It reboots into the new image on its own (`espflash` hard-resets after
flashing). If the panel stays dark, tap **RESET** — most likely if you had to
enter the bootloader by hand. And if flashing can't connect at all, hold
**BOOT**, tap **RESET**, then re-run.

The build **fails on purpose** if `SCOREBOARD_DEV_TLS_PIN` isn't set. There's no
safe default: a wrong pin produces a board that can never connect, so the error
comes at compile time rather than in your living room.

Watch it boot:

```sh
. ~/export-esp.sh && espflash monitor
```

You should see the version, then `SETUP` on the panel — it has no Wi-Fi yet.

---

## Part 4 — The app

```sh
bash ios/build-core.sh          # Rust core → Swift bindings
open ios/ScoreboardApp/ScoreboardApp.xcodeproj
```

Build to your iPhone or the Simulator. (BLE setup needs a **real device** —
the Simulator has no Bluetooth radio.)

---

## Part 5 — Set up the board

1. In the app, **Add Scoreboard**.
2. Choose **"Use my own server"** and enter:
   - **Address** — `host:port`, e.g. `192.168.1.50:7878`
   - **Pin** — the 64 hex characters from step 2.2
3. Pick your Wi-Fi network and enter the password.
4. The app sends everything over Bluetooth; the board saves it and reboots.
5. The panel shows a **6-digit pairing code**. Type it into the app.

Done — live scores.

> Setup **refuses** a mismatched address and pin (a custom address with no pin,
> or a pin with no address). Either would produce a board that dials one server
> while trusting another's key: a handshake that can never succeed, on a board
> that looked like it set up fine.

---

## Using the board

- **Turn the knob** — brightness.
- **Tap** — the menu. Your screens are at the top in the order the app lists
  them: Sports, Off, then any you've added (pick one to show it). Below them
  are version and updates, pairing code, diagnostics (Wi-Fi signal, IP,
  uptime, free memory), and factory reset. Double-tap opens the same menu.
  On firmware older than 2026.8.4, a tap is sleep / wake and a double-tap
  opens the settings part only.
- **Hold ~10 seconds** — factory reset from any state, including a board that
  can't connect. Wi-Fi, server, pin, and the board's identity are all erased and
  it returns to Bluetooth setup, unpaired from every phone. (A board authorized
  for the hosted server keeps its identity, which is its authorization.)

That last one is your way out of any mistake in Part 5. It needs no server and no
app, which is exactly why it's there.

---

## Troubleshooting

**Panel dark** — power. Check the adapter is 5V 3A and the panel's own power
leads are connected; the MatrixPortal being lit doesn't mean the panel is fed.

**`JOINING WI-FI` forever** — wrong password, or 5GHz-only. The ESP32-S3 is
**2.4GHz only**; if your router merges both bands under one name, some setups
need a separate 2.4GHz SSID.

**`FINDING SERVER` forever** — Wi-Fi is fine, the server isn't reachable.
Check it's running, that the address you provisioned is right, and that nothing
firewalls the port. From another machine:

```sh
openssl s_client -connect YOUR_HOST:7878 -tls1_3
```

You should see `Protocol: TLSv1.3` and an `ed25519` peer signature. If that
works but the board still can't connect, the **pin** is likely wrong — verify:

```sh
cargo run -p server --bin gen-tls-cert -- --pin cert.pem
```

If it doesn't match what you provisioned, hold the knob 10 seconds and redo
Part 5. (Requires OpenSSL 3; macOS ships LibreSSL, which can't read Ed25519 —
`brew install openssl@3`.)

**Ghosting — faint glow on pixels that should be black** — the panel clock is
too fast for your panel. Lower `Rate::from_mhz(10)` in `firmware-rs/src/main.rs`.

**Board connects, no games** — normal out of season, or nothing matches your
preferences yet. Add a favorite team in the app, or turn on **Demo games** in
the app's scoreboard settings
to confirm the whole pipeline end to end.

---

## Developing without hardware

```sh
cargo run -p simulator
```

A desktop window rendering the same `board-core` the firmware runs — same
screens, same menu, same gestures. Mouse wheel is the knob, and there's a hold
button for the reset gesture. `--server`, `--board`, `--tz`, and `--headless`
flags are there too. Most display work never needs a physical board.

---

## Adding your own screen

The screens a board can show besides scores (the clock, weather) are
**declared by your server**, not built into the app. Add one in
[`crates/server/src/offmode.rs`](crates/server/src/offmode.rs): an entry in
`off_modes()` describing it, and an arm in `render_off_mode()` that draws it.

That's the whole change. Because the apps never learn what any mode id means,
they pick it up as is:
- your screen appears under Settings › Screens › Add Screen, with its preview;
- people can check it for the idle rotation;
- it shows up in the board's knob menu.

Every string the apps draw travels in the descriptor. No protocol bump, no app
rebuild, no firmware release.

These rules keep that working:

- **Put every string the row needs in the descriptor** (label, subtitle, the
  input's placeholder). The moment app code reads `if id == "weather"`, forks
  need app releases again.
- **A mode declares its own settings screen.** `fields` on the descriptor:
  `text`, `toggle`, and `choice`, each with a default, and one level of
  `visible_when` so a field can depend on another. `kind` is a string, so an
  app that predates your field type skips it rather than breaking.
- **Mark a field `per_screen` when it belongs to one copy of the screen**, such
  as a layout or a style. A board can hold several copies of one screen, and
  they differ only in their per-screen fields: big weather and a forecast are
  two Weather screens with different layouts. Every other field (a location,
  24-hour time) is board-wide, typed once and shared by every copy.
- **Give each option of a per-screen choice a short `screen_label`** (15
  characters or fewer: "Forecast", "Word Clock"). It names every screen with
  that option. Without it a single copy goes by the mode's label, and copies
  that repeat fall back to "Weather · Forecast" (the knob shows a shortened
  form of that).
- **Render exactly one frame, with 16 colors or fewer.** A screen in the idle
  rotation shares the board's 24 KB frame cache with every other screen in it.
  The board keeps its place in the rotation only while the rotation's length
  holds still, and a frame with more than 16 colors ships uncompressed at about
  4 KB.
- **(Superseded) A mode may declare at most one text input** — a ZIP, a list of tickers.
  Anything richer (two fields, a picker, live validation) needs an app change.
  That ceiling is deliberate: a field-type enum would be a closed enum in the
  app's generated bindings, which is the exact thing this design avoids.

Boards never interpret your mode's id. The knob menu carries screen ids and
labels as plain strings the board echoes back. Everything else reaches the
board as frames plus a "dark or lit" flag, so an old board on an old firmware
renders your screen without knowing it exists.

## Monitoring and email alerts

`GET /health` reports whether each background loop is still running, and 503s
when one has stalled. `railway.json` points a platform healthcheck at it, so a
hung container restarts itself.

For email, set three variables on the server:

| Variable | Meaning |
|---|---|
| `RESEND_API_KEY` | An API key from [resend.com](https://resend.com) (free tier is plenty). |
| `ALERT_EMAIL_TO` | Where alerts and the daily report go. |
| `ALERT_EMAIL_FROM` | Optional sender; defaults to Resend's shared onboarding address. |
| `DAILY_REPORT_HOUR` | Optional UTC hour for the daily report (default 14). |

With nothing set, alerting is simply off — no errors, no noise.

Test it before trusting it:

```sh
railway run -s live cargo run -p server -- --test-alert
```

That reads the deployed service's variables, sends one email, and exits
non-zero with the provider's reason if it fails (an unverified `FROM` domain is
the usual one).

You get one email when a problem is confirmed (three consecutive failed checks,
so a single slow poll is not an incident), one when it clears, and one report a
day.

The watchdog probes with `try_lock` and never waits, which is deliberate: the
outage that prompted it was a deadlock, where anything that waited on a lock
joined the queue and went silent too. A lock it cannot take IS the alert.

It cannot tell you the process died — nothing left to send the mail. That case
belongs to the platform healthcheck above, and to any external uptime monitor
pointed at `/health`. The two cover different failures.

## Working in iCloud Drive

If this repo lives under `~/Documents` (synced), git can fail with:

```
fatal: unable to access '.git/config': Operation not permitted
```

while `cat .git/config` works fine, and cargo fails the same way on
`target/debug/.cargo-build-lock`.

The cause is iCloud **eviction**, not permissions. When iCloud reclaims space it
replaces a file's contents with a placeholder ("dataless"). `open()` triggers a
download and blocks until it lands — so `cat`, editors and compilers mostly just
work. `access()` does NOT: it returns `EPERM` on a dataless file rather than
fetching it, and `access(R_OK)` is exactly what git calls before reading a
config file. Files that happen to be materialized work; evicted ones don't, so
the failure looks random and intermittent.

It clears once the files are downloaded. To stop it recurring, pin the repo:
right-click the folder in Finder and choose **Keep Downloaded**. That is the
fix — no permission change is involved, and granting Full Disk Access will not
help, because nothing was ever denied.

If you are stuck mid-session, forcing a read of the affected directory
(`find .git -type f -exec cat {} + > /dev/null`) materializes it, after which
git works again. `scripts/git-icloud.sh` does something similar by copying the
git directory out and back, but on a large repo the copy itself is slow for the
same reason — pinning is better.

## Where to go next

- **[docs/cli.md](docs/cli.md)** — every server flag, route, and env var; the
  `server tui` pipeline debugger for when a card renders wrong.
- **[docs/data-sources.md](docs/data-sources.md)** — which ESPN feeds power what.
- **Hosting your own OTA updates** — `scripts/release-firmware.sh` builds, signs
  (Ed25519), and publishes to a channel your boards can pull from. Images are
  signature-verified on-device with a downgrade guard, so a bad or hostile image
  can't install; a broken update reverts itself after five minutes.

# Provisioning a board, and the TLS pin

Everything a board needs to reach a server, and what the pin actually is.

Operator reference (env vars, rotation procedure) lives in
[cli.md](cli.md#tls-on-the-board-link). Building a whole scoreboard from parts
lives in [DIY.md](../DIY.md). This page is the part neither of those explains:
what the pin *is*, and what each kind of user actually has to do.

---

## The pin in one paragraph

The pin is a **SHA-256 hash of the server certificate's public key** — 32 bytes,
written as 64 lowercase hex characters. It is not a key and **not a secret**. It
is a checksum of something the server hands to anyone who opens a socket, and
anyone can derive it from the running server in two commands
(see [Deriving a pin](#deriving-a-pin-from-a-running-server)). Publishing it
costs nothing.

Its job is to replace certificate validation the board cannot do. A normal TLS
client checks a certificate's expiry against a clock and chains it to a
certificate authority in a root store. The board has neither. So it does
something simpler and stricter instead:

> The server must present **exactly this public key**.

Not "a key signed by someone reputable" — that one. That is why the certificate
is minted with a 10-year validity and hostnames that are only advisory: nothing
in the trust decision reads dates or names.

### Why not just add a clock and use real CAs?

Asked and answered, so it doesn't get re-litigated. `embedded-tls` does ship a
CA-chain verifier, and it is deliberately switched off (see the note on the
dependency in `firmware-rs/Cargo.toml`). Three reasons an NTP client wouldn't
let us turn it on:

1. **NTP can't be trusted by the attacker it defends against.** Plain SNTP is
   unauthenticated UDP. Anyone positioned to forge a certificate is on the
   network path, so they can also answer the time query and set the clock to
   whenever their forged certificate is valid. The fix is NTS, which needs TLS
   to bootstrap.
2. **We don't own the name.** CA validation checks the hostname against the
   certificate's SANs. Boards dial a Railway TCP proxy hostname, which is
   Railway's domain, not ours — no CA will issue for it.
3. **NTP gives UTC; the board needs local time.** The board has no timezone of
   its own by design, and the server sends *already-localized* epoch millis
   (`board_local_epoch_ms`). Time would still have to come from the server.

Pinning also concentrates trust rather than spreading it — though on this board
the contrast is narrower than it first sounds. `embedded-tls`'s verifier is not a
root-store verifier either: `CertVerifier::new` takes exactly one CA certificate
and chains to it, which is pinning one level up with a clock bolted on. What
pinning actually saves is the apparatus a public certificate drags in: a
registrar, a 90-day renewal cycle, and a name that any of ~150 public roots could
issue for. For a device that talks to exactly one server you operate, pinning is
the simpler and stronger choice.

---

## Pin vs identity — two different 32-byte things

These are easy to confuse. They point in opposite directions.

|  | **Server pin** | **Board identity** |
|---|---|---|
| What | SHA-256 of the server's public key | Ed25519 secret seed |
| Secret? | No — publish it freely | **Yes** — never leaves the board |
| Unique per board? | **No** — every board on a server shares it | Yes — it *is* the board's id |
| Proves | the server, to the board | the board, to the server |
| Set when | Flash time, or BLE provisioning | Written at flash time (authorized), or minted on first boot |
| If lost | Re-derive from the certificate | The board becomes a new board (and loses its authorization) |

The board's identity is what makes "someone can't just connect and receive your
frames" true. Every connection is a challenge-response: the server sends a
random challenge, the board signs it, and its public key *is* its `board_id`.
Anyone can generate a keypair and connect — they get their own empty board.
Reaching *yours* needs your board's secret, which is only ever in its flash.

The hosted server goes one step further: it serves only boards we have
**authorized**. Their identity is written at flash time together with our
signature over its public key (a *blessing*), which the board presents with
every connection. A board without one is refused and shows NOT AUTHORIZED.
A server you run yourself doesn't check this unless you ask it to (see
[cli.md](cli.md#board-authorization)).

## Where the pin is stored

Four places, all the same 32 bytes.

| Where | Form | Notes |
|---|---|---|
| Server | Not stored — recomputed at boot from `TLS_CERT_PEM` | Cannot drift from the certificate |
| Firmware, default | A `const` compiled into the binary | From `SCOREBOARD_TLS_PIN` (prod) or `SCOREBOARD_DEV_TLS_PIN` (dev) |
| Firmware, override | 32 raw bytes in the flash credentials record | `nvs` at `0x9000`, magic `WBC3`, written over BLE |
| Your machine | `firmware-rs/local.env` | Gitignored; dev builds only |

A provisioned pin **replaces** the compiled-in ones — it is not an addition, and
it has no successor slot. Survives a USB reflash (the `nvs` partition is
deliberately preserved), cleared by a factory reset.

---

## The three flows

### A board we sell

**Us, once per board:** mint the board's authorized identity, then flash it
with the hosted-server image.

```sh
cargo run -p server --bin bless-board -- board.bin
IDENTITY=board.bin SCOREBOARD_TLS_PIN=<pin of the deployed cert> FEATURES=prod bash scripts/flash.sh
rm board.bin
```

That bakes in the hosted server's address, its pin, and the firmware signing
public key, and writes the identity the hosted server will accept. Delete the
image afterwards: whoever holds it can be that board. Then ship it. A new board
has no stored credentials, so it comes up in Bluetooth provisioning; its
`board_id` already exists, so it is known before it is ever switched on.

**The buyer:** plug in → app → pick the Wi-Fi network, type the password → the
board joins, connects over TLS, and shows a 6-digit code → type that into the
app. Done.

They never have to enter a pin: the field is hidden unless the app's server is
set to "My own server." TLS costs them nothing at all.

### BYOD — your hardware, our server

The hosted server only serves authorized boards, so a board you built needs
an authorization code from us. You create your board's identity yourself, and
its secret never leaves your machine: all we ever see is its public id.

1. **Create the identity.** One 4 KB file; the board id is printed.

   ```sh
   cargo run -p server --bin bless-board -- --new-identity board.bin
   ```

   Keep `board.bin` private: whoever has it can be your board.

2. **Send us the board id** (the 64 hex characters it printed). We send back an
   authorization code, `auth1-…`. It is not secret: it is useless without your
   `board.bin`.

3. **Attach it, then flash** with the hosted-server image and its pin (from
   [Deriving a pin](#deriving-a-pin-from-a-running-server)):

   ```sh
   cargo run -p server --bin bless-board -- --attach auth1-… board.bin
   IDENTITY=board.bin FEATURES=prod SCOREBOARD_TLS_PIN=<pin> bash scripts/flash.sh
   ```

   `--attach` refuses a code that is damaged or was made for a different
   board, so a copy-paste slip is caught before anything is flashed.

Then set it up in the app like any board. It behaves like one we sold,
including keeping its authorization through a factory reset. Keep
`board.bin` somewhere safe if you'd like to be able to restore the board after
replacing its flash; otherwise delete it.

A board pointed at the hosted server without an authorization connects, is
refused, and shows NOT AUTHORIZED.

Self-hosting below needs nothing from us and is unaffected.

### BYOS — your server

**TLS is required, not optional.** Shipped firmware always speaks it, so a
server with `TLS_CERT_PEM` unset serves plaintext and no board will ever
connect. Mint an identity once:

```sh
cargo run -p server --bin gen-tls-cert
```

It prints a certificate, a private key, and the pin. The PEMs become
`TLS_CERT_PEM` / `TLS_KEY_PEM` on your server. The private key is stored nowhere
else — losing it means minting a new identity.

**You do not need to build custom firmware.** Any board can be pointed at your
server, including one you bought from us — with one catch: a board that already
has credentials in flash never advertises `Scoreboard Setup`, so it has to be put
back into Bluetooth setup first. A new board already is, out of the box. One that
has been running needs the knob held for ten seconds (or ERASE from the menu),
which is a full factory reset — it un-pairs the board from every phone. (A board
that minted its own identity also gets a new one; a board we authorized keeps
its identity, and its authorization, through a reset.) (The single press that forgets *only* the network is
offered after a failed join, on the `PRESS TO SET UP` screen, and not otherwise.)

Then, in the app: tap **Use my own server**, pick "My own server", type your
server's address, and paste the pin into the field below it. Now open **Set up
Wi-Fi over Bluetooth** — the sheet shows the address it will send and asks for
the pin alongside the network and password. Bluetooth provisioning carries the
host, the port and the pin together, and the provisioned pin replaces the
compiled-in one.

Repointing an already-paired board from its own settings will *not* do instead:
that screen deliberately hides the pin field, because the pin already written to
that board's flash cannot be rewritten that way.

Building your own image with the server baked in also works and is what
[DIY.md](../DIY.md) walks through, but it is the harder path and only worth it
if you are changing firmware anyway.

**OTA still works.** Your server serves the `firmware-dist` images from this
repo, which are signed by our release key and match the public key baked into
your board. You would only need your own signing key if you build your own
firmware — and then you must serve your own signed images too.

---

## Deriving a pin from a running server

Any server, ours or yours, and useful whenever you need to check what a server is
actually presenting. Two commands — fetch the certificate, then hash it:

```sh
openssl s_client -connect <host>:<port> -showcerts </dev/null 2>/dev/null \
  | awk '/BEGIN CERTIFICATE/,/END CERTIFICATE/' > server-cert.pem
cargo run -p server --bin gen-tls-cert -- --pin server-cert.pem
```

`gen-tls-cert --pin` takes a file, not a host — it does no networking, which is
why the fetch is a separate step.

> **`no peer certificate available`?** The certificate is Ed25519, which
> LibreSSL — what macOS ships as `openssl` — cannot parse. Use a real OpenSSL 3
> (`brew install openssl@3`, then `/opt/homebrew/opt/openssl@3/bin/openssl`).

---

## When it goes wrong

**`evaluation panicked: SCOREBOARD_DEV_TLS_PIN is not set`** at build time. A
TLS build has no safe default pin, so this is a compile error by design. It
means you are building a *dev* image without having run `scripts/dev-server.sh`,
which mints the certificate and writes the pin into `firmware-rs/local.env` for
you. Run it once, then flash. (`flash.sh` reads `local.env` itself.)

If you meant to build the image that talks to the **hosted** server, you want
`FEATURES=prod` and `SCOREBOARD_TLS_PIN` instead — see
[BYOD](#byod--your-hardware-our-server).

**The board sits on `JOINING WI-FI`, then offers `PRESS TO SET UP`.** That is
the network, not TLS — a wrong password looks exactly like a slow join. Press
the knob to clear the stored network and come back up in Bluetooth provisioning,
keeping the board's identity and pairing.

**The board shows `NOT AUTHORIZED`.** It reached the server, and the server
only serves authorized boards (the hosted one, or yours with
`BOARD_BLESSING_PUBKEY` set). Nothing on the server can authorize it: the
authorization travels with the board's identity, so it has to be flashed one.
A board you built needs an identity from us (see
[BYOD](#byod--your-hardware-our-server)), or a server of your own.

**The board sits on `FINDING SERVER` and never connects.** A pin mismatch is a
handshake failure, so the board never reaches a connected state to drop from: the
server is presenting a different key than the board was told to expect. The
serial console shows `[tls] certificate key does not match a pinned key; retry`
on every attempt. Compare the pin the server logs at startup with the one the
board prints as `[tls] pinned to …`.

**Rotating the certificate** is a two-release procedure, not a swap — a board
whose only pin stops matching cannot be reached over the air, because the update
travels on the connection that stopped opening. The staged rollout is documented
in [cli.md](cli.md#rotating-the-key).

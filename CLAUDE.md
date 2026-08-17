# Scoreboard

A physical 64×32 RGB LED scoreboard. **Smart server, dumb client**: the Rust
server (`crates/server`) polls ESPN, renders every frame, and serves them over
websocket; the board (ESP32-S3, `firmware-rs` + `crates/board-core`) blits
what it is sent. The iOS app is views over a shared Rust core
(`crates/app-core`, via uniffi). `crates/protocol` is the wire.

## Hard rules

These are owner decisions, not preferences. Do not relitigate them silently.

- **Never show betting content.** No odds, no spreads, no moneylines — not in
  renders, not in the app, not in previews. This is absolute.
- **ESPN is polled on a timer, and only while clients are connected.** Adding
  a board or controller must NEVER trigger a fetch. The timer is the only
  trigger (`poll_loop`).
- **All animation is device-clock driven.** The board animates locally
  (spinners, countdown fills, scrolls, dwell-timed rotation). Server pushes
  are CONTENT changes. Never stream animation frames over the network.
- **The board must always be resettable** by holding its button. No change to
  input handling may break the hold-to-reset path.
- **No stale precision.** Board fields must read correctly from a poll up to
  ~3 minutes old. This is why the live game clock was removed — do not
  reintroduce per-second fields.
- **Never run `cargo fmt`.** The repo is deliberately not rustfmt-clean;
  formatting churn buries real diffs.
- **Deploys ship a clean snapshot of HEAD**, never the working tree:
  `git archive HEAD` into a temp dir, then
  `railway up "$SNAP" --path-as-root --service live --ci`. Verify with an
  explicit grep for "Deploy complete" — piped exit codes lie.
- **uniffi closed enums are a compatibility cliff.** An unknown enum case
  aborts the entire config decode in the fielded app. Any value a server can
  extend travels as a STRING plus a self-describing descriptor (see
  `off_mode`, `mode`). Same on the wire: `BoardConfig.power` has no serde
  default and must keep being serialized forever (derived from `mode`).
- **`ios/build-core.sh` is the only way to rebuild AppCore.xcframework.**
  Hand-rolled cargo/xcodebuild invocations produce signature and
  deployment-target breakage.
- **`swiftc -parse` is not a check.** It accepts nonexistent fields. The only
  acceptable Swift verification is a full `xcodebuild ... build`.
- **The board carries the information.** New game data goes on the 64×32
  frames, not into expanded app-card UI.
- **Firmware releases** go through `scripts/release-firmware.sh <channel>`
  (version bump in `firmware-rs/Cargo.toml`, notes in
  `firmware-dist/notes/<version>.txt`, `SCOREBOARD_TLS_PIN` re-derived from
  the deployed cert). Versions are one monotonic space across all channels
  and are never reused.
- Don't start the demo server, the simulator, or the app unless asked.
- **This repo is in `~/Documents`, which iCloud syncs** (intentionally, for the
  source). iCloud duplicates files as `"foo 2.ext"` under contention, which
  breaks builds in ways that read as code errors ("defined multiple times",
  "Failed file name validation"). Build trees therefore live OUTSIDE the synced
  tree: Gradle in `~/Library/Caches/scoreboard-android`, firmware in
  `firmware-rs/target.nosync`. A filename with a space and a number in a build
  error is this, not the code.

## Cross-platform parity (Android)

`android/` exists (Kotlin/Compose over the same core — see android/README.md
for status and build). These bind:

- **Every user-facing string lives in `crates/app-core` (`strings.rs`) or
  comes from the server's descriptors.** Neither platform hardcodes copy that
  the other platform also needs. New copy goes into the core first.
- **Logic lives in the core, not the platform layer.** A rule implemented in
  Swift or Kotlin alone is a drift bug waiting to happen; port it down before
  building on it.
- **A fix to app behavior or copy lands on BOTH platforms in the same
  change**, or the commit message states explicitly why it is one-sided.
- **Parity means strings, logic, and behavior — NOT layout or navigation
  idiom.** The Android app is Android-first, not a port of ContentView.swift.
- Platform-specific by nature (and staying that way): rendering shapes,
  navigation, BLE/Keychain equivalents, locale/timezone FORMATTING (the core
  supplies strings and data; the platform formats times — 24-hour locales
  broke once already).

## Layout

| Path | What |
|---|---|
| `crates/server` | axum server: ESPN polling, all rendering, ws, OTA dist |
| `crates/protocol` | wire types (shared by everything) |
| `crates/app-core` | app logic + strings, uniffi → Swift (and later Kotlin) |
| `crates/board-core` | board logic (menus, gestures, rotation), host-testable |
| `firmware-rs` | ESP32-S3 firmware embedding board-core (own workspace) |
| `ios/ScoreboardApp` | SwiftUI app; `ContentView.swift` is the whole UI |
| `android` | Kotlin/Compose app over the same core (`android/build-core.sh`) |
| `firmware-dist` | signed OTA dists per channel + notes + release ledger |

## Checks

```bash
cargo test --workspace          # server, protocol, app-core, board-core
./ios/build-core.sh             # after any app-core/protocol change
xcodebuild -project ios/ScoreboardApp/ScoreboardApp.xcodeproj \
  -scheme ScoreboardApp -sdk iphoneos -configuration Debug \
  ARCHS=arm64 CODE_SIGNING_ALLOWED=NO build
./android/build-core.sh         # after any app-core/protocol change (needs NDK)
(cd android && ./gradlew assembleDebug)   # needs the Android SDK
```

The parity reminder hook: `cp scripts/git-hooks/pre-commit .git/hooks/`.

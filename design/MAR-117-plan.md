# MAR-117 — Rework extra screens: implementation plan

Status: **owner decisions made 2026-10-05 (§0); ready to implement.** Nothing is implemented yet. Written 2026-10-05 from a full map of the
server, protocol, board-core/firmware, app-core, iOS and Android code, then stress-tested by five independent
reviewers (wire compatibility, board runtime, server behaviour, app parity, product fit) and a frame-size measurement.

**Ticket:** replace "one entry per screen type + pick ONE idle screen" with a per-board **list of screens** (defaults
Sports, Off, Weather, Clock; users can add more, e.g. *big weather* **and** *forecast*), and an **idle rotation**: a
subset of the list that the board cycles through on a long, configurable timer. Sports and Off cannot be removed;
everything else can be removed and reordered.

---

## 0. Decisions

### Owner decisions (2026-10-05)

| # | Question | Decision |
|---|---|---|
| Q1 | Do two Weather screens share one location, and is 24-hour time one board setting? | **Shared.** Location and 24-hour stay board-wide; each screen owns only its layout or style. You type your ZIP once. Two different cities on one board is not possible. |
| Q2 | Where do Sports and Off sit, and what is the first row called? | **Sports first, Off second, everywhere** (settings list, app rail, knob). The label "Scores" becomes **"Sports"** (knob: SPORTS); the wire id stays `scores` forever. This deliberately changes the knob on every beta board **at server deploy**: SCORES→SPORTS, and OFF moves from last to second. The firmware's compiled-in offline fallback still says SCORES until it rides along with some future firmware release. |
| Q3 | Can Off be a step inside the idle rotation? | **No.** The rotation is made of lit screens. With nothing checked, the board stays dark when idle (today's "Off" idle choice). |
| Q4 | How does Android reorder screens? | **Drag via `sh.calvin.reorderable`,** plus Move up / Move down accessibility actions for TalkBack. iOS uses native drag. |

### Decided by this plan (engineering calls, so override any you dislike)

- **D1. A screen is an instance:** `{id, kind, params, idle}`. `kind` is a server descriptor id, or the reserved
  `scores`/`off`. Two Weather instances with different layouts are *big weather* + *forecast* (*big weather* is the
  existing **Simple** layout). This partly reverses c6383e4 ("weather layouts are choices, not four top-level
  modes") and 98dd59a (word clock folded into Clock). The *default* list stays one-per-kind, so that intent survives;
  duplicates are opt-in. The commit message will say this.
- **D2. One user order** drives the Screens settings list, the app's Controls rail, the board's knob menu **and** the
  rotation order.
- **D3. Rotation timer:** one per board. Presets are 2, 5, 10, 15, 30 and 60 min, default 5. It is stored as
  `idle_cycle_secs`, with the presets and snap function in `protocol`, following the `DWELL_*` precedent. It is shown
  only when 2 or more screens are checked.
- **D4. Labels are derived, never typed.** A screen uses its descriptor label while its kind appears once (so nothing
  is renamed at deploy). When a kind repeats, it uses a short **server-declared** label for its layout or style (new
  optional `OffModeChoice.screen_label`, ≤10 chars). Examples: Simple → "Big weather", Forecast → "Forecast", Word →
  "Word clock". Any remaining tie gets " 2", " 3". One protocol function serves app labels, knob labels (≤15 ASCII,
  uppercased) and the idle notice.
- **D5. Picking a screen** (rail or knob) holds that instance for its own hold timer, then returns to Sports, as
  today. The rotation itself is not a pickable mode.
- **D6. The ring while rotating** (Sports mode, nothing on, 2 or more checked) marks **every** checked screen. One of
  them is on the glass, and the board never reports which. 1 checked rings that screen, as today. 0 checked or quiet
  hours ring Off, as today.
- **D7. Old apps keep working,** and a household with one old phone and one new phone can't corrupt the list (§2.3).
- **D8. New app + old self-hosted server:** the app degrades to today's single-select behaviour automatically (§4).

### Invariant: nothing changes at deploy

Some boards have never saved a screens list. That includes every board until someone edits screens in the new app.
For those boards the server emits **byte-identical** manifests (rotation, dwell, epoch, knob `modes` ids/labels/order),
an identical `Catalog.off_modes`, and identical legacy `BoardState` fields. A snapshot test pins this. The only
exception is the one Q2 chose on purpose: the knob's first row reads SPORTS, and OFF is second. The snapshot test
asserts exactly that difference and nothing else.

---

## 1. Frame encoder fix (prerequisite, protocol, ships first)

**Problem (measured):** `encode_frame_payload` skips any PAL_RLE result whose length is a multiple of 128 B. The
skip exists so a board can tell a legacy raw payload apart by its length (`protocol/src/lib.rs` ~1729). PAL_RLE
lengths are always even, so the skip fires on about 1 in 64 lengths, not the ~1/128 its comment claims. The frame
then ships **raw at 4,097 B**. Measured rates by frame type:
- compact 24h clock: 15% of minutes, including the 10:09 preview;
- large 24h clock: 7%;
- weather: 1–6% depending on conditions.

The board must keep every rotation frame in a 24 KiB cache. Above budget, LRU eviction always evicts the *next*
screen, and nothing re-requests it until a different manifest arrives. With today's encoder only **3** idle screens
are safe.

**Fix:** when a candidate's length is a multiple of 128, split one run of length ≥2 into two pairs with the same
index, adding 2 B.
- The output decodes identically, and every fielded decoder accepts it: stable `b299019` `decode_pal_rle` checks only
  `len ≥ 1 && index < count`.
- An accepted candidate always contains a run of length ≥2; one without would be bigger than raw.

**After the fix:** the worst stock idle frame is the word clock at 2,130 B (constant). The other maxima are:

| Frame | Max size |
|---|---|
| Classic clock | 242 B |
| Large clock | 402 B |
| Compact clock | 198 B |
| Simple weather | 314 B |
| Detailed weather | 760 B |
| Forecast | 1,082 B |
| Clock + Weather | 716 B |
| Notices | ≤778 B |

**Tests:**
- Sweep all 1,440 minutes × 12/24h × styles and the weather condition matrix through the encoder. Assert that no
  stock frame ships raw and that each one round-trips through `decode_frame_payload`.
- Fix the "~1/128" doc comment.

## 2. Protocol and server

### 2.1 Wire model (`crates/protocol`: no_std + alloc; strings only; every field serde-default and skip-if-empty)

```rust
pub struct ScreenConfig {
    pub id: String,     // "scores" | "off" | kind id for the first instance | "{kind}.{n}" after
    pub kind: String,   // "scores" | "off" | off-mode descriptor id — never an enum
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub params: BTreeMap<String, String>, // per-screen keys only
    #[serde(default, skip_serializing_if = "is_false")] pub idle: bool,
}
// BoardConfig +=
#[serde(default, skip_serializing_if = "Vec::is_empty")]   pub screens: Vec<ScreenConfig>,      // empty = never saved
#[serde(default, skip_serializing_if = "Option::is_none")] pub idle_cycle_secs: Option<u32>,
#[serde(default, skip_serializing_if = "Option::is_none")] pub mode_screen: Option<String>,    // held instance when ≠ kind
// OffModeField +=   #[serde(default, skip_serializing_if = "is_false")] pub per_screen: bool   // layout/style = true
// OffModeChoice +=  #[serde(default, skip_serializing_if = "Option::is_none")] pub screen_label: Option<String>
```

**Layered params (Q1 = shared).** A screen renders with `off_mode_params` (board-wide: location, hour24), overlaid by
`screen.params`, which holds only the fields its descriptor marks `per_screen` (stock: weather `variant`, clock
`style`). `off_mode_params` remains **real storage**, not a projection.

**Ids.**
- **Minting:** the kind id if no screen has it, else `{kind}.{max+1}`. The `.` separator is deliberate: a `weather-x`
  id would be rewritten by `offmode::migrate`. Never mint `scores`, `off`, `blank` or empty. Ids use the charset
  `[a-z0-9._-]` and are ≤32 chars. An id equal to a catalog kind id must have that kind.
- **Resolution** for any legacy or picked id: exact id first, then `migrate()`. A `weather-<v>` id prefers the
  instance whose variant is `<v>`; otherwise it takes the first instance of that kind in list order. No other code may
  assume `id == kind`.

**`mode` twin (keeps old apps and a rolled-back server sane).**
- `mode` keeps its old vocabulary: `scores`, `off`, or a kind id.
- `mode_screen` holds the instance id only when it differs from the kind.
- `held_screen_id()` returns `mode_screen` or else `mode`.
- `set_mode()` always clears `mode_screen`. New: `set_held_screen(id, kind)`.
- The power twin is unchanged. A held `weather.2` therefore looks like "Weather" to an old app (ring on the Weather
  tile, footer "Weather …"). Manifest.mode still carries the **instance** id, so the knob tick is right.

**Shared functions** (protocol, used by both server and app-core, each mirror-tested):
- `resolved_screens(&cfg, catalog)`: returns the stored list, or synthesizes one from the legacy fields. It builds the
  pinned entries plus one instance per lit descriptor in catalog order, with per-screen keys copied from the flat map.
  idle = `{off_mode kind}` if lit, none if `blank`, and the clock if `off_mode` is unknown, because dark is never the
  fallback.
- `normalize_screens(list, catalog)`: pins Sports first and Off second (§0 Q2), dedupes ids, validates ids and per-screen params
  against the descriptor, and drops kinds that are unoffered, `blank`, or reserved. If that drop empties an idle set
  that was not empty, it makes the clock idle. It also clears `idle` on Sports/Off and enforces `MAX_SCREENS = 10`
  (Sports + Off + 8).
- `set_screens(list)` re-derives every legacy twin on every write:
  - `off_mode`/`off_state` from the first idle member's kind, else `blank`;
  - per-screen keys of the first instance of each kind are mirrored into `off_mode_params` (first idle member first,
    then list order), so old apps' previews and pages show the right layout.
- `screen_label`, `board_label` (≤15 ASCII, uppercased, unique) and `notice_label` (≤10).
- `IDLE_CYCLE_SECS`, `snap_idle_cycle_secs` (never 0).
- `mode_duration_hours(id)` looks up `mode_hours[id]`, then `mode_hours[kind]`, then `sleep_hours`.
- `prefill_params(kind, preset, existing)`.

**PortableSettings** gains `screens` and `idle_cycle_secs`, with **no version bump** (the import check is
exact-equality). It still carries the legacy twins and never carries `mode`/`mode_screen`. The new names avoid the
substrings "override", "demo" and "channel", which existing tests assert against.

**Commands** (open strings and numbers only; every old variant is kept):
- `SetScreens { screens }`: one whole-list command for add, remove, reorder, idle toggles and layout changes. It
  follows the `SetSportConfig` precedent and runs the same `normalize_screens` as load and import. Last write wins,
  which is fine for a household board.
- `SetIdleCycle { secs }`.
- Board-wide fields (location, 24h) keep using `SetOffModeParam`. Its validation is **widened** to any offered
  descriptor that declares the key. That fixes a live bug: today, editing Weather's location while Clock is the idle
  screen is silently rejected server-side (`ws.rs:789`), and the app shows it as set until the next push.

No Manifest, ModeEntry or RotationEntry change. No new enum anywhere. No firmware change.

### 2.2 Server

- **One render seam.** Every render goes through `render_screen(&ScreenConfig, …)`, which passes `screen.kind` plus
  layered params. An instance id is never passed to `offmode::*`. `render_off_mode` silently draws a **clock** for an
  unknown id such as `weather.2`, so this bug would otherwise ship looking fine. A test asserts that each instance's
  frame equals `render_off_mode(kind, its params)` and is not the clock.
- **Idle rotation.**
  - **0 or 1 checked screens:** exactly today's manifest (epoch 0, the render's own dwell). The deploy invariant
    rests on this.
  - **2 or more:** one RotationEntry per checked screen (first frame only, if a fork renders several), dwell =
    snapped timer, `scroll: false`, `local_clock: false`, `off_state` derived.
  - **Epoch:** first 8 bytes of `sha256("idle|" + ids + "|" + dwell)`, OR'd with `1<<63`.
    - The same set gives the same epoch across minute re-pushes and deploys, so the board keeps its slot and timer
      (board-core `same_shape`).
    - Changing membership, order or the timer, or arriving from games, a notice or a held screen, starts at screen 1.
    - Param edits don't restart the rotation.
    - The epoch never collides with games (0) or notices (a counter from 1).
    - A test pins one literal value.
- **Knob menu.** `pickable_modes(&cfg)` is built per board from the resolved list, in order, with `board_label`s:
  SPORTS, OFF, then the user's screens. `scores` and `off` are always present and ids are unique.
  `Manifest.mode = held_screen_id()`. `the_picker_offers_what_select_mode_accepts` (main.rs:5700) is rewritten for
  the per-board list, Sports first and Off second.
- **Picks.** `SelectMode` and `SetMode` resolve against the board's list. An unknown or stale id (e.g. a queued
  offline knob pick of a since-deleted screen) is ignored with a warning, not guessed. A legacy kind id resolves to
  the first instance. If the kind has no instance (the user removed it), the pick is ignored. `blank` maps to `off`.
  An immediate weather fetch happens only if the picked screen's descriptor declares a location field. The
  `starts_with("weather")` sniffing is replaced by that check at every site.
- **Non-gesture release.** `release_to_scores()` does `set_mode(scores)` and clears `wake_at`, and **never** sets
  the quiet override. It is used by:
  - removing the held screen;
  - an import that drops it;
  - the load sanitize.

  `apply_mode()` sets the override as it does today, so deleting a screen at 1 am stays dark. The handoff notice
  still runs.
- **Invariant after every list write or import:** `held_screen_id()` resolves to `scores`, `off`, or a listed
  instance; otherwise `release_to_scores()`. At load, a stored mode `blank` maps to `off` (dark stays dark).
- **Timers:** `mode_hours` keyed by instance id, falling back to the kind, then `sleep_hours`. Removing a screen
  removes its key. `SetModeHours` validates the key.
- **Idle handoff notice:** "NO GAMES / WEATHER IN / 2 MINUTES" names the **kind** of the first checked screen, which
  always fits the 10-char budget. "SCREEN OFF IN" when none are checked. Unchanged for 0–1 members.
- **Weather:** a board needs its (board-wide) location fetched if any checked screen or the held screen declares a
  location field. `weather_dirty` fires when that set gains a location: SetScreens, SetMode/SelectMode, legacy
  SetOffMode, a location edit, or import. Spawned via `Handle::try_current()`. Nothing fetches on connect, as today.
- **`board_state_msg`:** puts the *resolved* list into the config clone sent to apps; this never reaches disk. A
  non-empty list is the app's capability signal.
- **Load:** keep `sanitize_off_modes`' legacy flat pass (test 9203), then sanitize per screen. This happens in memory
  only, as today.
- **Import:**
  - `apply_to` copies `screens` verbatim. An empty list leaves the board unsaved, so the imported legacy fields drive
    it.
  - Sanitize only a present list.
  - Re-check the held screen.
  - Trigger a weather refresh.
  - The existing round-trip test must keep passing.

### 2.3 Old apps against the new server

- **Board never saved a list:** legacy commands (SetOffMode, SetOffState, SetOffModeParam, SetMode, SetModeHours)
  run **today's exact code path** and never create a list. An old-app-only household sees no change at all.
- **Board with a saved list:**

| Old-app command | New-server behaviour |
|---|---|
| `SetOffMode{kind}` | If the first checked screen is already that kind: no-op. Both shipped apps resend on every tap, so this stops a re-tap from wiping a rotation set up on the new app. Otherwise the checked set becomes [the first idle instance of that kind, else the first instance]. A removed kind is ignored. `blank` means none checked. |
| `SetOffModeParam` | Board-wide key: goes to the flat map. Per-screen key: goes to the first checked screen that declares it, else the first instance that declares it. |
| `SetMode{kind}` | Resolves as in §2.2. |

### 2.4 Rollback

An older server parses new rows (unknown fields are ignored). On a board's first change after a rollback, it
rewrites the row without `screens`/`idle_cycle_secs`/`mode_screen`. The twins keep that graceful:
- the first checked screen as the single idle screen;
- one layout per kind;
- the real location;
- `mode` as kind.

Rolling forward re-synthesizes from them. **Accepted as lossy for duplicates, order and multi-screen rotation**; no
second storage location. A test simulates the strip-and-resynthesize.

## 3. Board / firmware

No firmware change, verified against stable 2026.8.1 (`b299019`), beta 2026.9.0 (`f47d886`) and HEAD:
- same-length + same-epoch re-pushes keep slot and timer;
- `epoch` is u64 everywhere and is only compared for equality;
- `dwell_ms` is u32 ms;
- knob ids are opaque apart from the literals `scores`/`off`, and the cursor finds Off **by id**, so moving it
  doesn't break tap-tap;
- 15 FONT_4X6 chars fit before the tick at x=61;
- hold-to-reset reads none of this;
- stable boards have no picker (a tap toggles off/scores), but a rotation reaches them as an ordinary rotation.

The firmware's offline fallback menu (board-core lib.rs:1101, used only with no manifest) keeps reading
SCORES/OFF. Changing it to SPORTS/OFF is a one-line board-core change, but it ships only with the next firmware
release, so it's left for whichever release comes next rather than cutting one for it.

Tests:
- one compact board-core regression test (3 entries at 300 s, an off-screen id changing every 60 s, advance at
  exactly 300 s, new epoch → slot 0);
- one **server end-to-end** test driving a real `board_core::Board` (the pattern at `main.rs:5320`). It uses the
  worst-case stock mix at the cap, with minute re-pushes over two full cycles, and asserts no slot ever shows the
  previous screen.

## 4. app-core (logic + strings; uniffi Records only)

- **Wire mirror.** `UiBoardConfig` gains:
  - `screens: Vec<UiScreenConfig{id, kind, params, idle}>`, verbatim; empty means an old server;
  - `screens_supported: bool`;
  - `idle_cycle_secs: u32` (resolved).

  `UiBoardConfig.mode` becomes the resolved held id. `UiOffModeField.per_screen` and `UiOffModeChoice.screen_label`
  are mapped through.
- **Derived view.** `screen_list(screens, off_mode, off_mode_params, off_modes) -> Vec<UiScreen{id, kind, label,
  preview, idle, fixed, removable, has_settings}>`. It takes only fields iOS's `BoardInfo` holds; no rule takes a
  whole `UiBoardConfig` (the `league_prefs` drift lesson). It falls back to `off_modes_or_fallback()` before the
  catalog arrives, which closes Android's existing cold-start gap.
  - **Preview:** `configured_preview(descriptor.preview, declared params)`, never from a choice path. That avoids
    duplicate `?variant=` keys and keeps the user's ZIP out of preview URLs (previews render sample weather anyway).
- **Optimistic mirrors** (each returns a new list; mirror-tested against the protocol functions):
  - `screens_added(...) -> {screens, id}`;
  - `screens_removed(...) -> {screens, mode, wake_at, mode_hours}` (mirrors the held-screen release);
  - `screens_with_order(ids)` (id-based, not index-based; Swift and Kotlin indices differ);
  - `screens_with_idle(id, on)`;
  - `screen_params_after_set` (an empty value removes the key, as the server does).

  Predicates: `can_add_screen`, `can_set_idle`, `idle_cycle_applies`. The mirrors refuse exactly what the server
  refuses.
- **Other rules:**
  - `addable_screens(off_modes)`: one preset per previewed layout/style, grouped by kind, hiding presets already
    present;
  - `ring(...) -> UiRing{ids, scroll_target}` (D6, with a single MAR-110 scroll target);
  - `mode_power(mode)`;
  - footer sentence functions;
  - `idle_cycle_options/selected/label/title`;
  - `hold_timer_options/hours/label/title`.

  The last two move the duplicated Swift/Kotlin `[0,1,2,4,8,12,24,48]`, its copy, and the `blank`→`off` id branch
  into the core.
- **Capability routing.** `client.set_screens(board, list)` sends `SetScreens` when the board's server supports it.
  Otherwise it sends today's commands (`SetOffMode{first checked kind | blank}`, then `SetOffModeParam` for that
  screen's layout), ordered by the single writer. When `screens_supported` is false, the platforms hide Add, Remove,
  reorder and the timer, and the checkmarks act as single-select. A test covers an old-server BoardState → legacy
  commands.
- **strings.rs:**
  - the Screens explainer;
  - "Add Screen" / "Remove Screen";
  - "Show when nothing is on";
  - "Switch screens every" (rotation);
  - "When picked, show for … then back to Sports" (hold timer), so three durations aren't all called "timer"
    (Screen speed stays the game pace);
  - the idle explainer for 0 / 1 / 2+ checked ("When nothing you follow is playing, the board switches between the
    checked screens every 5 minutes." / "…shows Forecast." / "…stays dark.");
  - the cap-reached note;
  - the "update your server to edit screens" note;
  - the footer sentences + "Keep" (today duplicated per platform);
  - the export summary (today duplicated on both platforms);
  - the "Sports" label: one core function used by the rail, settings, footers and `mode_label`, replacing "Scores"
    at rules.rs:841/928 and "back to scores" in both footers;
  - the troubleshooting guide's "pick Scores", which becomes "pick Sports" (strings.rs:49-50).
- **Bindings:** rebuild only via `./ios/build-core.sh` and `./android/build-core.sh`.

## 5. iOS (`ContentView.swift`)

- **Settings › "Screens"** (renamed from "Screen"):
  - **Pinned section:** Sports and Off, no move/delete. Off's page has only the hold timer; Sports has no page.
  - **Screens section:** `ForEach` with native `.onMove` (one `SetScreens` per drop via `screens_with_order`; no
    in-drag buffering is needed or possible) and `.onDelete`. Each row has an LED preview, the label, a checkmark
    ("show when nothing is on") and a NavigationLink to the per-screen page.
  - **"Add Screen"** capsule → a sheet of presets with previews and descriptor descriptions, anchored on the
    NavigationStack (the 5248 rule).
  - **"When nothing is on":** the rotation picker (2 or more checked) and the explainer.
  - Quiet hours unchanged.
- **Per-screen page:** looked up **by id** live and dismissed if the screen disappears. It renders descriptor fields:
  per-screen fields write via `SetScreens`, board-wide fields via `SetOffModeParam`. Then the hold timer, the
  checkmark toggle, and Remove.
- **Dashboard rail:** built from `screen_list`, keyed by instance id, ring and scroll target from `ring()`.
- **Remove the Swift id branches:** `timerKey`, `power = mode != "off"`, `!= "scores"`, and the default
  `offMode = "clock"`. Acceptance grep: no `"scores"`/`"off"`/`"blank"` literals left.
- AppNotes entry that explicitly supersedes "Weather is one entry with four layouts".

## 6. Android

Same page in `Settings.kt`:
- pinned rows;
- a screens list keyed by instance id;
- drag-to-reorder via **`sh.calvin.reorderable`** (Q4):
  - add it to `gradle/libs.versions.toml` and `app/build.gradle.kts`, picking a version verified against compose BOM
    2025.05.00;
  - drags run on a local `mutableStateListOf(ids)` buffer that pushes can't overwrite, and commit **once** on drag
    stop as one `SetScreens` via `screens_with_order`;
  - the pinned Sports/Off items are not reorderable;
  - Move up / Move down are exposed as accessibility custom actions for TalkBack;
  - Remove sits in the row's overflow menu;
- an Add sheet;
- the rotation `ChoiceRow`;
- the per-screen page by id, with a `BackHandler` inside ScreenPage (today system Back skips the page).

Elsewhere:
- `Dashboard.kt` rail from `screen_list`, with its remember-keys including the config list.
- `Session` intents copy-on-write. Fix `keepMode`'s in-place mutation, which makes StateFlow drop the update.
- Remove the Kotlin twins of the id branches (`Session.kt:115`, `Dashboard.kt:203`, `Settings.kt:303-306`).
- Update the `android/README.md` status line.

## 7. Sequencing and verification

1. **Encoder fix** (protocol): one small commit, `cargo test --workspace`.
2. **Protocol + server:** one commit, including the deploy-no-change snapshot test, the first-ever tests for
   SetOffMode/SetOffModeParam/SetOffState/weather_locations, the rollback simulation, the label uniqueness/width
   test, and the e2e cache test.
   - `cargo test --workspace`.
   - Then, **with your OK**, deploy: clean `git archive HEAD` snapshot → `railway up … --ci`, then grep "Deploy
     complete".
   - The only visible change is the knob menu on beta boards: SPORTS replaces SCORES, and OFF moves to second.
3. **app-core + iOS + Android:** one commit.
   1. `cargo test --workspace`.
   2. `./ios/build-core.sh`.
   3. `ANDROID_NDK_HOME=… ./android/build-core.sh`.
   4. The full `xcodebuild … build`.
   5. `JAVA_HOME=… ./gradlew assembleDebug`.
   6. Confirm all five binding files changed and there are no `* 2.*` iCloud duplicates.
   7. Run the id-literal greps.
4. **Docs:** the DIY.md fork seam (a screen in a rotation renders one frame with ≤16 colors; `per_screen` and
   `screen_label`; fix the stale "boards never see your mode's id"), README, and `android/README.md`.
5. **Device pass, by you** (or tell me to use the simulator/emulator): reorder, remove a screen while its page is
   open, Android system Back, a location edit then immediate Back, and watch a 2-screen rotation on a beta board.

## 8. Explicitly out of scope (proposed follow-up tickets)

- **Weather loop hygiene:**
  - it fetches for every stored board whether or not anything is connected, unlike ESPN;
  - untrimmed cache keys;
  - no request timeouts;
  - no cache eviction;
  - ZIP→coords re-resolved every fetch.
- **Text fields:** iOS sends a weather fetch per keystroke while typing a location; Android's 600 ms debounce drops
  the edit if you leave the page inside the window. Fix both with local state, debounce, and flush-on-leave.
- **Existing Android parity gaps:** favorite-team reorder/delete; quiet hours limited to whole hours.

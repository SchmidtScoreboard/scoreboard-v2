# F1 feed captures — 2026 Spanish GP qualifying (Madring), 2026-09-12

Raw payloads from both F1 sources, taken while qualifying ran (14:00–15:00Z),
for reference when the parsers next need checking against reality. Everything
here is verbatim from the APIs; nothing is edited.

Event 600057443, qualifying competition 401839106. 22-car field.

## What is here

| Path | What |
|---|---|
| `espn-scoreboard-<ts>.json` | `site.api.espn.com/.../racing/f1/scoreboard` — the poll's primary resource |
| `espn-core-competition-qual-<ts>.json` | core API competition resource (mostly `$ref`s; `session: 1`) |
| `espn-core-status-qual-<ts>.json` | core API competition status (`period` = the leader's laps, NOT the segment) |
| `espn-core-competitors-<ts>.json` | competitor list with vehicle number / manufacturer / teamColor |
| `espn-core-stats-p1-verstappen-<ts>.json`, `...-p16-ocon-<ts>.json` | per-driver statistics resource (the one the server fans out to) |
| `live/<ts>/` | one snapshot a minute from 14:32Z: `scoreboard.json`, `status.json`, `stats-<cid>.json` + `cstatus-<cid>.json` for all 22 drivers |
| `openf1-<name>-<ts>.json` | OpenF1 `sessions`, `meetings`, `drivers`, `position`, `laps`, `intervals`, `stints`, `race_control`, `session_result`, `starting_grid` at a few moments (14:37Z mid-Q2, 14:40Z end of Q2, 14:56Z Q3, 14:58Z) |

Render any ESPN snapshot to PNGs with the ignored fixture test (paths absolute):

```bash
F1_FIXTURE_DIR=$PWD/design/f1-logs/live/20260912T144011Z F1_FIXTURE_OUT=/tmp/f1 \
  cargo test -p server render_f1_fixture -- --ignored --nocapture
```

and an OpenF1 one-shot with:

```bash
OPENF1_FIXTURE_DIR=$PWD/design/f1-logs OPENF1_FIXTURE_TS=20260912T143721Z F1_FIXTURE_OUT=/tmp/f1 \
  cargo test -p server render_openf1_fixture -- --ignored --nocapture
```

## What the data showed (ESPN)

- The scoreboard's inline `statistics` is `[]` for every driver in every
  session. Times and gaps only exist in the per-driver core statistics
  resource. Its `general` category has `place`, `lapsCompleted`,
  `qual1TimeMS`/`qual2TimeMS`/`qual3TimeMS`; its `gapToLeader` category has
  `position`, `gapToLeader`, `lastUpdated`.
- `position` (timing-screen order) LEADS the scoreboard `order`/`place` by
  about a minute (14:35Z: `position` had HAM 3rd on +0.153; `order` still had
  RUS 3rd on +0.293 until 14:36Z). `gapToLeader` is relative to `position`'s
  order, not `order`'s.
- `gapToLeader` is the CURRENT segment's gap for a driver who has set a time in
  it, and LAST segment's gap otherwise — ESPN sorts the latter to the bottom of
  the segment's field (Q2 at 14:35Z: P15 COL "+0.752", P16 GAS "+1.035", both
  Q1 numbers, under P14 ALB "+2.975"). Drivers eliminated in Q1 keep their Q1
  gap all session.
- `qualNTimeMS` is filled for a driver only for the segment they were
  ELIMINATED in, and only some minutes after the segment ends: Q1 ended 14:18Z,
  the Q1 times for P17–P20 were present by 14:31Z (first capture); Q2 ended
  14:40Z, the six Q2 times appeared between 14:44Z and 14:45Z (Q3 started
  14:47Z). P21/P22 (no lap set) carry "0.000" throughout. After the flag
  (~15:00Z) the status became `STATUS_SESSION_COMPLETE` / "End of Session" at
  15:04Z and the ten Q3 times appeared at 15:06Z.
- Status `period` on both the scoreboard and the core status is the leader's
  completed laps (9 → 11 → 12 across Q2); it says nothing about the segment.
  Per-competitor status `period` is that driver's laps.
- The scoreboard's `status.type.detail` is just "In Progress" for the whole hour.

## What the data showed (OpenF1)

- Unauthenticated access to EVERYTHING (including past sessions) is refused
  while a live session runs. Authenticated: 6 requests/second rate limit
  (429 with `"Rate limit exceeded. Max 6 requests/second."`).
- `race_control` category `SessionStatus` delimits the segments exactly:
  `SESSION STARTED` 14:00:00 / `SESSION FINISHED` 14:18:00 / `SESSION STARTED`
  14:25:00 / `SESSION FINISHED` 14:40:00 / `SESSION STARTED` 14:47:00 — the
  regulation 18 + 7 + 15 + 8 + 12 minutes to the second, no red flags this day.
- Deleted lap times arrive as category `Other`: `CAR 41 (LIN) TIME 1:35.149
  DELETED - TRACK LIMITS AT TURN 17 LAP 3 16:04:00`. Track-limit deletions of
  non-flying laps read `LAP DELETED` with no time.
- `laps` times EVERY lap: out-laps (`is_pit_out_lap: true`, 450–500 s), in-laps
  (NOT flagged; recognisable because the driver's next lap is a pit-out; e.g.
  COL lap 8 at 14:27:45Z, 113.19 s, then lap 9 pit-out), and cool-down laps.
  A driver whose only Q2 laps were out + in has no time on the timing screen
  (ESPN agrees: their `gapToLeader` was still the Q1 figure), so those laps
  must not count as a best.
- `intervals` is empty for qualifying (race-only). `session_result` is
  "No results found." until some time after the chequered flag — still
  absent at 15:09Z, ten minutes after Q3's `SESSION FINISHED` (15:00:00Z).
- `position` during qualifying is the timing-screen order and matches ESPN's
  `position` to the driver.

## Field size and cuts

22 cars in 2026 (Cadillac joined). Q1 drops six (P17–P22), Q2 drops six
(P11–P16), Q3 is ten. `QualiZones::for_field` derives this as "half the
surplus over ten per segment", which also gives the 2025 five-and-five for 20.

## Race distances (lap counts) — `openf1-lap-counts-*.json`

Neither feed carries a race's scheduled lap count, so `crates/server/src/laps.rs`
keeps one per circuit. `openf1-lap-counts.py` pulled every 2023–2026 race and
sprint session from OpenF1 and recorded the highest lap number in its laps
feed (`openf1-lap-counts-2023-2026.json`, taken 2026-09-15). What it showed:

- 2024 and 2025 match the scheduled distances exactly, race and sprint, at
  every circuit. 2023 runs one high at most circuits (an extra lap record
  after the flag in early OpenF1 data) — ignore it.
- Distances are stable year to year; they only move with the circuit
  (Barcelona 66 → Madring 57 when the Spanish GP moved in 2026).
- Every sprint is the fewest laps over 100 km of the circuit length
  (Shanghai/Miami/Austin/Lusail 19, Spa 15, Baku/Silverstone 17, Montreal 23,
  Zandvoort/Interlagos 24), which is how Singapore's first sprint (Oct 2026)
  is entered as 21 before it has run.
- Shortened races show up as short counts (2024 Interlagos 69, 2025 Austria
  70, 2026 Montreal 68); the table keeps the scheduled distance.
- OpenF1 names Sepang "Kuala Lumpur" (`circuit_key` 12).

#!/usr/bin/env python3
"""Capture both F1 feeds, and what the live server renders, once a minute.

Each snapshot is a directory named for its UTC time:

    <out>/<YYYYmmddTHHMMSSZ>/
        meta.json          what was captured, ESPN status, OpenF1 leader, errors
        espn/              verbatim ESPN: scoreboard.json, status.json (the
                           session's core status: period, flag),
                           stats-<cid>.json and cstatus-<cid>.json per driver
        openf1/            verbatim OpenF1: sessions, drivers, position,
                           intervals (last 3 minutes), laps (the leader's),
                           stints, race_control, session_result
        server/            the live server: health.json, game-espn.json +
                           render-espn.png (no board: the raw ESPN game), and
                           game-board.json + render-board.png (as the given
                           board sees it; data_source_active says which feed)

The espn/ and openf1/ folders are exactly what the ignored fixture tests read,
so every snapshot re-renders through the server's own code afterwards:

    F1_FIXTURE_DIR=<out> F1_FIXTURE_OUT=/tmp/f1 \\
      cargo test -p server render_f1_fixture -- --ignored
    OPENF1_FIXTURE_DIR=<out> F1_FIXTURE_OUT=/tmp/f1 \\
      cargo test -p server render_openf1_fixture -- --ignored

OpenF1 refuses unauthenticated calls while a session is live, so this needs
OPENF1_EMAIL / OPENF1_KEY in the environment (scripts/capture-f1.sh loads
them from ~/.zshrc). The key and the bearer token stay in memory: nothing
here writes either to disk or to the log.

It waits for --start, captures every --every seconds, and stops on its own
once ESPN calls the session complete and OpenF1 has published the
classification (plus --tail minutes), or at --hard-stop, whichever is first.
"""

from __future__ import annotations  # the system python3 is 3.9

import argparse
import concurrent.futures
import datetime as dt
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

SERVER = "https://live-production-118c.up.railway.app"
ESPN_SCOREBOARD = "https://site.api.espn.com/apis/site/v2/sports/racing/f1/scoreboard"
ESPN_CORE = "https://sports.core.api.espn.com/v2/sports/racing/leagues/f1"
OPENF1 = "https://api.openf1.org/v1"

# The session names each feed uses, keyed by the server's session ids.
ESPN_ABBREV = {"race": "Race", "quali": "Qual", "sprint": "SR", "sprint-quali": "SS"}
OPENF1_NAME = {"race": "Race", "quali": "Qualifying", "sprint": "Sprint", "sprint-quali": "Sprint Qualifying"}


def now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def stamp(t: dt.datetime) -> str:
    return t.strftime("%Y%m%dT%H%M%SZ")


def parse_utc(s: str) -> dt.datetime:
    t = dt.datetime.fromisoformat(s.replace("Z", "+00:00"))
    return t if t.tzinfo else t.replace(tzinfo=dt.timezone.utc)


def log(out: pathlib.Path, msg: str) -> None:
    line = f"{now().strftime('%H:%M:%SZ')} {msg}"
    print(line, flush=True)
    with open(out / "capture.log", "a") as f:
        f.write(line + "\n")


def fetch(url: str, headers=None, data=None, timeout=25) -> tuple[int, bytes]:
    """(HTTP status, body). Network failure is status 0 with the error text."""
    # No custom User-Agent: ESPN answers 403 to unfamiliar ones and 200 to
    # urllib's default, which is also closest to what the server sends.
    req = urllib.request.Request(url, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001 - a capture never dies on one request
        return 0, str(e).encode()


class OpenF1:
    """Bearer-token client. The token lives an hour; refreshed with 5 left."""

    def __init__(self, email: str, key: str):
        self._email, self._key = email, key
        self._token, self._expiry = None, 0.0

    def _bearer(self):
        if self._token and time.time() < self._expiry - 300:
            return self._token
        body = urllib.parse.urlencode({"username": self._email, "password": self._key}).encode()
        status, raw = fetch("https://api.openf1.org/token", data=body,
                            headers={"Content-Type": "application/x-www-form-urlencoded"})
        if status != 200:
            raise RuntimeError(f"openf1 token: HTTP {status}")
        t = json.loads(raw)
        self._token = t["access_token"]
        self._expiry = time.time() + max(60, int(str(t.get("expires_in", "300"))))
        return self._token

    def get(self, path: str) -> tuple[int, bytes]:
        # 6 requests/second is the account's limit: stay well under it, and
        # back off on a 429 rather than lose the snapshot.
        for attempt in range(4):
            status, raw = fetch(f"{OPENF1}/{path}", headers={"Authorization": f"Bearer {self._bearer()}"})
            if status == 401:
                self._token = None
                continue
            if status == 429:
                time.sleep(1.5 * (attempt + 1))
                continue
            time.sleep(0.25)
            return status, raw
        return status, raw


def espn_session(session: str):
    """(event, competition) for today's weekend and the chosen session."""
    status, raw = fetch(ESPN_SCOREBOARD)
    if status != 200:
        raise RuntimeError(f"espn scoreboard: HTTP {status}")
    sb = json.loads(raw)
    for ev in sb.get("events", []):
        for comp in ev.get("competitions", []):
            if (comp.get("type") or {}).get("abbreviation") == ESPN_ABBREV[session]:
                return ev, comp, raw
    raise RuntimeError(f"espn: no {ESPN_ABBREV[session]} competition on the scoreboard")


def openf1_session(api: OpenF1, session: str):
    status, raw = api.get("sessions?meeting_key=latest")
    if status != 200:
        raise RuntimeError(f"openf1 sessions: HTTP {status}")
    for s in json.loads(raw):
        if s.get("session_name") == OPENF1_NAME[session]:
            return s
    raise RuntimeError(f"openf1: no {OPENF1_NAME[session]} session in the latest meeting")


def leader(positions: list) -> int | None:
    """The driver whose LATEST position record says 1 (as openf1.rs does)."""
    latest = {}
    for p in positions:
        n, d = p.get("driver_number"), p.get("date") or ""
        if n is not None and (n not in latest or d >= latest[n][0]):
            latest[n] = (d, p.get("position"))
    return next((n for n, (_, pos) in latest.items() if pos == 1), None)


def write(path: pathlib.Path, raw: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(raw)


def capture(out: pathlib.Path, args, api: OpenF1 | None, of1_session: dict | None) -> dict:
    t = now()
    snap = out / stamp(t)
    meta = {"captured_at": t.isoformat(), "session": args.session, "errors": []}

    def note(what: str, status: int, raw: bytes):
        # OpenF1 says "no rows yet" with a 404 — normal before the start and
        # before the classification is published, so not an error.
        if status == 404 and what.startswith("openf1"):
            meta.setdefault("empty", []).append(what)
        elif status != 200:
            meta["errors"].append(f"{what}: HTTP {status} {raw[:120].decode('utf-8', 'replace')}")

    # ---- ESPN: the scoreboard, the session's status, and the per-driver
    # resources the server fans out to (stats) plus their status (laps).
    try:
        ev, comp, sb_raw = espn_session(args.session)
        write(snap / "espn" / "scoreboard.json", sb_raw)
        eid, cid = ev["id"], comp["id"]
        base = f"{ESPN_CORE}/events/{eid}/competitions/{cid}"
        meta["espn"] = {"event": eid, "competition": cid, "circuit": (ev.get("circuit") or {}).get("fullName")}
        jobs = {"status.json": f"{base}/status?lang=en&region=us"}
        for c in comp.get("competitors", []):
            jobs[f"stats-{c['id']}.json"] = f"{base}/competitors/{c['id']}/statistics?lang=en&region=us"
            jobs[f"cstatus-{c['id']}.json"] = f"{base}/competitors/{c['id']}/status?lang=en&region=us"
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = dict(zip(jobs, pool.map(fetch, jobs.values())))
        for name, (status, raw) in results.items():
            note(f"espn {name}", status, raw)
            if status == 200:
                write(snap / "espn" / name, raw)
        st = results["status.json"]
        if st[0] == 200:
            s = json.loads(st[1])
            meta["espn"].update({
                "period": s.get("period"),
                "status": (s.get("type") or {}).get("name"),
                "state": (s.get("type") or {}).get("state"),
                "completed": (s.get("type") or {}).get("completed"),
                "flag": s.get("flag"),
            })
        meta["game_id"] = f"{eid}-{args.session}"
    except Exception as e:  # noqa: BLE001
        meta["errors"].append(f"espn: {e}")

    # ---- OpenF1: what the server's fetch_latest reads, for this session.
    if api and of1_session:
        key = of1_session["session_key"]
        meta["openf1"] = {"session_key": key, "circuit": of1_session.get("circuit_short_name")}
        try:
            since = (t - dt.timedelta(seconds=180)).strftime("%Y-%m-%dT%H:%M:%S")
            paths = {
                "sessions": f"sessions?session_key={key}",
                "latest": "sessions?session_key=latest",
                "drivers": f"drivers?session_key={key}",
                "position": f"position?session_key={key}",
                "stints": f"stints?session_key={key}",
                "race_control": f"race_control?session_key={key}",
                "session_result": f"session_result?session_key={key}",
            }
            if args.session in ("race", "sprint"):
                paths["intervals"] = f"intervals?session_key={key}&date%3E={since}"
            else:
                paths["laps"] = f"laps?session_key={key}"
            got = {}
            for name, path in paths.items():
                status, raw = api.get(path)
                note(f"openf1 {name}", status, raw)
                # A 404 is how OpenF1 says "no rows yet": keep it as an empty
                # list so the fixture replays what the server would parse.
                if status == 404:
                    raw = b"[]"
                if status in (200, 404):
                    write(snap / "openf1" / f"{name}.json", raw)
                    got[name] = raw
            if args.session in ("race", "sprint") and "position" in got:
                n = leader(json.loads(got["position"] or b"[]"))
                meta["openf1"]["leader"] = n
                if n is not None:
                    status, raw = api.get(f"laps?session_key={key}&driver_number={n}")
                    note("openf1 laps (leader)", status, raw)
                    if status == 404:
                        raw = b"[]"
                    if status in (200, 404):
                        write(snap / "openf1" / "laps.json", raw)
                        laps = [l.get("lap_number", 0) for l in json.loads(raw or b"[]")]
                        meta["openf1"]["leader_lap_records"] = max(laps, default=0)
            if "session_result" in got:
                meta["openf1"]["classified"] = len(json.loads(got["session_result"] or b"[]"))
        except Exception as e:  # noqa: BLE001
            meta["errors"].append(f"openf1: {e}")

    # ---- The live server: the game as it holds it, and as the board sees it.
    status, raw = fetch(f"{SERVER}/health")
    note("server health", status, raw)
    if status == 200:
        write(snap / "server" / "health.json", raw)
    gid = meta.get("game_id")
    if gid:
        views = {"espn": "", "board": f"?board={args.board}" if args.board else None}
        for view, q in views.items():
            if q is None:
                continue
            for kind, url, name in (("game", f"{SERVER}/game/{gid}{q}", f"game-{view}.json"),
                                    ("render", f"{SERVER}/render/{gid}.png{q}", f"render-{view}.png")):
                status, raw = fetch(url)
                note(f"server {kind} {view}", status, raw)
                if status == 200:
                    write(snap / "server" / name, raw)
                    if kind == "game" and view == "board":
                        try:
                            g = json.loads(raw)
                            meta["server_board"] = {
                                "data_source": g.get("data_source"),
                                "data_source_active": g.get("data_source_active"),
                                "laps": (g.get("leaderboard") or {}).get("laps"),
                                "flag": (g.get("leaderboard") or {}).get("flag"),
                            }
                        except ValueError:
                            pass

    write(snap / "meta.json", json.dumps(meta, indent=1).encode())
    return meta


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, type=pathlib.Path)
    ap.add_argument("--session", default="race", choices=sorted(ESPN_ABBREV))
    ap.add_argument("--board", default="", help="board id whose view the server renders")
    ap.add_argument("--start", help="UTC time to begin, e.g. 2026-09-26T10:40:00Z (default: now)")
    ap.add_argument("--hard-stop", help="UTC time to stop no matter what (default: start + 4h)")
    ap.add_argument("--every", type=int, default=60, help="seconds between snapshots")
    ap.add_argument("--tail", type=int, default=15, help="minutes to keep going after the session completes")
    ap.add_argument("--once", action="store_true", help="one snapshot now, then exit")
    args = ap.parse_args()

    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    start = parse_utc(args.start) if args.start else now()
    hard_stop = parse_utc(args.hard_stop) if args.hard_stop else start + dt.timedelta(hours=4)
    if now() >= hard_stop:
        log(out, f"past the hard stop ({hard_stop:%H:%MZ}); nothing to do")
        return 0

    email, key = os.environ.get("OPENF1_EMAIL"), os.environ.get("OPENF1_KEY")
    api = OpenF1(email, key) if email and key else None
    if not api:
        log(out, "OPENF1_EMAIL/OPENF1_KEY not set: capturing ESPN and the server only")

    if not args.once and now() < start:
        log(out, f"waiting for {start:%Y-%m-%d %H:%M:%SZ} ({args.session}, every {args.every}s, "
                 f"hard stop {hard_stop:%H:%MZ})")
        while now() < start:
            time.sleep(min(30, max(1, (start - now()).total_seconds())))

    of1_session = None
    done_at = None
    n = 0
    while True:
        tick = time.monotonic()
        if api and of1_session is None:
            try:
                of1_session = openf1_session(api, args.session)
                log(out, f"openf1 session {of1_session['session_key']} "
                         f"({of1_session.get('circuit_short_name')}, {of1_session.get('session_name')})")
            except Exception as e:  # noqa: BLE001
                log(out, f"openf1 session lookup failed, retrying next snapshot: {e}")
        try:
            meta = capture(out, args, api, of1_session)
        except Exception as e:  # noqa: BLE001
            meta = {"errors": [f"snapshot: {e}"]}
        n += 1
        e, o, b = meta.get("espn", {}), meta.get("openf1", {}), meta.get("server_board", {})
        log(out, f"#{n} espn {e.get('status')} lap {e.get('period')} flag {e.get('flag')} | "
                 f"openf1 leader #{o.get('leader')} lap-records {o.get('leader_lap_records')} "
                 f"classified {o.get('classified')} | board {b.get('data_source_active')} "
                 f"laps {b.get('laps')} | {len(meta.get('errors', []))} errors")
        for err in meta.get("errors", [])[:3]:
            log(out, f"   {err}")
        if args.once:
            return 0

        # Stop once ESPN says complete and OpenF1 has its classification, plus
        # the tail; an hour after completion regardless; or at the hard stop.
        if e.get("completed") and done_at is None:
            done_at = now()
            log(out, "espn reports the session complete")
        if done_at:
            waited = (now() - done_at).total_seconds() / 60
            if (o.get("classified") or not api) and waited >= args.tail or waited >= 60:
                log(out, f"done: {n} snapshots")
                return 0
        if now() >= hard_stop:
            log(out, f"hard stop: {n} snapshots")
            return 0
        time.sleep(max(1, args.every - (time.monotonic() - tick)))


if __name__ == "__main__":
    sys.exit(main())

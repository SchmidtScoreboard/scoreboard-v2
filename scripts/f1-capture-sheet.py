#!/usr/bin/env python3
"""One HTML page comparing a capture's inputs and frames, snapshot by snapshot.

    python3 scripts/f1-capture-sheet.py <capture root> <replay dir> <out.html>

<capture root> is what scripts/capture-f1.py wrote; <replay dir> is what
`render_f1_capture` wrote from it (F1_FIXTURE_OUT). Each row shows what each
feed said (ESPN status, lap and flag; OpenF1 leader and classification), the
frame the live server actually served, and the full-depth board frames the
replay drew for ESPN and for OpenF1, with both towers' rows side by side and
any non-leader row whose timing came out blank marked.
"""

import base64
import html
import json
import pathlib
import sys


def img(path: pathlib.Path, label: str) -> str:
    if not path.exists():
        return f'<div class="frame none">{html.escape(label)}<br>—</div>'
    b = base64.b64encode(path.read_bytes()).decode()
    return f'<div class="frame"><img src="data:image/png;base64,{b}" alt="{html.escape(label)}"><span>{html.escape(label)}</span></div>'


def rows_html(rows, status) -> str:
    if not rows:
        return '<pre class="rows">—</pre>'
    out = []
    for i, r in enumerate(rows):
        parts = r.split()
        # rank + code and no value, below the leader, once there is timing to
        # have (a pregame tower is supposed to be empty)
        blank = status != "Pregame" and i > 0 and len(parts) < 3
        line = html.escape(r)
        out.append(f'<b class="blank">{line}  ← blank</b>' if blank else line)
    return '<pre class="rows">' + "\n".join(out) + "</pre>"


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    cap, rep, dest = map(pathlib.Path, sys.argv[1:])
    snaps = sorted(p for p in cap.iterdir() if (p / "meta.json").exists())
    blocks = []
    for snap in snaps:
        name = snap.name
        meta = json.loads((snap / "meta.json").read_text())
        summ_path = rep / f"{name}-summary.json"
        summ = json.loads(summ_path.read_text()) if summ_path.exists() else {}
        e, o, b = meta.get("espn", {}), meta.get("openf1", {}), meta.get("server_board") or {}
        es, os_ = summ.get("espn") or {}, summ.get("openf1") or {}
        facts = (
            f"<b>{name[9:11]}:{name[11:13]}:{name[13:15]}Z</b> "
            f"ESPN {html.escape(str(e.get('status')))} · period {e.get('period')} · flag {e.get('flag')}"
            f" &nbsp;|&nbsp; OpenF1 leader #{o.get('leader')} · lap records {o.get('leader_lap_records')}"
            f" · classified {o.get('classified')}"
            f" &nbsp;|&nbsp; board on {b.get('data_source_active')}"
            f" &nbsp;|&nbsp; bar ESPN {es.get('laps')} · OpenF1 {os_.get('laps')}"
            f" &nbsp;|&nbsp; flag ESPN {es.get('flag')} · OpenF1 {os_.get('flag')}"
        )
        errs = meta.get("errors") or []
        err_html = f'<div class="err">{html.escape("; ".join(errs[:3]))}</div>' if errs else ""
        board = ""
        if b.get("data_source_active") == "openf1":
            board = img(snap / "server" / "render-board.png", "server: board (OpenF1)")
        blocks.append(f"""
<section>
  <div class="facts">{facts}</div>{err_html}
  <div class="frames">
    {img(snap / "server" / "render-espn.png", "server: served (ESPN)")}
    {board}
    {img(rep / f"{name}-espn.png", "replay: ESPN")}
    {img(rep / f"{name}-openf1.png", "replay: OpenF1")}
    <div class="tower"><span>ESPN rows</span>{rows_html(es.get("rows"), es.get("status"))}</div>
    <div class="tower"><span>OpenF1 rows</span>{rows_html(os_.get("rows"), os_.get("status"))}</div>
  </div>
</section>""")
    dest.write_text(f"""<!doctype html><meta charset="utf-8"><title>F1 capture {html.escape(cap.name)}</title>
<style>
body{{background:#111;color:#ddd;font:13px system-ui;margin:0;padding:16px}}
section{{border-top:1px solid #333;padding:12px 0}}
.facts{{margin-bottom:8px;line-height:1.5}} .err{{color:#f77;margin-bottom:6px}}
.frames{{display:flex;flex-wrap:wrap;gap:12px;align-items:flex-start}}
.frame{{background:#000;padding:6px;border-radius:6px;text-align:center}}
.frame img{{width:256px;image-rendering:pixelated;display:block}}
.frame span,.tower span{{display:block;color:#999;font-size:11px;margin-top:4px}}
.frame.none{{width:256px;color:#666;padding:20px 6px}}
.rows{{margin:0;font:12px ui-monospace,monospace;color:#ccc}} .blank{{color:#fa5}}
</style>
<h1 style="font-size:16px">F1 capture: {html.escape(cap.name)} ({len(snaps)} snapshots)</h1>
{''.join(blocks)}""")
    print(f"wrote {dest} ({len(snaps)} snapshots)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

import json, time, urllib.request, sys, collections
def get(url):
    for i in range(4):
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                return json.load(r)
        except Exception as e:
            time.sleep(1.5 * (i + 1))
    return []
rows = []
for year in (2023, 2024, 2025, 2026):
    sessions = get(f"https://api.openf1.org/v1/sessions?year={year}&session_type=Race")
    time.sleep(0.3)
    for s in sessions:
        sprint = s["session_name"] == "Sprint"
        floor = 8 if sprint else 30
        laps = get(f"https://api.openf1.org/v1/laps?session_key={s['session_key']}&lap_number>={floor}")
        time.sleep(0.3)
        mx = max((l["lap_number"] for l in laps), default=None)
        rows.append(dict(year=year, key=s["session_key"], circuit=s["circuit_short_name"], circuit_key=s["circuit_key"],
                         country=s["country_name"], location=s["location"], session=s["session_name"], laps=mx,
                         date=s["date_start"][:10]))
        print(year, s["date_start"][:10], s["circuit_short_name"], s["circuit_key"], s["session_name"], mx, flush=True)
json.dump(rows, open(sys.argv[1], "w"), indent=1)

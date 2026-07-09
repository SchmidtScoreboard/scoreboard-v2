# Data source options per league

Research verified live on **2026-07-09** (pricing pages fetched, endpoints curl-tested — several
during in-progress events). Prices and limits change; re-verify before committing to a paid tier.

## What the board actually consumes

Any source is judged against the fields the server renders today:

| Need | Used by |
|---|---|
| Team catalog: names, abbreviations, **primary + secondary brand colors** | every team-sport frame, app editor |
| Score, clock, period/ordinal, status, ISO start times | every frame |
| Situations: down/distance/possession (NFL/CFB), balls/strikes/outs/runners/half (MLB), power play/skaters (NHL) | situational layouts |
| Pregame: season records + national TV broadcast | scheduled-game frames |
| Championship context ("Stanley Cup Final", WC rounds) | big-game subscriptions, trophy captions |
| Leaderboards: position, name, score/gap, (golf) thru/round, (F1) session + tires | golf/F1 towers |

**The colors problem (cross-cutting):** ESPN is the *only* free live source that carries brand
colors. Every recommendation below keeps the ESPN team catalog (fetched once, cached to disk)
even where live data moves elsewhere. Escape hatches if ESPN's catalog ever dies: the
[teamcolors](https://github.com/jimniels/teamcolors) static dataset baked into the repo, or
TheSportsDB ($9/mo, verified real hexes: `strColour1/2`).

**House rule:** several vendors push betting/odds products. They were evaluated strictly on
scoring data; odds are never a benefit and never rendered.

---

## NHL — adopt api-web.nhle.com fully (free)

Already trusted for power plays. Verified today it can carry the whole NHL feature set:

| | ESPN (current) | **api-web.nhle.com** (recommended) |
|---|---|---|
| Pricing / auth | free, none | free, none (no tiers exist; plain curl works) |
| Latency | ~minutes | seconds — it backs nhl.com GameCenter |
| Gains | — | shots on goal (`sog`), national TV (`tvBroadcasts`, verified "ABC" on SCF), `seriesStatus` with round + wins ("Stanley Cup Final", 2–1) — ready-made for series lines, exact clock (`secondsRemaining`, `inIntermission`), `periodDescriptor` REG/OT/SO, empty-net flags |
| Missing | — | **team colors** (keep ESPN catalog); records need one cached `/v1/standings/now` join |
| Risk | undocumented | undocumented but first-party; stable for years; predecessor was retired with notice in 2023 |

Notes: `/v1/score/now` 307-redirects to a dated URL (follow redirects). The payload contains an
`oddsPartners` block — ignored per house rule.

## NBA — adopt cdn.nba.com liveData (free), keep ESPN for WNBA

| | ESPN (current) | **cdn.nba.com liveData** (recommended for NBA) |
|---|---|---|
| Pricing / auth | free, none | free, no key — but **requires browser-like headers** (bare curl gets Akamai 403; verified working recipe: Chrome UA + Referer/Origin nba.com + Sec-Fetch headers) |
| Latency | ~minutes | seconds (nba.com's own feed) |
| Gains | — | **`inBonus` + `timeoutsRemaining` per team** — the basketball situation row ESPN can't provide; tenths-precision clock; records inline; `seriesText`/`gameLabel` ("NBA Finals", "Game 5"); national TV via the daily `scheduleLeagueV2_1.json` (~0.5 MB — fetch daily, not per poll) |
| Missing | — | team colors (keep ESPN catalog); no shot clock anywhere public; scoreboard is today-only |
| Risk | undocumented | Akamai bot management could tighten; residential IP (this deployment) is the favorable case |

**WNBA: stay on ESPN.** Verified dead ends: cdn.nba.com's WNBA scoreboard is frozen at
2020-10-06; the legacy S3 feed 403s; stats.wnba.com (what wnba.com itself uses) hangs for
non-browser clients even from a residential Mac — silent timeouts, the worst failure mode for
a poller.

## MLB — adopt MLB StatsAPI (free, quasi-blessed)

The strongest recommendation in this document.

| | ESPN (current) | **statsapi.mlb.com** (recommended) |
|---|---|---|
| Pricing / auth | free, none | free, none — and its embedded [copyright notice](http://gdx.mlb.com/components/copyright.txt) explicitly permits individual non-commercial use; a home board fits squarely |
| Latency | ~minutes | pitch-by-pitch; the live feed even suggests its own poll interval (`metaData.wait`) and supports `?timecode` diffs |
| Gains | — | exact runners (`offense.first/second/third` vs ESPN's summary), current batter AND pitcher, **due-up batters** (`onDeck`/`inHole` — the intermission-frame idea), richer statuses (Warmup/Delayed/Suspended), records + broadcasts with an `isNational` flag on one hydrated schedule call |
| Missing | — | team colors (keep ESPN catalog) |
| Risk | undocumented | powers MLB's own Gameday, ~8 years stable, large tolerated wrapper ecosystem |

## NFL — no good alternative; stay on ESPN

Verified: **api.nfl.com is partner-gated** (developer portal 401s; community token-harvesting
breaks on every key rotation and is ToS-hostile). **Sportradar** is enterprise-only (trial:
1,000 calls per 30 days; contracts reportedly ~$10k+/mo). **SportsDataIO** documentably has
everything (incl. `HomeTimeouts`/`RedZone`) but scrambles trial data and hides pricing behind
sales (~$500+/mo reported). **Tank01 on RapidAPI** ($10/mo covers the poll cadence) is
fantasy-box-score oriented — down/distance/timeouts unverified, likely absent.

ESPN's situation object (down/distance/possession/`isRedZone`/timeouts) remains the best NFL
data at any hobby price — including several fields not yet rendered.

## College — CFBD + CBBD ($1–5/mo Patreon), WCBB stays on ESPN

| | ESPN (current) | **CollegeFootballData.com** (CFB) / **CollegeBasketballData.com** (CBB) |
|---|---|---|
| Pricing | free | one shared Patreon: **$1/mo** = 5k calls + live scoreboard, **$5/mo** = 30k + live play-by-play ([tiers](https://collegefootballdata.com/api-tiers)); free tier has NO live data |
| Auth | none | API key (instant signup) |
| Gains | — | a *legitimate, documented* API (the only one here with clean ToS); `/teams` has **color + alternateColor** (the one non-ESPN source with proper colors); AP + CFP rankings; TV network; CBBD adds structured tournament/seed context (March Madness rounds) |
| Missing | — | CFB situation is one preformatted string ("3rd & 8 at OSU 24") — needs a regex vs ESPN's structured fields; records are a separate call; **CBBD has zero women's data** |
| Risk | undocumented | official but one-maintainer; tiers "subject to change" |

**WCBB:** the only live alternative is the self-hostable
[henrygd/ncaa-api](https://github.com/henrygd/ncaa-api) proxy (MIT, Docker one-liner; public
instance is best-effort 5 req/s) — verified serving live WCBB with rank, network, and bracket
round, but no colors and no structured situations. Reasonable self-hosted fallback; ESPN stays
simplest.

## Soccer (MLS / EPL / World Cup) — stay on ESPN; API-Football if richer live fields wanted

| | ESPN (current) | **API-Football** ($19/mo Pro) | football-data.org |
|---|---|---|---|
| Coverage | all three | all three (MLS incl. playoffs) | EPL + WC free; **MLS needs €49/mo, MLS playoffs €199/mo** — verified against their live competitions endpoint |
| Pricing | free | free tier 100 req/day **excludes current season** (community-confirmed), so live use = $19/mo (7,500/day) | free (delayed, undocumented lag) / €12/mo live / cards need €29/mo pack |
| Gains | — | true match minute **plus stoppage time** (`elapsed`+`extra` → 90+4), red cards as events, first-class shootout score, round names ("Semi-finals") | minute + `injuryTime`, stage names |
| Missing | — | no national TV; only per-match *kit* colors (keep ESPN catalog) | red cards paywalled; text color names only |

ESPN already covers scores/status/records/broadcasts/colors for all three competitions.
API-Football is the upgrade path if stoppage-time clocks, red cards, or shootout frames become
wanted — the soccer analog of the situational layouts.

Sofascore/FotMob: best-in-class data, actively defended against scraping (signed headers,
datacenter blocks), explicit ToS prohibitions — skip.

## Golf — PGA: orchestrator GraphQL (free); LPGA: ESPN leaderboard endpoint (free)

| | ESPN scoreboard (current) | **orchestrator.pgatour.com GraphQL** (PGA) | **ESPN leaderboard endpoint** (LPGA) |
|---|---|---|---|
| Pricing / auth | free | free; `x-api-key` extracted from pgatour.com's JS bundle (stable for years, re-extractable by grepping for `da2-`) — verified working today mid-tournament | free, none |
| Gains | — | position ("T1"), thru ("F*"), today + total, round, player state, tee times, **projected cut line**, player country, season schedule | per-player **thru/hole/round**, position ("T87"), country flags, national TV ("Golf Chnl") — fields the plain scoreboard endpoint we currently use omits |
| Missing | — | **no LPGA** (tour code doesn't exist); no TV field | cut line null in round 1 (unverified later) |
| Risk | undocumented | unofficial + key rotation possible; schema churn observed (V2→V3) | same class as existing ESPN usage |

LPGA reality: verified today that *nothing else* covers it live at hobby prices — SlashGolf
(PGA/LIV only), DataGolf (no LPGA; betting-analytics shop besides), SportsDataIO golf
($99/mo tier is **next-day delayed**), and the official LPGA datafeed host no longer resolves.
ESPN's leaderboard endpoint (during an LPGA major, today) is the only free live source.
Paid hedge if the free PGA paths break: SlashGolf Pro/Ultra ($25/$60/mo — free tier's
250 calls/mo is ~one day of polling).

## F1 — OpenF1 (decided), €9.90/mo for realtime

Verified: free tier is **historical only, ~30 min post-session**; the
[paid supporter account](https://openf1.org/auth) (€9.90/mo) unlocks live REST + MQTT/WebSocket,
~3s behind live. `/stints` carries tire compound + age (lights up the tire strips), `/intervals`
+ `/position` beat ESPN's gaps, `/drivers` maps numbers → codes. Keep ESPN for session
scheduling/status skeleton or migrate fully; unofficial single-maintainer project — treat as
best-effort.

## Multi-sport umbrellas — evaluated, not recommended

| Vendor | Verified pricing | Verdict |
|---|---|---|
| **TheSportsDB** | $9/mo ([pricing](https://www.thesportsdb.com/pricing)): 2-min livescores (big-5 leagues only), 100 req/min | Best cheap *fallback*: real brand-color hexes (verified) + `idESPN` cross-refs. No situations, no clock granularity, no CFB livescore. Degraded mode = scores without the interesting parts |
| **API-Sports** | free 100 req/day per sport; $19–39/mo *per sport* (~$95–133/mo for our slate) | 15s updates, but **no situations** (no down/distance, no count/runners, no PP state) and no colors — pays more for less of what makes this board good |
| **SportsDataIO** | trial = scrambled data; Discovery Lab $99–149/mo = **next-day delayed**; live = sales quote ~$500+/mo | Data superset, price non-starter. Beware: scrambled trial data looks plausible on a 64×32 panel |
| **Sportradar** | trial 1,000 calls/30 days; contracts ~$1,250/mo floor, typically 5 figures | The "if this ever became a product" reference point |

## Summary

| League | Recommendation | Cost | Headline gain |
|---|---|---|---|
| NHL | api-web.nhle.com (full adoption) | free | SOG, series status, exact clock, TV |
| NBA | cdn.nba.com liveData | free | bonus + timeouts situation row |
| WNBA | stay on ESPN | free | (all alternatives verified dead/blocked) |
| MLB | MLB StatsAPI | free | exact runners, batter/pitcher, due-up |
| NFL | stay on ESPN | free | (already the best hobby source; unused fields remain) |
| CFB | CFBD | $1–5/mo | legit API, colors, AP/CFP ranks |
| CBB | CBBD (same Patreon) | included | tournament rounds + seeds |
| WCBB | stay on ESPN (ncaa-api as self-hosted fallback) | free | — |
| MLS/EPL/WC | stay on ESPN (API-Football $19/mo if stoppage/cards wanted) | free | — |
| PGA | orchestrator GraphQL | free | thru, projected cut, country |
| LPGA | ESPN *leaderboard* endpoint | free | thru/round/position (missing today) |
| F1 | OpenF1 realtime | €9.90/mo | live gaps + **tire compounds** |

Full build-out: **~$11–16/month** (OpenF1 + CFBD Patreon), everything else free.

## Migration notes

- `SportsDataSource` is already a trait; each adoption is a new source composed behind it, the
  way `attach_nhl_situations` bolts onto ESPN today. Hybrid-per-league is the natural shape:
  **catalog (colors) from ESPN, live data from the best per-league feed.**
- The demo/catalog disk cache means catalog fetches stay rare — color risk is cold-start only.
- Every free source here is unofficial and can change shape silently. The mitigation that works
  is the one already in place: tolerant serde (`#[serde(default)]` everywhere), per-source
  timeouts, and frames that degrade to fewer fields rather than erroring.
- Order of adoption by value: MLB StatsAPI (biggest gain, best-blessed), NHL full adoption,
  OpenF1 (tires), NBA liveData, golf endpoints, college Patreon.

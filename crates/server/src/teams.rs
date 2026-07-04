//! Curated short team names, ported from the original Python backend's per-league
//! `team_map`. For known teams we use the hand-picked short name; for unknown
//! ones we fall back to ESPN's shortDisplayName with the original >11-char rule.

/// Resolve the on-scoreboard name for a team.
pub fn short_name(sport_id: &str, abbrev: &str, espn_short: &str) -> String {
    if let Some(curated) = curated(sport_id, abbrev) {
        return curated.to_string();
    }
    let s = shorten(espn_short);
    if s.chars().count() <= 11 {
        s
    } else {
        abbrev.to_string()
    }
}

fn curated(sport_id: &str, abbrev: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = match sport_id {
        "nhl" => NHL,
        "nba" => NBA,
        "mlb" => MLB,
        "nfl" => NFL,
        _ => return None,
    };
    table.iter().find(|(a, _)| *a == abbrev).map(|(_, n)| *n)
}

/// Original shortening rule (team_generator.py): only applied when >11 chars —
/// trailing "State" -> "St", leading direction word -> its initial.
fn shorten(name: &str) -> String {
    if name.chars().count() <= 11 {
        return name.to_string();
    }
    let mut parts: Vec<String> = name.split_whitespace().map(|s| s.to_string()).collect();
    if let Some(last) = parts.last_mut() {
        if last == "State" {
            *last = "St".to_string();
        }
    }
    if let Some(first) = parts.first_mut() {
        for (dir, init) in [("North", "N"), ("East", "E"), ("South", "S"), ("West", "W"), ("Central", "C")] {
            if first == dir {
                *first = init.to_string();
                break;
            }
        }
    }
    parts.join(" ")
}

static NHL: &[(&str, &str)] = &[
    ("NJD", "Devils"), ("NYI", "Islanders"), ("NYR", "Rangers"), ("PHI", "Flyers"),
    ("PIT", "Penguins"), ("BOS", "Bruins"), ("BUF", "Sabres"), ("MTL", "Canadiens"),
    ("OTT", "Senators"), ("TOR", "Leafs"), ("CAR", "Canes"), ("FLA", "Panthers"),
    ("TBL", "Lightning"), ("WSH", "Capitals"), ("CHI", "B Hawks"), ("DET", "Red Wings"),
    ("NSH", "Predators"), ("STL", "Blues"), ("CGY", "Flames"), ("COL", "Avalanche"),
    ("EDM", "Oilers"), ("VAN", "Canucks"), ("ANA", "Ducks"), ("DAL", "Stars"),
    ("LAK", "Kings"), ("SJS", "Sharks"), ("CBJ", "B Jackets"), ("MIN", "Wild"),
    ("WPG", "Jets"), ("ARI", "Coyotes"), ("VGK", "Knights"), ("SEA", "Kraken"),
    ("UTA", "Mammoth"),
];

static NBA: &[(&str, &str)] = &[
    ("ATL", "Hawks"), ("BOS", "Celtics"), ("BKN", "Nets"), ("CHA", "Hornets"),
    ("CHI", "Bulls"), ("CLE", "Cavaliers"), ("DAL", "Mavericks"), ("DEN", "Nuggets"),
    ("DET", "Pistons"), ("GS", "Warriors"), ("HOU", "Rockets"), ("IND", "Pacers"),
    ("LAC", "Clippers"), ("LAL", "Lakers"), ("MEM", "Grizzlies"), ("MIA", "Heat"),
    ("MIL", "Bucks"), ("MIN", "T Wolves"), ("NO", "Pelicans"), ("NY", "Knicks"),
    ("OKC", "Thunder"), ("ORL", "Magic"), ("PHI", "76ers"), ("PHX", "Suns"),
    ("POR", "T Blazers"), ("SAC", "Kings"), ("SA", "Spurs"), ("TOR", "Raptors"),
    ("UTAH", "Jazz"), ("WSH", "Wizards"),
];

static MLB: &[(&str, &str)] = &[
    ("ARI", "D-backs"), ("ATL", "Braves"), ("BAL", "Orioles"), ("BOS", "Red Sox"),
    ("CHC", "Cubs"), ("CHW", "White Sox"), ("CIN", "Reds"), ("CLE", "Guardians"),
    ("COL", "Rockies"), ("DET", "Tigers"), ("HOU", "Astros"), ("KC", "Royals"),
    ("LAA", "Angels"), ("LAD", "Dodgers"), ("MIA", "Marlins"), ("MIL", "Brewers"),
    ("MIN", "Twins"), ("NYM", "Mets"), ("NYY", "Yankees"), ("OAK", "Athletics"),
    ("PHI", "Phillies"), ("PIT", "Pirates"), ("SD", "Padres"), ("SF", "Giants"),
    ("SEA", "Mariners"), ("STL", "Cardinals"), ("TB", "Rays"), ("TEX", "Rangers"),
    ("TOR", "Blue Jays"), ("WSH", "Nationals"),
];

static NFL: &[(&str, &str)] = &[
    ("ARI", "Cardinals"), ("ATL", "Falcons"), ("BAL", "Ravens"), ("BUF", "Bills"),
    ("CAR", "Panthers"), ("CHI", "Bears"), ("CIN", "Bengals"), ("CLE", "Browns"),
    ("DAL", "Cowboys"), ("DEN", "Broncos"), ("DET", "Lions"), ("GB", "Packers"),
    ("HOU", "Texans"), ("IND", "Colts"), ("JAX", "Jaguars"), ("KC", "Chiefs"),
    ("LV", "Raiders"), ("LAC", "Chargers"), ("LAR", "Rams"), ("MIA", "Dolphins"),
    ("MIN", "Vikings"), ("NE", "Patriots"), ("NO", "Saints"), ("NYG", "Giants"),
    ("NYJ", "Jets"), ("PHI", "Eagles"), ("PIT", "Steelers"), ("SF", "49ers"),
    ("SEA", "Seahawks"), ("TB", "Buccaneers"), ("TEN", "Titans"), ("WSH", "Washington"),
];

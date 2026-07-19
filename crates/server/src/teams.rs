//! Curated short team names, ported from the original Python backend's per-league
//! `team_map`. For known teams we use the hand-picked short name; for unknown
//! ones we fall back to ESPN's shortDisplayName with the original >11-char rule.

/// Resolve the on-scoreboard team NAME (mascot). `mascot` is ESPN's `name`
/// field ("Fighting Illini", "Yellow Jackets", "Bruins").
pub fn short_name(sport_id: &str, abbrev: &str, mascot: &str) -> String {
    if let Some(curated) = curated(sport_id, abbrev) {
        return curated.to_string();
    }
    let m = ascii_fold(mascot.trim());
    if m.chars().count() <= 11 {
        return m;
    }
    // A two-plus-word mascot's LAST word is the recognizable core — an
    // adjective ("Fighting"/"Yellow"/"Golden") or place prefix leads it:
    // "Fighting Illini" -> "Illini", "Yellow Jackets" -> "Jackets".
    if let Some(last) = m.split_whitespace().last() {
        if last.chars().count() > 1 && last.chars().count() <= 11 {
            return last.to_string();
        }
    }
    abbrev.to_string()
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

/// Shorten a city / college / country for the `Location` name style. Unlike the
/// mascot rule this applies unconditionally (the substitutions are canonical
/// abbreviations that always read): a leading "University of" / "Univ" is
/// dropped, a trailing "State" becomes "St", and a leading direction word
/// collapses to its initial ("North Carolina" -> "N Carolina") — the same
/// transforms `team_generator.py` used, plus the college "University of" trim.
/// Case-insensitive on the tokens it matches; other words keep their case (the
/// board uppercases the whole string at draw time).
/// ASCII-fold accented letters — the 4x6 / 5x8 board fonts are ASCII-only, so
/// "Türkiye"/"CF Montréal"/"Curaçao" would otherwise render blank glyphs.
pub fn ascii_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => 'A',
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'I',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => 'O',
            'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'U',
            'ç' | 'Ç' => 'C',
            'ñ' | 'Ñ' => 'N',
            'ı' => 'I',
            'ş' | 'Ş' => 'S',
            'ğ' | 'Ğ' => 'G',
            other => other,
        })
        .collect()
}

/// Curated location short forms for names that don't fit even after the
/// mechanical rules — hand-add here when `server team-names` flags one you
/// want spelled out instead of falling back to the abbreviation. Keyed by the
/// UPPERCASE ASCII-folded raw location.
fn curated_location(name_upper: &str) -> Option<&'static str> {
    const TABLE: &[(&str, &str)] = &[
        ("NORTH CAROLINA", "N CAROLINA"),
        ("SOUTH CAROLINA", "S CAROLINA"),
        ("BOSTON COLLEGE", "BOSTON C"),
        ("MANCHESTER UNITED", "MAN UNITED"),
        ("MANCHESTER CITY", "MAN CITY"),
        ("BOSNIA-HERZEGOVINA", "BOSNIA"),
        ("UNITED STATES", "USA"),
        ("SAUDI ARABIA", "SAUDI"),
    ];
    TABLE.iter().find(|(k, _)| *k == name_upper).map(|(_, v)| *v)
}

pub fn shorten_location(name: &str) -> String {
    let folded = ascii_fold(name);
    if let Some(curated) = curated_location(&folded.to_ascii_uppercase()) {
        return curated.to_string();
    }
    let mut parts: Vec<String> = folded.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        return String::new();
    }
    let up = |s: &str| s.to_ascii_uppercase();
    // Drop a trailing club suffix (soccer: "... FC" / "... SC" / "... United").
    if parts.len() > 1 {
        if let Some(last) = parts.last() {
            if matches!(up(last).as_str(), "FC" | "SC" | "CF" | "AFC") {
                parts.pop();
            }
        }
    }
    // Drop a leading "University of" / "Univ." (rare in ESPN's location field,
    // but defensive for college feeds that spell it out).
    if matches!(up(&parts[0]).trim_end_matches('.'), "UNIVERSITY" | "UNIV") {
        parts.remove(0);
        if parts.first().map(|s| up(s)).as_deref() == Some("OF") {
            parts.remove(0);
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    // Trailing "State" -> "St".
    if let Some(last) = parts.last_mut() {
        if up(last) == "STATE" {
            *last = "St".to_string();
        }
    }
    // Leading direction word -> its initial. ESPN's college locations use both
    // the bare ("East Carolina") and the -ERN form ("Eastern Michigan").
    if let Some(first) = parts.first_mut() {
        for (dir, init) in [
            ("NORTH", "N"), ("NORTHERN", "N"), ("EAST", "E"), ("EASTERN", "E"),
            ("SOUTH", "S"), ("SOUTHERN", "S"), ("WEST", "W"), ("WESTERN", "W"),
            ("CENTRAL", "C"),
        ] {
            if up(first) == dir {
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

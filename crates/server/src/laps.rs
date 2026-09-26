//! Scheduled race distances, in laps, per circuit.
//!
//! Neither feed carries the total: ESPN's scoreboard and core competition
//! resources have no lap count, and OpenF1's sessions only name the circuit.
//! Both feeds DO name the circuit, so the distance lives here, keyed by
//! either feed's spelling of the venue.
//!
//! The numbers are stable: a Grand Prix distance is the fewest laps that
//! exceed 305 km (Monaco excepted, ~260 km) and a sprint the fewest over
//! 100 km, so a circuit's count only changes when its layout does. Every
//! entry below was checked against OpenF1's lap records for 2024, 2025 and
//! the 2026 races run so far (`design/f1-logs` has the method); the two that
//! have not run yet — Sepang, and Singapore's first sprint — are the rule
//! applied to the published circuit length, and are marked. A shortened
//! race (red flag, timed out) still draws against its scheduled distance,
//! which is what the broadcast counts down to as well.
//!
//! An unknown circuit resolves to None and the frame draws no bar: no bar
//! beats a wrong one.

/// `(feed spellings of the venue, race laps, sprint laps)`. ESPN uses the
/// circuit's full name; OpenF1 its short name. A sprint of None means the
/// circuit has never hosted one, so a surprise sprint draws nothing rather
/// than a guess.
const CIRCUITS: &[(&[&str], u32, Option<u32>)] = &[
    (&["Melbourne Grand Prix Circuit", "Albert Park", "Melbourne"], 58, None),
    (&["Shanghai International Circuit", "Shanghai"], 56, Some(19)),
    (&["Suzuka International Racing Course", "Suzuka Circuit", "Suzuka"], 53, None),
    (&["Bahrain International Circuit", "Sakhir"], 57, None),
    (&["Jeddah Street Circuit", "Jeddah Corniche Circuit", "Jeddah"], 50, None),
    (&["Miami International Autodrome", "Miami"], 57, Some(19)),
    (&["Circuit Gilles-Villeneuve", "Circuit Gilles Villeneuve", "Montreal"], 70, Some(23)),
    (&["Circuit de Monaco", "Monte Carlo"], 78, None),
    (&["Circuit de Catalunya", "Circuit de Barcelona-Catalunya", "Catalunya"], 66, None),
    (&["Red Bull Ring", "Spielberg"], 71, None),
    (&["Silverstone Circuit", "Silverstone"], 52, Some(17)),
    (&["Circuit de Spa-Francorchamps", "Spa-Francorchamps"], 44, Some(15)),
    (&["Hungaroring"], 70, None),
    (&["Circuit Park Zandvoort", "Circuit Zandvoort", "Zandvoort"], 72, Some(24)),
    (&["Autodromo Nazionale Monza", "Autodromo Nazionale di Monza", "Monza"], 53, None),
    (&["Madring", "Madrid"], 57, None),
    (&["Baku City Circuit", "Baku"], 51, Some(17)),
    // 5.543 km, 305 km rule: 56. Returns in 2026; last run 2017 at 56.
    (&["Sepang International Circuit", "Sepang", "Kuala Lumpur"], 56, None),
    // Sprint: 4.940 km, 100 km rule: 21. First sprint here is October 2026.
    (&["Marina Bay Street Circuit", "Singapore"], 62, Some(21)),
    (&["Circuit of the Americas", "Austin"], 56, Some(19)),
    (&["Autodromo Hermanos Rodriguez", "Autódromo Hermanos Rodríguez", "Mexico City"], 71, None),
    (&["Autodromo Jose Carlos Pace", "Autódromo José Carlos Pace", "Interlagos"], 71, Some(24)),
    (&["Las Vegas Street Circuit", "Las Vegas Strip Circuit", "Las Vegas"], 50, None),
    (&["Losail International Circuit", "Lusail International Circuit", "Lusail"], 57, Some(19)),
    (&["Yas Marina Circuit", "Yas Marina"], 58, None),
    (&["Autodromo Enzo e Dino Ferrari", "Autodromo Internazionale Enzo e Dino Ferrari", "Imola"], 63, None),
];

/// Letters and digits only, lowercased, so "Circuit Gilles-Villeneuve" and
/// "circuit gilles villeneuve" are one key and an accent does not matter.
fn key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            c => c,
        })
        .collect()
}

/// The scheduled distance of a race (or, `sprint`, a sprint) at `circuit`,
/// under either feed's name for it. None for a circuit not in the table, an
/// empty name, or a sprint at a circuit that has never held one.
pub fn total_laps(circuit: &str, sprint: bool) -> Option<u32> {
    let k = key(circuit);
    if k.is_empty() {
        return None;
    }
    let (_, race, sprint_laps) = CIRCUITS.iter().find(|(names, _, _)| names.iter().any(|n| key(n) == k))?;
    if sprint { *sprint_laps } else { Some(*race) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ESPN's circuit names for the whole 2026 calendar, as its scoreboard
    /// listed them on 2026-09-15, every one of which must resolve.
    #[test]
    fn every_2026_espn_circuit_resolves() {
        let espn = [
            ("Melbourne Grand Prix Circuit", 58), ("Shanghai International Circuit", 56),
            ("Suzuka International Racing Course", 53), ("Bahrain International Circuit", 57),
            ("Jeddah Street Circuit", 50), ("Miami International Autodrome", 57),
            ("Circuit Gilles-Villeneuve", 70), ("Circuit de Monaco", 78), ("Circuit de Catalunya", 66),
            ("Red Bull Ring", 71), ("Silverstone Circuit", 52), ("Circuit de Spa-Francorchamps", 44),
            ("Hungaroring", 70), ("Circuit Park Zandvoort", 72), ("Autodromo Nazionale Monza", 53),
            ("Madring", 57), ("Baku City Circuit", 51), ("Sepang International Circuit", 56),
            ("Marina Bay Street Circuit", 62), ("Circuit of the Americas", 56),
            ("Autodromo Hermanos Rodriguez", 71), ("Autodromo Jose Carlos Pace", 71),
            ("Las Vegas Street Circuit", 50), ("Losail International Circuit", 57), ("Yas Marina Circuit", 58),
        ];
        for (name, laps) in espn {
            assert_eq!(total_laps(name, false), Some(laps), "{name}");
        }
    }

    /// OpenF1's `circuit_short_name` for every 2024–2026 race session, with
    /// the lap count its own laps feed showed (2024 and 2025 were exact).
    #[test]
    fn every_openf1_circuit_resolves() {
        let openf1 = [
            ("Sakhir", 57), ("Jeddah", 50), ("Melbourne", 58), ("Suzuka", 53), ("Shanghai", 56),
            ("Miami", 57), ("Imola", 63), ("Monte Carlo", 78), ("Montreal", 70), ("Catalunya", 66),
            ("Spielberg", 71), ("Silverstone", 52), ("Hungaroring", 70), ("Spa-Francorchamps", 44),
            ("Zandvoort", 72), ("Monza", 53), ("Baku", 51), ("Singapore", 62), ("Austin", 56),
            ("Mexico City", 71), ("Interlagos", 71), ("Las Vegas", 50), ("Lusail", 57),
            ("Yas Marina Circuit", 58), ("Madring", 57), ("Kuala Lumpur", 56),
        ];
        for (name, laps) in openf1 {
            assert_eq!(total_laps(name, false), Some(laps), "{name}");
        }
    }

    /// Sprint distances as OpenF1 recorded them (2024–2026), and None where
    /// a circuit has never held one.
    #[test]
    fn sprints_resolve_only_where_they_have_run() {
        for (name, laps) in [
            ("Shanghai", 19), ("Miami", 19), ("Montreal", 23), ("Silverstone", 17), ("Spa-Francorchamps", 15),
            ("Zandvoort", 24), ("Baku", 17), ("Austin", 19), ("Interlagos", 24), ("Lusail", 19),
        ] {
            assert_eq!(total_laps(name, true), Some(laps), "{name} sprint");
        }
        assert_eq!(total_laps("Circuit de Monaco", true), None, "no sprint at Monaco");
        assert_eq!(total_laps("Madring", true), None);
    }

    #[test]
    fn matching_ignores_case_punctuation_and_accents() {
        assert_eq!(total_laps("circuit gilles villeneuve", false), Some(70));
        assert_eq!(total_laps("AUTÓDROMO JOSÉ CARLOS PACE", false), Some(71));
        assert_eq!(total_laps("yas marina circuit", false), Some(58));
    }

    #[test]
    fn unknown_or_empty_circuit_draws_no_bar() {
        assert_eq!(total_laps("", false), None);
        assert_eq!(total_laps("Brands Hatch", false), None);
    }
}

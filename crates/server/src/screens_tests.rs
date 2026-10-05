//! MAR-117: a board's screens as a list of instances — server tests.
//!
//! Kept beside `main.rs`'s own tests rather than inside them so the whole
//! feature's server contract reads in one place.

use super::*;
use crate::tests::watch_all;
use crate::ws::{apply_command, apply_settings_import, param_descriptor};
use std::collections::BTreeMap;

const FROZEN: (u32, u32) = (14, 37);
const ZIP: &str = "60601";

/// Pin every render on this thread to one wall time, so two renders (or a
/// render and an expectation) can be compared frame id for frame id.
fn freeze_at(hm: (u32, u32)) {
    offmode::TEST_NOW_HM.with(|c| c.set(Some(hm)));
}

fn sc(id: &str, kind: &str, params: &[(&str, &str)], idle: bool) -> ScreenConfig {
    ScreenConfig {
        id: id.into(),
        kind: kind.into(),
        params: params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        idle,
    }
}

fn params(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
    kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// A configured, gameless board with a location, holding `list`.
fn edited(list: Vec<ScreenConfig>) -> BoardConfig {
    let mut cfg = watch_all();
    cfg.off_mode_params.insert(offmode::LOCATION.into(), ZIP.into());
    assert!(cfg.set_screens(list, &offmode::off_modes()));
    cfg
}

/// A state with no games, the weather for `ZIP` fetched, and `cfg` loaded as
/// a deploy loads it.
fn state_with(cfg: &BoardConfig, games: bool) -> AppState {
    let state = AppState::new_ephemeral();
    if !games {
        state.games.lock().unwrap().clear();
    }
    state.weather.seed(ZIP, weather::Weather::sample());
    let mut configs = HashMap::from([("b".to_string(), cfg.clone())]);
    sanitize_off_modes(&mut configs);
    sanitize_screens(&mut configs);
    *state.configs.lock().unwrap() = configs;
    state
}

/// Render `kind` the way the pre-list server did: by id, with `params`.
fn render(state: &AppState, cfg: &BoardConfig, kind: &str, params: &BTreeMap<String, String>) -> Vec<offmode::OffFrame> {
    offmode::render_off_mode(
        kind,
        &offmode::OffCtx {
            tz: board_tz(cfg),
            params,
            preview: false,
            weather: state.weather.get(params.get(offmode::LOCATION).map(String::as_str).unwrap_or("")),
        },
    )
}

fn ids(m: &Manifest) -> Vec<String> {
    m.rotation.iter().map(|r| r.frame_id.clone()).collect()
}

fn notice_lines(state: &AppState, board: &str) -> Vec<String> {
    state.notices.lock().unwrap().get(board).map(|n| n.lines.iter().map(|(t, ..)| t.clone()).collect()).unwrap_or_default()
}

// ---- the deploy-day invariant ----

/// A board that never saved a list gets EXACTLY the manifest it got before
/// lists existed, but for the two differences chosen on purpose: the knob
/// reads SPORTS first and OFF second (§0 Q2), and a stored mode "blank"
/// loads as "off" (it was already dark; now the board says so).
///
/// The expectations are built the way the pre-list server built them —
/// `render_off_mode(id, flat params)`, dwell 60 s, epoch 0 — so a change to
/// any shared renderer cannot make this pass by moving both sides at once
/// unless it also changed what that server would have sent. Verified once
/// against manifests captured from the pre-change binary (all ten identical
/// apart from the two differences above).
#[test]
fn an_unsaved_board_keeps_todays_manifest() {
    freeze_at(FROZEN);
    let knob = vec![
        ModeEntry { id: "scores".into(), label: "SPORTS".into() },
        ModeEntry { id: "off".into(), label: "OFF".into() },
        ModeEntry { id: "clock".into(), label: "CLOCK".into() },
        ModeEntry { id: "weather".into(), label: "WEATHER".into() },
    ];
    let lit = |state: &AppState, cfg: &BoardConfig, kind: &str, mode: &str| {
        let rotation: Vec<RotationEntry> = render(state, cfg, kind, &cfg.off_mode_params)
            .into_iter()
            .map(|f| RotationEntry { frame_id: f.id, dwell_ms: 60_000, scroll: false, pin_rows: 0 })
            .collect();
        assert_eq!(rotation.len(), 1, "{kind}: one frame");
        Manifest {
            mode: mode.into(),
            modes: knob.clone(),
            local_clock: false,
            clock_hour24: cfg.off_mode_params.get("hour24").map(String::as_str) == Some("1"),
            display_on: true,
            off_state: OffState::Clock,
            rotation,
            brightness: cfg.brightness,
            epoch: 0,
            countdown_ms: None,
        }
    };
    let dark = |cfg: &BoardConfig, mode: &str| Manifest {
        mode: mode.into(),
        modes: knob.clone(),
        local_clock: false,
        clock_hour24: false,
        display_on: false,
        off_state: OffState::Blank,
        rotation: Vec::new(),
        brightness: cfg.brightness,
        epoch: 0,
        countdown_ms: None,
    };
    let check = |name: &str, cfg: BoardConfig, games: bool, expect: &dyn Fn(&AppState, &BoardConfig) -> Manifest| {
        let state = state_with(&cfg, games);
        let got = manifest_for_board(&state, "b");
        let want = expect(&state, &cfg);
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            serde_json::to_value(&want).unwrap(),
            "{name}: an unsaved board's manifest changed"
        );
        assert!(state.config_for("b").screens.is_empty(), "{name}: nothing materializes a list");
    };

    check("default", watch_all(), false, &|s, c| lit(s, c, "clock", "scores"));

    let mut b = watch_all();
    b.set_off_mode("weather", false);
    b.off_mode_params = params(&[("variant", "forecast"), ("location", ZIP)]);
    check("idle forecast", b, false, &|s, c| lit(s, c, "weather", "scores"));

    let mut c = watch_all();
    c.set_off_mode("blank", true);
    check("idle blank", c, false, &|_, c| dark(c, "scores"));

    let mut d = watch_all();
    d.set_mode("weather");
    d.off_mode_params = params(&[("variant", "detailed"), ("location", ZIP)]);
    check("held weather", d, false, &|s, c| lit(s, c, "weather", "weather"));

    let mut e = watch_all();
    e.set_mode("clock");
    e.off_mode_params = params(&[("style", "word")]);
    check("held word clock", e, false, &|s, c| lit(s, c, "clock", "clock"));

    let mut h24 = watch_all();
    h24.off_mode_params = params(&[("style", "compact"), ("hour24", "1")]);
    check("24-hour idle clock", h24, false, &|s, c| lit(s, c, "clock", "scores"));

    let mut f = watch_all();
    f.set_mode("off");
    check("off", f, false, &|_, c| dark(c, "off"));

    let mut g = watch_all();
    g.quiet_hours = Some(crate::tests::window_around_now(&g));
    check("quiet", g, false, &|_, c| dark(c, "scores"));

    check("unconfigured", BoardConfig::default(), false, &|_, c| {
        let (id, _) = render::render_notice(&SETUP_LINES, None);
        Manifest {
            rotation: vec![RotationEntry { frame_id: id, dwell_ms: 60_000, scroll: false, pin_rows: 0 }],
            display_on: true,
            off_state: OffState::Clock,
            ..dark(c, "scores")
        }
    });

    // A stored "blank" mode: dark before, dark after — now named "off".
    let mut j = watch_all();
    j.mode = "blank".into();
    j.power = true;
    j.wake_at = Some(unix_now() + 3600);
    check("held blank", j.clone(), false, &|_, c| dark(c, "off"));
    let loaded = state_with(&j, false).config_for("b");
    assert_eq!((loaded.mode.as_str(), loaded.power, loaded.wake_at), ("off", false, j.wake_at), "dark stays dark, wake kept");

    // Games: the games path is untouched, and still epoch 0.
    let state = state_with(&watch_all(), true);
    let m = manifest_for_board(&state, "b");
    let chosen = chosen_games(&state, &state.config_for("b"));
    assert!(!chosen.is_empty());
    assert_eq!(m.rotation.len(), chosen.len());
    assert_eq!((m.epoch, m.display_on, m.mode.as_str()), (0, true, "scores"));
    assert_eq!(m.modes, knob);
}

// ---- the render seam ----

/// Every instance renders as its KIND with its own layered params — never
/// as its id, which `render_off_mode` would silently draw as a clock.
#[test]
fn an_instance_renders_its_kind_and_its_own_layout() {
    freeze_at(FROZEN);
    let cfg = edited(vec![
        sc("weather", "weather", &[("variant", "forecast")], true),
        sc("weather.2", "weather", &[], false),
        sc("weather.3", "weather", &[("variant", "simple")], false),
        sc("clock.2", "clock", &[("style", "word")], false),
    ]);
    // The flat mirror carries the FIRST idle weather's layout…
    assert_eq!(cfg.off_mode_params.get("variant").map(String::as_str), Some("forecast"));
    let state = state_with(&cfg, false);
    let clock = render(&state, &cfg, "clock", &BTreeMap::new())[0].id.clone();
    for (id, kind, own) in [
        ("weather", "weather", vec![("variant", "forecast")]),
        // …which a default-layout sibling must NOT inherit.
        ("weather.2", "weather", vec![]),
        ("weather.3", "weather", vec![("variant", "simple")]),
        ("clock.2", "clock", vec![("style", "word")]),
    ] {
        handle_select_mode(&state, "b", id);
        assert_eq!(state.config_for("b").held_screen_id(), id);
        let m = manifest_for_board(&state, "b");
        let mut p = params(&[("location", ZIP)]);
        p.extend(params(&own));
        let want = render(&state, &cfg, kind, &p)[0].id.clone();
        assert_eq!(ids(&m), [want.clone()], "{id} renders as {kind} with its own params");
        if kind == "weather" {
            assert_ne!(want, clock, "{id} must not fall back to the clock");
        }
    }
    let detailed = render(&state, &cfg, "weather", &params(&[("location", ZIP), ("variant", "detailed")]))[0].id.clone();
    handle_select_mode(&state, "b", "weather.2");
    assert_eq!(ids(&manifest_for_board(&state, "b")), [detailed], "an unset layout is the default, not a sibling's");
}

// ---- the idle rotation ----

#[test]
fn two_checked_screens_rotate_on_the_board_clock() {
    freeze_at(FROZEN);
    let mut cfg = edited(vec![
        sc("weather", "weather", &[("variant", "forecast")], true),
        sc("clock", "clock", &[("style", "large")], true),
        sc("weather.2", "weather", &[("variant", "simple")], true),
        sc("clock.2", "clock", &[], false),
    ]);
    cfg.idle_cycle_secs = Some(600);
    cfg.off_mode_params.insert("hour24".into(), "1".into());
    let state = state_with(&cfg, false);
    let m = manifest_for_board(&state, "b");
    assert_eq!(m.rotation.len(), 3, "one entry per checked screen");
    for (e, (kind, own)) in m.rotation.iter().zip([
        ("weather", params(&[("variant", "forecast")])),
        ("clock", params(&[("style", "large")])),
        ("weather", params(&[("variant", "simple")])),
    ]) {
        let mut p = cfg.off_mode_params.clone();
        p.extend(own);
        assert_eq!(e.frame_id, render(&state, &cfg, kind, &p)[0].id);
        assert_eq!((e.dwell_ms, e.scroll, e.pin_rows), (600_000, false, 0));
    }
    assert_eq!(m.epoch, idle_epoch(&["weather", "clock", "weather.2"], 600_000));
    assert_ne!(m.epoch, 0);
    assert!(m.display_on && !m.local_clock && m.clock_hour24 && m.countdown_ms.is_none());
    assert_eq!(m.off_state, OffState::Clock);
    assert_eq!(m.mode, "scores");

    // A minute later: new clock frame, same program.
    freeze_at((FROZEN.0, FROZEN.1 + 1));
    let later = manifest_for_board(&state, "b");
    assert_ne!(ids(&later), ids(&m), "the clock re-rendered");
    assert_eq!(later.epoch, m.epoch, "a minute re-push keeps the board's slot and timer");

    let epoch_after = |cmd: Command| {
        let st = state_with(&cfg, false);
        apply_command(&st, "b", cmd);
        manifest_for_board(&st, "b").epoch
    };
    let mut reordered = cfg.screens.clone();
    reordered.swap(2, 3);
    assert_ne!(epoch_after(Command::SetScreens { screens: reordered }), m.epoch, "reorder restarts");
    let mut fewer = cfg.screens.clone();
    fewer[4].idle = false;
    assert_ne!(epoch_after(Command::SetScreens { screens: fewer }), m.epoch, "membership restarts");
    assert_ne!(epoch_after(Command::SetIdleCycle { secs: 120 }), m.epoch, "the timer restarts");
    let mut relaid = cfg.screens.clone();
    relaid[2].params.insert("variant".into(), "detailed".into());
    assert_eq!(epoch_after(Command::SetScreens { screens: relaid }), m.epoch, "a layout edit does not");
    assert_eq!(
        epoch_after(Command::SetOffModeParam { key: "hour24".into(), value: "0".into() }),
        m.epoch,
        "nor does a board-wide edit"
    );
}

/// One checked screen on a SAVED list is still today's single-screen
/// manifest; none is dark.
#[test]
fn one_checked_screen_is_todays_manifest_and_none_is_dark() {
    freeze_at(FROZEN);
    let cfg = edited(vec![sc("clock", "clock", &[("style", "word")], true), sc("weather", "weather", &[], false)]);
    let state = state_with(&cfg, false);
    let m = manifest_for_board(&state, "b");
    let want = render(&state, &cfg, "clock", &params(&[("style", "word")]))[0].id.clone();
    assert_eq!(ids(&m), [want]);
    assert_eq!((m.epoch, m.rotation[0].dwell_ms), (0, 60_000));

    let none = edited(vec![sc("clock", "clock", &[], false)]);
    assert_eq!(none.off_mode_id(), "blank");
    let m = manifest_for_board(&state_with(&none, false), "b");
    assert!(!m.display_on && m.rotation.is_empty());
    assert_eq!(m.off_state, OffState::Blank);
}

#[test]
fn the_idle_handoff_names_the_first_checked_screens_kind() {
    let state = state_with(&watch_all(), false);
    for (list, line) in [
        (vec![sc("weather.2", "weather", &[], true), sc("clock", "clock", &[], true)], "WEATHER IN"),
        (vec![sc("clock.2", "clock", &[], true), sc("weather", "weather", &[], true)], "CLOCK IN"),
        (vec![sc("clock", "clock", &[], false)], "SCREEN OFF IN"),
    ] {
        apply_command(&state, "b", Command::SetScreens { screens: list });
        apply_command(&state, "b", Command::SetMode { mode: "off".into() });
        apply_command(&state, "b", Command::SetMode { mode: "scores".into() });
        assert_eq!(notice_lines(&state, "b"), ["NO GAMES", line, "2 MINUTES"]);
    }
}

// ---- the knob ----

#[test]
fn knob_labels_are_unique_and_fit_with_repeated_kinds() {
    let cfg = edited(vec![
        sc("weather", "weather", &[("variant", "simple")], true),
        sc("weather.2", "weather", &[("variant", "clock")], false),
        sc("weather.3", "weather", &[("variant", "clock")], false),
        sc("weather.4", "weather", &[], false),
        sc("clock", "clock", &[("style", "word")], false),
        sc("clock.2", "clock", &[], false),
        sc("clock.3", "clock", &[("style", "large")], false),
        sc("clock.4", "clock", &[("style", "large")], false),
    ]);
    let modes = pickable_modes(&cfg);
    assert_eq!(modes.len(), MAX_SCREENS);
    let labels: Vec<&str> = modes.iter().map(|m| m.label.as_str()).collect();
    for l in &labels {
        assert!(l.len() <= BOARD_LABEL_MAX && l.is_ascii() && !l.is_empty(), "{l:?}");
    }
    let mut unique = labels.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), labels.len(), "{labels:?}");
    assert_eq!(&labels[..3], ["SPORTS", "OFF", "BIG WEATHER"]);
}

// ---- picks ----

#[test]
fn picks_resolve_against_the_boards_list() {
    let cfg = edited(vec![
        sc("weather", "weather", &[], true),
        sc("weather.2", "weather", &[("variant", "forecast")], false),
        sc("clock.2", "clock", &[("style", "word")], false),
    ]);
    let state = state_with(&cfg, false);
    let held = |s: &AppState| s.config_for("b").held_screen_id().to_string();
    let screens = state.config_for("b").screens.clone();

    // Knob and app share the resolver.
    for via_app in [false, true] {
        let pick = |id: &str| {
            if via_app {
                apply_command(&state, "b", Command::SetMode { mode: id.into() });
            } else {
                handle_select_mode(&state, "b", id);
            }
        };
        pick("weather.2");
        assert_eq!(held(&state), "weather.2", "an exact instance");
        assert_eq!(state.config_for("b").mode, "weather", "mode keeps the kind for old apps");
        pick("weather-forecast");
        assert_eq!(held(&state), "weather.2", "a legacy weather-x finds the instance showing x");
        pick("clock");
        assert_eq!(held(&state), "clock.2", "a kind finds its first instance");
        pick("weather");
        assert_eq!(held(&state), "weather");
        pick("blank");
        assert_eq!(state.config_for("b").mode, "off", "blank is off");
        pick("weather");
        pick("stocks");
        assert_eq!(held(&state), "weather", "junk is ignored");
        pick("weather.7");
        assert_eq!(held(&state), "weather", "a removed screen is ignored, not guessed");
        assert_eq!(state.config_for("b").screens, screens, "a pick never edits a screen");
    }

    // The instance's own timer, else its kind's, else sleep_hours.
    apply_command(&state, "b", Command::SetModeHours { mode: "weather".into(), hours: 2 });
    apply_command(&state, "b", Command::SetModeHours { mode: "weather.2".into(), hours: 5 });
    let hours_left = |s: &AppState| (s.config_for("b").wake_at.unwrap() as i64 - unix_now() as i64 + 1800) / 3600;
    handle_select_mode(&state, "b", "weather.2");
    assert_eq!(hours_left(&state), 5);
    handle_select_mode(&state, "b", "weather");
    assert_eq!(hours_left(&state), 2);
    handle_select_mode(&state, "b", "clock.2");
    assert_eq!(hours_left(&state), state.config_for("b").sleep_hours as i64);

    // A kind the user removed is ignored.
    apply_command(&state, "b", Command::SetScreens { screens: vec![sc("weather", "weather", &[], true)] });
    handle_select_mode(&state, "b", "weather");
    handle_select_mode(&state, "b", "clock");
    assert_eq!(held(&state), "weather");
}

/// An unsaved board's picks run the pre-list path, migration included.
#[test]
fn an_unsaved_boards_picks_run_the_old_path() {
    let state = state_with(&watch_all(), false);
    handle_select_mode(&state, "b", "weather-forecast");
    let cfg = state.config_for("b");
    assert_eq!((cfg.mode.as_str(), cfg.mode_screen.as_deref()), ("weather", None));
    assert_eq!(cfg.off_mode_params.get("variant").map(String::as_str), Some("forecast"));
    assert!(cfg.screens.is_empty());
    apply_command(&state, "b", Command::SetMode { mode: "blank".into() });
    assert_eq!(state.config_for("b").mode, "off");
}

#[test]
fn set_mode_hours_names_something_that_exists() {
    let state = state_with(&edited(vec![sc("weather.2", "weather", &[], true)]), false);
    for key in ["weather.2", "weather", "clock", "off", "stocks", "weather.9", "scores", "blank"] {
        apply_command(&state, "b", Command::SetModeHours { mode: key.into(), hours: 3 });
    }
    let keys: Vec<String> = state.config_for("b").mode_hours.keys().cloned().collect();
    assert_eq!(keys, ["clock", "off", "weather", "weather.2"]);
}

// ---- releases ----

/// Removing the held screen is not a gesture: back to Sports, with no quiet
/// override, so a board inside its quiet window stays dark.
#[test]
fn removing_the_held_screen_in_quiet_hours_stays_dark() {
    let mut cfg = edited(vec![sc("weather", "weather", &[], true), sc("weather.2", "weather", &[], false)]);
    cfg.quiet_hours = Some(crate::tests::window_around_now(&cfg));
    cfg.set_held_screen("weather.2", "weather");
    cfg.mode_hours = BTreeMap::from([("weather.2".into(), 4), ("weather".into(), 2)]);
    let state = state_with(&cfg, false);
    assert!(!manifest_for_board(&state, "b").display_on, "precondition: quiet");
    apply_command(&state, "b", Command::SetScreens { screens: vec![sc("weather", "weather", &[], true)] });
    let after = state.config_for("b");
    assert_eq!((after.held_screen_id(), after.wake_at), ("scores", None));
    assert!(!after.quiet_override, "a removal is not 'answer me now'");
    assert!(!manifest_for_board(&state, "b").display_on, "the quiet board stays dark");
    assert_eq!(after.mode_hours, BTreeMap::from([("weather".into(), 2)]), "the removed instance's timer goes; the kind's stays");

    // Removing the FIRST instance while holding it releases too, even with a
    // sibling of its kind left (the hold must not drift to another layout).
    let mut cfg = edited(vec![sc("weather", "weather", &[], true), sc("weather.2", "weather", &[], false)]);
    cfg.set_held_screen("weather", "weather");
    let state = state_with(&cfg, false);
    apply_command(&state, "b", Command::SetScreens { screens: vec![sc("weather.2", "weather", &[], true)] });
    assert_eq!(state.config_for("b").held_screen_id(), "scores");
    assert_eq!(notice_lines(&state, "b"), ["NO GAMES", "WEATHER IN", "2 MINUTES"], "the handoff still runs");
}

/// Load: lists are normalized, a bare-kind hold (a rolled-back server's
/// write) is canonicalized, a vanished hold is released. Idempotent.
#[test]
fn load_sanitizes_saved_lists() {
    let mut bare = edited(vec![sc("weather.2", "weather", &[], true)]);
    bare.mode = "weather".into(); // as a rolled-back server writes a hold
    bare.wake_at = Some(unix_now() + 3600);
    let mut gone = edited(vec![sc("clock", "clock", &[], true)]);
    gone.set_held_screen("weather.3", "weather");
    let mut junk = watch_all();
    junk.screens = vec![sc("stocks", "stocks", &[], true), sc("weather", "weather", &[("bogus", "1")], false)];
    let mut configs = HashMap::from([("bare".to_string(), bare), ("gone".to_string(), gone), ("junk".to_string(), junk)]);
    sanitize_screens(&mut configs);
    assert_eq!(configs["bare"].held_screen_id(), "weather.2");
    assert_eq!(configs["bare"].mode, "weather");
    assert!(configs["bare"].wake_at.is_some(), "the hold keeps its timer");
    assert_eq!((configs["gone"].held_screen_id(), configs["gone"].wake_at), ("scores", None));
    let junk = &configs["junk"];
    let kinds: Vec<&str> = junk.screens.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(kinds, ["scores", "off", "weather"], "the unoffered screen and the undeclared param are dropped");
    assert!(junk.screens[2].params.is_empty());
    assert!(junk.screens[2].idle, "an idle board never goes dark because its idle screen was dropped");
    assert_eq!(junk.off_mode_id(), "weather");
    let once = configs.clone();
    sanitize_screens(&mut configs);
    for (k, v) in &once {
        assert_eq!(serde_json::to_string(v).unwrap(), serde_json::to_string(&configs[k]).unwrap(), "{k}: idempotent");
    }
}

// ---- old apps on both kinds of board ----

#[test]
fn legacy_idle_commands_on_an_unsaved_board_run_the_old_path() {
    let state = state_with(&watch_all(), false);
    apply_command(&state, "b", Command::SetOffMode { id: "weather-simple".into() });
    let cfg = state.config_for("b");
    assert_eq!(cfg.off_mode.as_deref(), Some("weather"));
    assert_eq!(cfg.off_mode_params.get("variant").map(String::as_str), Some("simple"));
    apply_command(&state, "b", Command::SetOffMode { id: "stocks".into() });
    assert_eq!(state.config_for("b").off_mode.as_deref(), Some("weather"), "unknown ignored");
    apply_command(&state, "b", Command::SetOffState { off_state: OffState::Blank });
    let cfg = state.config_for("b");
    assert_eq!((cfg.off_mode_id(), cfg.off_state), ("blank", OffState::Blank));
    apply_command(&state, "b", Command::SetOffState { off_state: OffState::Clock });
    assert_eq!(state.config_for("b").off_mode_id(), "clock");
    assert!(state.config_for("b").screens.is_empty(), "an old app never creates a list");
}

#[test]
fn legacy_idle_commands_on_a_saved_list_respect_the_rotation() {
    let list = vec![
        sc("weather", "weather", &[("variant", "simple")], true),
        sc("clock", "clock", &[], true),
        sc("weather.2", "weather", &[("variant", "forecast")], false),
    ];
    let state = state_with(&edited(list), false);
    let idle = |s: &AppState| -> Vec<String> {
        s.config_for("b").screens.iter().filter(|x| x.idle).map(|x| x.id.clone()).collect()
    };
    // Both shipped apps resend their selection on every tap.
    apply_command(&state, "b", Command::SetOffMode { id: "weather".into() });
    assert_eq!(idle(&state), ["weather", "clock"], "a re-tap of the first checked kind is a no-op");
    apply_command(&state, "b", Command::SetOffMode { id: "clock".into() });
    assert_eq!(idle(&state), ["clock"], "another kind becomes the one checked screen");
    assert_eq!(state.config_for("b").off_mode_id(), "clock");
    apply_command(&state, "b", Command::SetOffMode { id: "weather-forecast".into() });
    assert_eq!(idle(&state), ["weather"], "its first instance; the legacy layout writes nothing");
    assert_eq!(state.config_for("b").screens[2].params.get("variant").map(String::as_str), Some("simple"));
    apply_command(&state, "b", Command::SetOffState { off_state: OffState::Blank });
    assert!(idle(&state).is_empty());
    assert_eq!(state.config_for("b").off_mode_id(), "blank");
    apply_command(&state, "b", Command::SetOffState { off_state: OffState::Clock });
    assert_eq!(idle(&state), ["clock"]);
    apply_command(&state, "b", Command::SetScreens { screens: vec![sc("weather", "weather", &[], true)] });
    apply_command(&state, "b", Command::SetOffMode { id: "clock".into() });
    assert_eq!(idle(&state), ["weather"], "a removed kind is ignored");
    apply_command(&state, "b", Command::SetOffState { off_state: OffState::Clock });
    assert_eq!(idle(&state), ["weather"], "the legacy clock too");
    assert_eq!(state.config_for("b").screens.len(), 3, "an old app never adds a screen");
}

#[test]
fn set_off_mode_param_is_validated_by_whoever_declares_the_key() {
    // Unsaved board, idle CLOCK: a Weather location used to be rejected here.
    let state = state_with(&watch_all(), false);
    let flat = |s: &AppState, k: &str| s.config_for("b").off_mode_params.get(k).cloned();
    apply_command(&state, "b", Command::SetOffModeParam { key: "location".into(), value: ZIP.into() });
    assert_eq!(flat(&state, "location").as_deref(), Some(ZIP), "the live bug: accepted now");
    apply_command(&state, "b", Command::SetOffModeParam { key: "variant".into(), value: "forecast".into() });
    assert_eq!(flat(&state, "variant").as_deref(), Some("forecast"), "an unsaved board's layout is flat");
    apply_command(&state, "b", Command::SetOffModeParam { key: "variant".into(), value: "sideways".into() });
    assert_eq!(flat(&state, "variant").as_deref(), Some("forecast"), "an unoffered choice is rejected");
    apply_command(&state, "b", Command::SetOffModeParam { key: "nonsense".into(), value: "1".into() });
    assert_eq!(flat(&state, "nonsense"), None, "an undeclared key is rejected");
    apply_command(&state, "b", Command::SetOffModeParam { key: "location".into(), value: String::new() });
    assert_eq!(flat(&state, "location"), None, "empty removes");
    assert!(state.config_for("b").screens.is_empty());

    // Saved list: a layout goes to ONE screen — the first checked one that
    // has it — and the flat mirror follows; board-wide keys stay flat.
    let state = state_with(
        &edited(vec![
            sc("weather", "weather", &[], false),
            sc("clock", "clock", &[], true),
            sc("weather.2", "weather", &[("variant", "forecast")], true),
        ]),
        false,
    );
    apply_command(&state, "b", Command::SetOffModeParam { key: "variant".into(), value: "simple".into() });
    let cfg = state.config_for("b");
    assert_eq!(cfg.screens[4].params.get("variant").map(String::as_str), Some("simple"));
    assert!(cfg.screens[2].params.is_empty(), "the unchecked sibling is untouched");
    assert_eq!(cfg.off_mode_params.get("variant").map(String::as_str), Some("simple"), "mirror re-derived");
    apply_command(&state, "b", Command::SetOffModeParam { key: "style".into(), value: "word".into() });
    assert_eq!(state.config_for("b").screens[3].params.get("style").map(String::as_str), Some("word"));
    apply_command(&state, "b", Command::SetOffModeParam { key: "hour24".into(), value: "1".into() });
    assert_eq!(flat(&state, "hour24").as_deref(), Some("1"));
    assert!(state.config_for("b").screens.iter().all(|s| !s.params.contains_key("hour24")));
    // A choice has no "empty" option, so it is rejected exactly as before.
    apply_command(&state, "b", Command::SetOffModeParam { key: "variant".into(), value: String::new() });
    assert_eq!(state.config_for("b").screens[4].params.get("variant").map(String::as_str), Some("simple"));
    apply_command(&state, "b", Command::SetOffModeParam { key: "location".into(), value: String::new() });
    assert_eq!(flat(&state, "location"), None, "a board-wide empty value removes the flat key");
}

/// A fork's idle mode that shares a key with a stock mode (different choices)
/// is judged by ITS OWN field, as HEAD did — not by the first catalog entry
/// that happens to declare the key. A non-idle declarer still gets a fair
/// hearing: any offered mode that accepts the value admits it.
#[test]
fn set_off_mode_param_prefers_the_idle_modes_own_field() {
    let mut catalog = offmode::off_modes();
    let mut stocks = catalog.iter().find(|m| m.id == "clock").unwrap().clone();
    stocks.id = "stocks".into();
    let style = stocks.fields.iter_mut().find(|f| f.key == "style").unwrap();
    style.options.retain(|_| false);
    for v in ["ticker", "chart"] {
        style.options.push(OffModeChoice { value: v.into(), label: v.into(), preview: None, screen_label: None });
    }
    style.default = "ticker".into();
    catalog.push(stocks);
    let verdict = |idle: &str, key: &str, value: &str| {
        param_descriptor(&catalog, idle, key, value).map(|(d, r)| (d.id.clone(), r.is_ok()))
    };

    // Idle fork mode: its own choices, exactly the old path.
    assert_eq!(verdict("stocks", "style", "chart"), Some(("stocks".into(), true)));
    assert_eq!(verdict("stocks", "style", "word"), Some(("stocks".into(), false)), "the idle mode's field rules");
    // Idle stock clock: its own field, which knows nothing of "chart".
    assert_eq!(verdict("clock", "style", "word"), Some(("clock".into(), true)));
    assert_eq!(verdict("clock", "style", "chart"), Some(("clock".into(), false)));
    // Idle mode doesn't declare the key: any declarer that accepts it.
    assert_eq!(verdict("weather", "style", "chart"), Some(("stocks".into(), true)), "not just the first declarer");
    assert_eq!(verdict("weather", "style", "word"), Some(("clock".into(), true)));
    assert_eq!(verdict("clock", "location", ZIP), Some(("weather".into(), true)), "the live bug stays fixed");
    // Nobody accepts it: rejected with the first declarer's reason.
    assert_eq!(verdict("weather", "style", "sideways"), Some(("clock".into(), false)));
    assert_eq!(verdict("clock", "nonsense", "1"), None);
}

#[test]
fn set_idle_cycle_snaps_to_a_preset() {
    let state = state_with(&watch_all(), false);
    for (secs, want) in [(0, 120), (7, 120), (400, 300), (3600, 3600), (99_999, 3600)] {
        apply_command(&state, "b", Command::SetIdleCycle { secs });
        assert_eq!(state.config_for("b").idle_cycle_secs, Some(want));
    }
    assert!(state.config_for("b").screens.is_empty(), "no list materialized");
}

// ---- weather ----

#[test]
fn weather_is_fetched_for_checked_or_held_weather_screens_only() {
    let state = AppState::new_ephemeral();
    let add = |id: &str, cfg: BoardConfig| {
        state.configs.lock().unwrap().insert(id.into(), cfg);
    };
    let with_loc = |mut c: BoardConfig, loc: &str| {
        c.off_mode_params.insert("location".into(), loc.into());
        c
    };
    let mut idle_weather = BoardConfig::default();
    idle_weather.set_off_mode("weather", false);
    add("a", with_loc(idle_weather, "11111"));
    let mut held = BoardConfig::default();
    held.set_mode("weather");
    add("b", with_loc(held, "22222"));
    add("c", with_loc(BoardConfig::default(), "33333")); // idle clock: not needed
    add("d", edited(vec![sc("clock", "clock", &[], true), sc("weather", "weather", &[], false)])); // listed, unused
    add("e", with_loc(edited(vec![sc("clock", "clock", &[], true), sc("weather.2", "weather", &[], true)]), "44444"));
    let mut held2 = with_loc(edited(vec![sc("clock", "clock", &[], true), sc("weather.2", "weather", &[], false)]), "55555");
    held2.set_held_screen("weather.2", "weather");
    add("f", held2);
    let mut off = with_loc(BoardConfig::default(), "66666");
    off.set_off_mode("weather", false);
    off.off_mode_params.insert("location".into(), "  ".into());
    add("g", off);
    assert_eq!(weather_locations(&state), ["11111", "22222", "44444", "55555"]);
}

// ---- what apps see ----

#[test]
fn board_state_carries_the_resolved_list_but_never_stores_it() {
    let state = state_with(&watch_all(), false);
    let ServerMsg::BoardState { config, .. } = board_state_msg(&state, "b") else { panic!() };
    let ids: Vec<&str> = config.screens.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["scores", "off", "clock", "weather"]);
    assert!(config.screens[2].idle);
    assert!(state.config_for("b").screens.is_empty(), "the stored row is untouched");
    let stored = serde_json::to_string(&state.config_for("b")).unwrap();
    assert!(!stored.contains("\"screens\""), "and persists exactly as before");
}

// ---- rollback ----

/// A rolled-back server rewrites the row without the new fields. The twins
/// keep that graceful: the first checked screen as THE idle screen, its
/// layout, the real location, and the held screen as its kind. Rolling
/// forward re-synthesizes a list from them (lossy for duplicates, order and
/// a multi-screen rotation, by design).
#[test]
fn a_rollback_strips_the_list_and_the_twins_carry_the_board() {
    freeze_at(FROZEN);
    let mut cfg = edited(vec![
        sc("clock", "clock", &[("style", "word")], false),
        sc("weather.2", "weather", &[("variant", "forecast")], true),
        sc("clock.2", "clock", &[("style", "large")], true),
    ]);
    cfg.set_held_screen("weather.2", "weather");
    let mut row = serde_json::to_value(&cfg).unwrap();
    for k in ["screens", "idle_cycle_secs", "mode_screen"] {
        row.as_object_mut().unwrap().remove(k);
    }
    let stripped: BoardConfig = serde_json::from_value(row).unwrap();
    let state = state_with(&stripped, false);
    let back = state.config_for("b");
    assert_eq!(back.held_screen_id(), "weather", "the held screen survives as its kind");
    let m = manifest_for_board(&state, "b");
    assert_eq!(m.mode, "weather");
    let forecast = render(&state, &cfg, "weather", &params(&[("location", ZIP), ("variant", "forecast")]))[0].id.clone();
    assert_eq!(ids(&m), [forecast], "with the layout the first checked weather had");
    let ids: Vec<String> = back_screens(&back);
    assert_eq!(ids, ["scores", "off", "clock", "weather*"], "one per kind; the first checked one idle");
}

fn back_screens(cfg: &BoardConfig) -> Vec<String> {
    board_screens(cfg).iter().map(|s| if s.idle { format!("{}*", s.id) } else { s.id.clone() }).collect()
}

// ---- import ----

#[test]
fn an_old_export_onto_a_saved_list_hands_the_board_back_to_legacy_fields() {
    let mut cfg = edited(vec![sc("clock", "clock", &[], true), sc("weather.2", "weather", &[], false)]);
    cfg.set_held_screen("weather.2", "weather");
    let state = state_with(&cfg, false);
    let mut old = PortableSettings::from(&watch_all());
    old.off_mode = Some("weather-forecast".into());
    assert!(old.screens.is_empty());
    apply_settings_import(&state, "b", &serde_json::to_string(&old).unwrap()).unwrap();
    let after = state.config_for("b");
    assert!(after.screens.is_empty(), "the old blob clears the list");
    assert_eq!(after.off_mode_id(), "weather");
    assert_eq!(after.off_mode_params.get("variant").map(String::as_str), Some("forecast"));
    assert_eq!(after.held_screen_id(), "scores", "weather.2 no longer exists");
}

#[test]
fn a_new_export_with_a_list_is_normalized_and_rederives_the_twins() {
    let mut src = edited(vec![
        sc("weather.2", "weather", &[("variant", "forecast")], true),
        sc("clock", "clock", &[("style", "word")], true),
    ]);
    src.idle_cycle_secs = Some(900);
    let mut blob = PortableSettings::from(&src);
    // Hand-edited: twins that disagree with the list, a mode nothing offers.
    blob.off_mode = Some("clock".into());
    blob.off_mode_params.remove("variant");
    blob.screens.push(sc("stocks", "stocks", &[], true));
    let mut target = edited(vec![sc("weather", "weather", &[], true)]);
    target.set_held_screen("weather", "weather");
    let state = state_with(&target, false);
    apply_settings_import(&state, "b", &serde_json::to_string(&blob).unwrap()).unwrap();
    let after = state.config_for("b");
    let ids: Vec<&str> = after.screens.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["scores", "off", "weather.2", "clock"]);
    assert_eq!(after.off_mode_id(), "weather", "the first checked screen's kind");
    assert_eq!(after.off_mode_params.get("variant").map(String::as_str), Some("forecast"));
    assert_eq!(after.idle_cycle_secs, Some(900));
    assert_eq!(after.held_screen_id(), "scores", "the held 'weather' left with the import");
}

// ---- the board, end to end ----

/// The worst stock mix at the cap, driven through a real `board_core::Board`
/// over two full cycles with the server re-rendering every minute: every
/// slot shows ITS OWN current frame (never `last_good`, never the previous
/// screen), and the board never evicts a frame the rotation still needs.
///
/// This rotation does NOT bring the cache under eviction pressure — the
/// board's manifest retain keeps only the rotation plus `last_good`, so stale
/// minute frames never pile up. What keeps a full rotation safe is HEADROOM,
/// so that is what is pinned: the peak cached bytes, at every step, stay
/// within 3/4 of the board's 24 KiB budget (`MAX_SCREENS` is sized to 80%).
/// A frame-size regression fails here before it ever causes an eviction.
/// Victim choice UNDER pressure (unreferenced first, then least recently
/// shown, never the lit frame) is board-core's own test:
/// `the_cache_is_bounded_in_bytes_and_never_drops_the_on_screen_frame`.
#[test]
fn a_full_idle_rotation_survives_minute_re_pushes_on_a_real_board() {
    let cfg = {
        let mut c = edited(vec![
            sc("clock", "clock", &[("style", "word")], true),
            sc("weather", "weather", &[("variant", "forecast")], true),
            sc("weather.2", "weather", &[("variant", "detailed")], true),
            sc("weather.3", "weather", &[("variant", "clock")], true),
            sc("clock.2", "clock", &[("style", "large")], true),
            sc("clock.3", "clock", &[("style", "classic")], true),
            sc("clock.4", "clock", &[("style", "compact")], true),
            sc("weather.4", "weather", &[("variant", "simple")], true),
        ]);
        c.off_mode_params.insert("hour24".into(), "1".into());
        c.idle_cycle_secs = Some(120);
        c
    };
    assert_eq!(cfg.screens.len(), MAX_SCREENS, "at the cap");
    let state = state_with(&cfg, false);
    let dwell = cfg.idle_cycle_ms() as u64;
    let px_of = |id: &str| state.frames.lock().unwrap().get(id).map(|(_, p)| p.clone()).unwrap();

    // What the panel looks like showing just this frame.
    let mut reference: HashMap<String, Vec<u32>> = HashMap::new();
    let mut panel_for = |id: &str, brightness: u8| -> Vec<u32> {
        reference
            .entry(id.to_string())
            .or_insert_with(|| {
                let mut b = board_core::Board::new();
                b.gesture(board_core::Gesture::Long);
                b.setup_done();
                b.set_online(true);
                b.tick(0);
                b.on_manifest(Manifest {
                    rotation: vec![RotationEntry { frame_id: id.into(), dwell_ms: 60_000, scroll: false, pin_rows: 0 }],
                    display_on: true,
                    local_clock: false,
                    clock_hour24: false,
                    off_state: OffState::Clock,
                    brightness,
                    epoch: 0,
                    countdown_ms: None,
                    mode: String::new(),
                    modes: Vec::new(),
                });
                b.on_frame(id.into(), encode_frame_payload(&px_of(id), board_core::SUPPORTED_FRAME_CODECS));
                b.tick(1);
                b.framebuffer()
            })
            .clone()
    };

    let mut b = board_core::Board::new();
    b.gesture(board_core::Gesture::Long);
    b.setup_done();
    b.set_online(true);
    b.tick(0);
    let mut last_json = String::new();
    let mut current: Option<Manifest> = None;
    let mut minute = (9u32, 58u32); // through the top of an hour
    let mut pushes = 0;
    let mut seen_per_slot: Vec<HashSet<String>> = vec![HashSet::new(); 8];
    // Board-core's `CACHE_BUDGET_BYTES` is private; this is its value.
    const CACHE_BUDGET: usize = 24 * 1024;
    let mut peak_cache = 0usize;
    let total = 2 * 8 * dwell;
    let mut t = 0u64;
    while t < total {
        // The server's re-render: at start, then every minute between dwells.
        if t == 0 || t % 60_000 == 30_000 {
            if t != 0 {
                minute = if minute.1 == 59 { ((minute.0 + 1) % 24, 0) } else { (minute.0, minute.1 + 1) };
            }
            freeze_at(minute);
            let m = manifest_for_board(&state, "b");
            let json = serde_json::to_string(&m).unwrap();
            if json != last_json {
                last_json = json;
                pushes += 1;
                if let Some(ClientMsg::NeedFrames { ids, codecs }) = b.on_manifest(m.clone()) {
                    for id in ids {
                        b.on_frame(id.clone(), encode_frame_payload(&px_of(&id), &codecs));
                        peak_cache = peak_cache.max(b.cache_bytes());
                    }
                }
                // Probe: re-offering the same program must find every frame
                // still cached (a same-shape re-push keeps slot and timer).
                assert!(b.on_manifest(m.clone()).is_none(), "t={t}: the board evicted a current rotation frame");
                current = Some(m);
            }
        }
        b.tick(t);
        peak_cache = peak_cache.max(b.cache_bytes());
        let m = current.as_ref().unwrap();
        assert_eq!(m.rotation.len(), 8);
        let slot = ((t / dwell) % 8) as usize;
        let want = &m.rotation[slot].frame_id;
        seen_per_slot[slot].insert(want.clone());
        assert_eq!(
            b.framebuffer(),
            panel_for(want, m.brightness),
            "t={t}ms slot {slot}: the panel is not showing this slot's current frame"
        );
        if t % dwell == 0 {
            assert!(b.on_manifest(m.clone()).is_none(), "t={t}: a rotation frame was evicted");
        }
        t += 1000;
    }
    assert!(
        peak_cache <= CACHE_BUDGET * 3 / 4,
        "a full rotation must leave headroom: peak {peak_cache} B cached of a {CACHE_BUDGET} B budget"
    );
    assert!(pushes > 16, "the clock re-pushed every minute ({pushes})");
    assert!(seen_per_slot[3].len() > 1 && seen_per_slot[4].len() > 1, "clock slots saw fresh frames mid-dwell");
    let distinct: HashSet<&String> = current.as_ref().unwrap().rotation.iter().map(|e| &e.frame_id).collect();
    assert_eq!(distinct.len(), 8, "eight different screens");
}

// ---- the fork seam ----

#[test]
fn stock_modes_declare_their_screen_seams() {
    let modes = offmode::off_modes();
    let ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["blank", "clock", "weather"], "catalog order is unchanged");
    let weather = modes.iter().find(|m| m.id == "weather").unwrap();
    let clock = modes.iter().find(|m| m.id == "clock").unwrap();
    assert!(weather.per_screen("variant") && clock.per_screen("style"));
    assert!(!weather.per_screen("location") && !weather.per_screen("hour24") && !clock.per_screen("hour24"));
    for m in &modes {
        for f in m.fields.iter().filter(|f| f.per_screen && f.kind == "choice") {
            for o in &f.options {
                let l = o.screen_label.as_deref().unwrap_or_else(|| panic!("{}/{}: no screen_label", m.id, o.value));
                assert!(!l.is_empty() && l.chars().count() <= BOARD_LABEL_MAX, "{l:?}");
            }
        }
    }
    assert!(offmode::needs_location("weather"));
    assert!(!offmode::needs_location("clock") && !offmode::needs_location("blank"));
    assert!(!offmode::needs_location("weather.2"), "kinds, never instance ids");
}

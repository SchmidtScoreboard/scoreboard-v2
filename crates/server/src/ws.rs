//! Websocket handling. One endpoint; role chosen by the first `Auth` message.
//! Devices (board_id = their pubkey) get per-board manifests + binary frames.
//! Controllers fetch the catalog, subscribe to board state, and send commands
//! (which update that board's stored config and re-render it).

use crate::{board_state_msg, push_board, push_board_state, try_pair, AppState};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use scoreboard_protocol::*;
use tokio::sync::mpsc;

type WsSender = SplitSink<WebSocket, Message>;
type WsReceiver = SplitStream<WebSocket>;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Random per-connection challenge the client must sign with its Ed25519 key.
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let challenge = hex::encode(nonce);
    let hello = ServerMsg::Hello { challenge: challenge.clone() };
    if sender
        .send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .is_err()
    {
        return;
    }

    let (role, pubkey, sig) = loop {
        match receiver.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(ClientMsg::Auth { role, pubkey, sig }) =
                    serde_json::from_str::<ClientMsg>(&t)
                {
                    break (role, pubkey, sig);
                }
            }
            Some(Ok(_)) => continue,
            _ => return,
        }
    };

    // The pubkey is only a real identity once it has signed our challenge.
    if !verify_hex(&pubkey, challenge.as_bytes(), &sig) {
        tracing::warn!("auth rejected: bad signature for pubkey '{pubkey}'");
        return; // drop the connection
    }

    let _ = sender
        .send(Message::Text(serde_json::to_string(&ServerMsg::AuthOk).unwrap()))
        .await;

    match role {
        Role::Device => device_loop(sender, receiver, state, pubkey).await,
        Role::Controller => controller_loop(sender, receiver, state, pubkey).await,
    }
}

async fn device_loop(sender: WsSender, mut receiver: WsReceiver, state: AppState, board_id: String) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state.devices.lock().unwrap().insert(board_id.clone(), tx.clone());
    // A (re)connecting board has no manifest — drop the dedup entry so the
    // initial push below always goes out.
    state.forget_last_manifest(&board_id);
    state.poll_wake.notify_one(); // pull fresh games now, not at the next poll tick
    // Tell subscribed apps immediately that the board is online (they'd otherwise
    // wait for the next games refresh to learn it).
    push_board_state(&state, &board_id);
    tracing::info!("device '{board_id}' connected");

    let mut sink = sender;
    let forward = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    // Keepalive: quiet content (an overnight leaderboard) can mean minutes of
    // manifest silence, but the firmware aborts a socket that's idle for 45s
    // and prod's TCP proxy reaps idle flows — the connection would flap
    // connect/auth/drop forever. Two pings per timeout window keep it up.
    let keepalive = spawn_keepalive(tx.clone());

    // Initial per-board manifest.
    push_board(&state, &board_id);

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(t) = msg {
            match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::NeedFrames { ids }) => {
                    let mut out = Vec::new();
                    {
                        let frames = state.frames.lock().unwrap();
                        for fid in ids {
                            if let Some((_, px)) = frames.get(&fid) {
                                out.push((fid.clone(), px.clone()));
                            }
                        }
                    }
                    for (fid, px) in out {
                        let _ = tx.send(Message::Binary(encode_frame(&fid, &px)));
                    }
                }
                Ok(ClientMsg::EnterSetup { code }) => {
                    tracing::info!("device '{board_id}' entered setup (code {code})");
                    // Registering supersedes this board's previous code and
                    // expires stale ones — only what's on the panel pairs.
                    state.register_pairing_code(code, &board_id);
                }
                Ok(ClientMsg::ButtonPress) => {
                    crate::handle_button(&state, &board_id);
                }
                Ok(ClientMsg::DeviceBrightness { level }) => {
                    crate::handle_device_brightness(&state, &board_id, level);
                }
                Ok(ClientMsg::DeviceTimezone { tz }) => {
                    crate::handle_device_timezone(&state, &board_id, &tz);
                }
                Ok(ClientMsg::DeviceVersion { version }) => {
                    crate::handle_device_version(&state, &board_id, version);
                }
                Ok(ClientMsg::RequestUpdate) => {
                    // The on-device menu's UPDATE row was selected: run the
                    // exact pick+relay the app's Update button runs. Nothing
                    // newer (or no reported version) is a logged no-op in
                    // there — the board asked, there's just nothing to send.
                    crate::handle_update_firmware(&state, &board_id);
                }
                _ => {}
            }
        }
    }

    // Tear down only OUR registration. A board that reboots without closing the
    // socket (OTA reset, power blip) reconnects while this stale loop is still
    // parked on its dead receiver — when the keepalive finally errors it out,
    // an unconditional cleanup would clobber the NEW connection's entries and
    // leave a connected board invisible until its next reconnect.
    let ours = {
        let mut devices = state.devices.lock().unwrap();
        match devices.get(&board_id) {
            Some(current) if current.same_channel(&tx) => {
                devices.remove(&board_id);
                true
            }
            _ => false,
        }
    };
    if ours {
        // Drop the manifest-dedup entry too: with the board gone nothing can
        // request its frames, so the eviction pass may reclaim them.
        state.forget_last_manifest(&board_id);
        // An offline board has no CURRENT firmware version (the contract: None
        // while offline; devices re-report on every connect).
        state.device_versions.lock().unwrap().remove(&board_id);
    }
    keepalive.abort();
    forward.abort();
    if ours {
        // Mirror the connect path: apps should see "offline" now, not at the next poll.
        push_board_state(&state, &board_id);
    }
    tracing::info!("device '{board_id}' disconnected (stale: {})", !ours);
}

async fn controller_loop(
    sender: WsSender,
    mut receiver: WsReceiver,
    state: AppState,
    pubkey: String,
) {
    // Channel + forwarder so the poll loop (and commands) can push to this app
    // after it subscribes — mirrors how devices are wired.
    let (out, mut rx) = mpsc::unbounded_channel::<Message>();
    let mut sink = sender;
    let forward = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    // Same idle-flow reaping applies to apps sitting on a quiet screen.
    let keepalive = spawn_keepalive(out.clone());
    let mut subscribed: Vec<String> = Vec::new();
    state.poll_wake.notify_one(); // fetch fresh games now (covers browse before subscribe)

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(t) = msg {
            match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::Pair { code }) => {
                    // Throttle pairing-code guesses — wrong guesses are otherwise free,
                    // so the 6-digit code would be brute-forceable. Budgeted per
                    // controller identity: one guesser must not lock out real phones.
                    if !state.allow_pair_attempt(&pubkey) {
                        let _ = out.send(Message::Text(
                            serde_json::to_string(&ServerMsg::PairFailed {
                                reason: "too many attempts; wait a moment".into(),
                            })
                            .unwrap(),
                        ));
                        continue;
                    }
                    let reply = match try_pair(&state, &code, &pubkey) {
                        Ok(board_id) => {
                            tracing::info!("controller '{pubkey}' paired with '{board_id}'");
                            // tell the device it's paired so it leaves setup, then refresh it
                            if let Some(dtx) = state.devices.lock().unwrap().get(&board_id) {
                                let _ = dtx.send(Message::Text(
                                    serde_json::to_string(&ServerMsg::SetupDone).unwrap(),
                                ));
                            }
                            push_board(&state, &board_id);
                            ServerMsg::Paired { board_id }
                        }
                        Err(reason) => ServerMsg::PairFailed { reason },
                    };
                    let _ = out.send(Message::Text(serde_json::to_string(&reply).unwrap()));
                }
                Ok(ClientMsg::Unpair { board_id }) => {
                    // A controller may only revoke ITS OWN grant: `pubkey` comes
                    // from the signed auth handshake, never from the message, so
                    // no app can unpair someone else's phone. Unpairing a board
                    // you aren't paired with is a silent no-op.
                    if state.unpair(&board_id, &pubkey) {
                        tracing::info!("controller '{pubkey}' unpaired from '{board_id}'");
                    } else {
                        tracing::warn!("controller '{pubkey}' asked to unpair '{board_id}': no such pairing");
                    }
                    // Drop the subscription in the same breath — a revoked grant
                    // that kept receiving board state would resurrect the board
                    // the user just removed.
                    {
                        let mut controllers = state.controllers.lock().unwrap();
                        if let Some(subs) = controllers.get_mut(&board_id) {
                            subs.retain(|s| !s.same_channel(&out));
                            if subs.is_empty() {
                                controllers.remove(&board_id);
                            }
                        }
                    }
                    subscribed.retain(|b| b != &board_id);
                }
                Ok(ClientMsg::Subscribe { board_ids }) => {
                    // Subscribe REPLACES this connection's set: purge our sender
                    // from every board first, so re-subscribing with fewer boards
                    // (the app removed one) actually stops its pushes.
                    {
                        let mut controllers = state.controllers.lock().unwrap();
                        for subs in controllers.values_mut() {
                            subs.retain(|tx| !tx.same_channel(&out));
                        }
                        controllers.retain(|_, subs| !subs.is_empty());
                    }
                    // A controller can watch several boards at once; subscribe to each
                    // it's paired with and push that board's current state.
                    for bid in &board_ids {
                        if state.is_allowed(bid, &pubkey) {
                            tracing::info!("controller '{pubkey}' subscribed to {bid}");
                            state
                                .controllers
                                .lock()
                                .unwrap()
                                .entry(bid.clone())
                                .or_default()
                                .push(out.clone());
                            subscribed.push(bid.clone());
                            let _ = out.send(Message::Text(
                                serde_json::to_string(&board_state_msg(&state, bid)).unwrap(),
                            ));
                        } else {
                            tracing::warn!("controller '{pubkey}' denied subscribe to '{bid}' (not paired)");
                        }
                    }
                }
                Ok(ClientMsg::GetCatalog) => {
                    tracing::info!("controller requested catalog");
                    let cat = ServerMsg::Catalog((*state.catalog).clone());
                    let _ = out.send(Message::Text(serde_json::to_string(&cat).unwrap()));
                }
                Ok(ClientMsg::GetGames { board_id }) => {
                    // Board-scoped so spoiler rules sanitize the browse list too
                    // — otherwise All Games would leak the very scores the
                    // rotation hides. (No board = default config: server-local
                    // times, default leaderboard depth.)
                    let cfg = board_id
                        .as_deref()
                        .map(|b| state.config_for(b))
                        .unwrap_or_default();
                    let tz = crate::board_tz(&cfg);
                    // Only a board-scoped browse can say what's coming to THAT
                    // panel; an unscoped one leaves `queued` false throughout.
                    let prefs = board_id.as_deref().map(|_| crate::board_prefs(&cfg));
                    let games: Vec<GameSummary> = {
                        let g = state.games.lock().unwrap();
                        g.iter()
                            .filter(|g| crate::browse_shows_now(&state, g))
                            .map(|g| {
                                let spoiler = crate::spoiler_for(&cfg, g);
                                let mut s = crate::to_summary_for(g, spoiler, tz);
                                s.scrolls = crate::game_scroll_capable(&cfg, g);
                                s.queued = prefs
                                    .as_ref()
                                    .is_some_and(|p| crate::game_queued(&state, &cfg, p, g));
                                s
                            })
                            .collect()
                    };
                    let _ = out.send(Message::Text(
                        serde_json::to_string(&ServerMsg::Games { games }).unwrap(),
                    ));
                }
                Ok(ClientMsg::ListFirmware) => {
                    // The DEBUG picker's full menu: one entry per channel the
                    // server hosts a dist for, in stable-first order. Not
                    // board-scoped — the on-device same-or-newer guard decides
                    // what any given board actually takes.
                    let images: Vec<FwImage> = FwChannel::ALL
                        .iter()
                        .filter_map(|c| {
                            state.firmware.get(c).map(|fw| FwImage {
                                channel: c.as_str().to_string(),
                                version: fw.manifest.version.clone(),
                            })
                        })
                        .collect();
                    let _ = out.send(Message::Text(
                        serde_json::to_string(&ServerMsg::FirmwareList { images }).unwrap(),
                    ));
                }
                Ok(ClientMsg::ExportSettings { board_id }) => {
                    // Same per-board guard as commands: only a paired controller
                    // may read a board's preferences. Answer with an error (via
                    // ImportResult) instead of going silent — a stale pairing
                    // (server DB reset) would otherwise be a dead button.
                    if !state.is_allowed(&board_id, &pubkey) {
                        tracing::warn!("controller '{pubkey}' denied settings export for '{board_id}' (not paired)");
                        let _ = out.send(Message::Text(
                            serde_json::to_string(&ServerMsg::ImportResult {
                                board_id,
                                error: Some("this server doesn't recognize the pairing — re-pair the board".into()),
                            })
                            .unwrap(),
                        ));
                        continue;
                    }
                    let settings = PortableSettings::from(&state.config_for(&board_id));
                    let json = serde_json::to_string_pretty(&settings).unwrap();
                    let _ = out.send(Message::Text(
                        serde_json::to_string(&ServerMsg::SettingsExport { board_id, json }).unwrap(),
                    ));
                }
                Ok(ClientMsg::ImportSettings { board_id, json }) => {
                    let error = if !state.is_allowed(&board_id, &pubkey) {
                        Some("not paired with this board".to_string())
                    } else {
                        apply_settings_import(&state, &board_id, &json).err()
                    };
                    let ok = error.is_none();
                    let _ = out.send(Message::Text(
                        serde_json::to_string(&ServerMsg::ImportResult { board_id: board_id.clone(), error })
                            .unwrap(),
                    ));
                    if ok {
                        // Reflect the updated config back to the app (mirrors Command).
                        let _ = out.send(Message::Text(
                            serde_json::to_string(&board_state_msg(&state, &board_id)).unwrap(),
                        ));
                    }
                }
                Ok(ClientMsg::Command { req_id, board_id, cmd }) => {
                    if !state.is_allowed(&board_id, &pubkey) {
                        let ack = ServerMsg::CommandAck {
                            req_id,
                            ok: false,
                            error: Some("not paired with this board".into()),
                        };
                        let _ = out.send(Message::Text(serde_json::to_string(&ack).unwrap()));
                    } else {
                        apply_command(&state, &board_id, cmd);
                        let _ = out.send(Message::Text(
                            serde_json::to_string(&ServerMsg::CommandAck { req_id, ok: true, error: None })
                                .unwrap(),
                        ));
                        // Reflect the updated config back to the app.
                        let _ = out.send(Message::Text(
                            serde_json::to_string(&board_state_msg(&state, &board_id)).unwrap(),
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    // Drop this controller from the registry so the poll loop stops pushing to it.
    {
        let mut controllers = state.controllers.lock().unwrap();
        for bid in &subscribed {
            if let Some(subs) = controllers.get_mut(bid) {
                subs.retain(|s| !s.same_channel(&out));
                if subs.is_empty() {
                    controllers.remove(bid);
                }
            }
        }
    }
    keepalive.abort();
    forward.abort();
    tracing::info!("controller '{pubkey}' disconnected");
}

/// Ws ping every 20s through the connection's outbox channel. Content pushes
/// are the only other traffic, and a quiet slate can go minutes without one —
/// while the firmware's 45s read timeout and the TCP proxy's idle reaper both
/// need SOMETHING on the wire. Devices answer with pongs; so do phones.
fn spawn_keepalive(tx: mpsc::UnboundedSender<Message>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
        tick.tick().await; // the first tick fires immediately; skip it
        loop {
            tick.tick().await;
            if tx.send(Message::Ping(Vec::new().into())).is_err() {
                return; // connection gone; the forwarder cleaned up
            }
        }
    })
}

/// Apply an exported settings blob to one board: parse + version-check it,
/// overwrite the durable preferences (and nothing else), then persist and push
/// through the exact path `apply_command` uses. Errors are human-readable —
/// they go straight into the app's import-failed alert.
pub(crate) fn apply_settings_import(state: &AppState, board_id: &str, json: &str) -> Result<(), String> {
    // Import is the one single-message bulk write into a config (commands only
    // grow it one entry at a time), and the blob is persisted whole and cloned
    // on every render — so bound it before parsing.
    if json.len() > 128 * 1024 {
        return Err("settings blob is implausibly large".to_string());
    }
    let mut settings: PortableSettings = serde_json::from_str(json)
        .map_err(|e| format!("that isn't a settings export ({e})"))?;
    if settings.version != PORTABLE_SETTINGS_VERSION {
        return Err(format!(
            "unsupported settings version {} (this server understands version {})",
            settings.version, PORTABLE_SETTINGS_VERSION
        ));
    }
    // Every command path is find-or-push by sport_id, so duplicates are
    // structurally impossible there — and a duplicate smuggled in here would be
    // half-active but uneditable (commands rewrite only the first match).
    // Keep first occurrence, drop the rest.
    let mut seen = std::collections::HashSet::new();
    settings.sports.retain(|s| seen.insert(s.sport_id.clone()));
    // An unparseable timezone in a hand-edited blob is dropped BEFORE applying
    // — `apply_to` skips None, so the board keeps its current (valid) zone.
    if settings.timezone.as_deref().is_some_and(|t| t.parse::<chrono_tz::Tz>().is_err()) {
        settings.timezone = None;
    }
    let updated = {
        let mut configs = state.configs.lock().unwrap();
        let cfg = configs.entry(board_id.to_string()).or_default();
        settings.apply_to(cfg);
        // The blob is user-portable text: re-apply the same bounds the
        // equivalent commands enforce, so a hand-edited export can't smuggle
        // out-of-range values into the config.
        cfg.brightness = cfg.brightness.clamp(10, 100);
        cfg.dwell_secs = cfg.dwell_secs.clamp(3, 60);
        cfg.sleep_hours = cfg.sleep_hours.clamp(1, 72);
        for s in &mut cfg.sports {
            s.dwell_secs = s.dwell_secs.map(|v| v.clamp(3, 60));
            s.leaderboard_rows = s.leaderboard_rows.map(|v| v.clamp(4, 20));
        }
        cfg.clone()
    };
    push_board(state, board_id);
    push_board_state(state, board_id);
    state.persist_config(board_id, &updated);
    Ok(())
}

pub(crate) fn apply_command(state: &AppState, board_id: &str, cmd: Command) {
    // Firmware update touches no config at all: relay the hosted dist to the
    // board's device socket (no dist / offline board = logged no-op) and skip
    // the fold-persist-push cycle below entirely.
    if matches!(cmd, Command::UpdateFirmware) {
        crate::handle_update_firmware(state, board_id);
        return;
    }
    // Same short-circuit as UpdateFirmware: InstallFirmware relays a named
    // channel's dist to the device and touches no config. Borrow `cmd` (don't
    // move it) so the exhaustive fold below still compiles.
    if let Command::InstallFirmware { channel } = &cmd {
        crate::handle_install_firmware(state, board_id, *channel);
        return;
    }
    // Set when the app toggles power, so the board gets the same "SLEEPING" splash
    // (or notice cancel) the physical button produces. Handled outside the config
    // lock below.
    let mut power_toggle: Option<bool> = None;
    // Set on SetFwChannel: the update answer a connected DEVICE holds
    // (`UpdateAvailable`, feeding its settings menu) may change with the
    // channel, so it gets re-pushed alongside the app's BoardState below.
    let mut fw_channel_switched = false;
    let updated = {
        let mut configs = state.configs.lock().unwrap();
        let cfg = configs.entry(board_id.to_string()).or_default();
        match cmd {
            // Same range as the physical knob (10..=100 in steps of 10) — one
            // brightness, one scale, whichever control set it last.
            Command::SetBrightness { level } => cfg.brightness = level.clamp(10, 100),
            Command::SetPower { on } => {
                cfg.power = on;
                if on {
                    // Turning on from the app cancels any pending sleep timer.
                    cfg.wake_at = None;
                } else {
                    // App sleep = button sleep: the same timed window, after
                    // which the server wakes the board — never an indefinite off.
                    cfg.wake_at =
                        Some(crate::unix_now() + cfg.sleep_hours.max(1) as u64 * 3600);
                }
                power_toggle = Some(on);
            }
            Command::SetSportConfig {
                sport_id,
                favorite_team_ids,
                championship_option_id,
            } => {
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    // Field-wise on purpose: a league's dwell override survives
                    // favorite/option edits.
                    s.favorite_team_ids = favorite_team_ids;
                    s.championship_option_id = championship_option_id;
                } else {
                    cfg.sports.push(SportConfig {
                        sport_id,
                        favorite_team_ids,
                        championship_option_id,
                        ..Default::default()
                    });
                }
            }
            // Add/Remove are symmetric one-off tweaks: adding un-excludes a game the
            // user removed earlier (or appends an extra); removing un-adds an extra
            // (or excludes a preference-driven game WITHOUT touching the preference).
            Command::AddGame { game_id } => {
                cfg.excluded_game_ids.retain(|id| id != &game_id);
                // Un-excluding restores the game only if preferences still
                // cover it; otherwise the add MUST become an extra — the old
                // "un-exclude only" behavior made the first tap a silent
                // no-op on uncovered games (the app's optimistic add then
                // visibly reverted, and only a second tap stuck).
                let covered = crate::candidate_games(&state, cfg).iter().any(|g| g.id == game_id);
                if !covered && !cfg.extra_game_ids.contains(&game_id) {
                    cfg.extra_game_ids.push(game_id);
                }
            }
            Command::RemoveGame { game_id } => {
                if cfg.extra_game_ids.contains(&game_id) {
                    cfg.extra_game_ids.retain(|id| id != &game_id);
                } else if !cfg.excluded_game_ids.contains(&game_id) {
                    cfg.excluded_game_ids.push(game_id);
                }
            }
            Command::FocusGame { game_id } => cfg.focused_game_id = Some(game_id),
            Command::ClearFocus => cfg.focused_game_id = None,
            Command::ResetRotation => {
                cfg.focused_game_id = None;
                cfg.extra_game_ids.clear();
                cfg.excluded_game_ids.clear();
                // Per-game spoiler one-offs are rotation tweaks too. League
                // scrolling is NOT reset here: it's the league's leaderboard
                // depth, a durable setting like dwell, not a rotation tweak.
                cfg.spoiler_game_ids.clear();
                cfg.revealed_game_ids.clear();
            }
            Command::Rename { name } => {
                let trimmed = name.trim();
                cfg.name = (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            Command::SetOffState { off_state } => cfg.off_state = off_state,
            Command::SetSleepHours { hours } => cfg.sleep_hours = hours.clamp(1, 72),
            // Indefinite off: drop the auto-wake but leave the power state alone
            // (only meaningful while asleep; harmless no-op when awake).
            Command::ClearWakeTimer => cfg.wake_at = None,
            Command::SetDwell { secs } => cfg.dwell_secs = secs.clamp(3, 60),
            // Per-league "time per event": Some overrides the board default for
            // that league's games (F1 can scroll 30s while MLB flashes by in
            // 10); None reverts to the default.
            Command::SetLeagueDwell { sport_id, secs } => {
                let secs = secs.map(|s| s.clamp(3, 60));
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.dwell_secs = secs;
                } else if secs.is_some() {
                    cfg.sports.push(SportConfig { sport_id, dwell_secs: secs, ..Default::default() });
                }
            }
            // Leaderboard depth for golf/F1: 4 = static top-4, 20 = full field.
            Command::SetLeagueRows { sport_id, rows } => {
                let rows = rows.map(|r| r.clamp(4, 20));
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.leaderboard_rows = rows;
                } else if rows.is_some() {
                    cfg.sports.push(SportConfig { sport_id, leaderboard_rows: rows, ..Default::default() });
                }
            }
            // Per-league team-name display style: Some overrides the league
            // default (City/Team name/Abbreviation), None reverts to it. Upsert
            // by sport_id like SetLeagueDwell so a name-style edit on a league
            // the user hasn't otherwise configured still sticks.
            Command::SetLeagueNameStyle { sport_id, style } => {
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.name_style = style;
                } else if style.is_some() {
                    cfg.sports.push(SportConfig { sport_id, name_style: style, ..Default::default() });
                }
            }
            Command::SetSpoilerMode { mode } => cfg.spoiler_mode = mode,
            Command::SetSpoilerAll { on } => cfg.spoiler_all = on,
            Command::SetLeagueSpoiler { sport_id, on } => {
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.spoiler = on;
                } else if on {
                    cfg.sports.push(SportConfig { sport_id, spoiler: true, ..Default::default() });
                }
            }
            Command::SetTeamSpoiler { sport_id, team_id, on } => {
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.spoiler_team_ids.retain(|t| t != &team_id);
                    if on {
                        s.spoiler_team_ids.push(team_id);
                    }
                } else if on {
                    cfg.sports.push(SportConfig {
                        sport_id,
                        spoiler_team_ids: vec![team_id],
                        ..Default::default()
                    });
                }
            }
            // Per-game toggle. The two lists are mutually exclusive: hiding
            // clears a reveal and vice versa, so the button always flips state.
            Command::SetGameSpoiler { game_id, hidden } => {
                cfg.spoiler_game_ids.retain(|id| id != &game_id);
                cfg.revealed_game_ids.retain(|id| id != &game_id);
                if hidden {
                    cfg.spoiler_game_ids.push(game_id);
                } else {
                    cfg.revealed_game_ids.push(game_id);
                }
            }
            // Board-local IANA timezone. An unparseable name is ignored — the
            // board keeps whatever zone it already had.
            Command::SetTimezone { tz } => {
                if tz.parse::<chrono_tz::Tz>().is_ok() {
                    cfg.timezone = Some(tz);
                }
            }
            // Firmware release channel. The push_board_state below re-runs the
            // offer computation against the new channel, so switching to Beta
            // with a newer beta dist hosted lights the app's Update button on
            // THIS push — no reconnect involved. The device's own menu answer
            // is re-pushed too (see fw_channel_switched).
            Command::SetFwChannel { channel } => {
                cfg.fw_channel = channel;
                fw_channel_switched = true;
            }
            // Relayed to the device by the early return at the top of this
            // function — never reaches the config fold. The arm stays so a new
            // Command variant is still a compile error here.
            Command::UpdateFirmware => unreachable!("handled before the config fold"),
            // Relayed to the device by the early return at the top of this
            // function (`handle_install_firmware`) — never reaches the config
            // fold. The arm stays so a new Command variant is a compile error.
            Command::InstallFirmware { .. } => unreachable!("handled before the config fold"),
            // Deliberately no catch-all: a new Command variant should be a compile
            // error here, not a silently ignored no-op.
        }
        cfg.clone()
    };
    if let Some(on) = power_toggle {
        crate::handle_app_power(state, board_id, on);
    }
    push_board(state, board_id);
    push_board_state(state, board_id);
    if fw_channel_switched {
        // The device-side mirror of the recomputed app offer above: a channel
        // switch changes what RequestUpdate would install, so re-answer the
        // board's menu too (quiet no-op when offline / pre-report).
        crate::push_update_available(state, board_id);
    }
    state.persist_config(board_id, &updated);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use axum::routing::get;
    use axum::Router;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message as TMessage;
    use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

    type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

    async fn spawn_server() -> (String, AppState) {
        spawn_server_with(AppState::new_ephemeral()).await
    }

    async fn spawn_server_with(state: AppState) -> (String, AppState) {
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("ws://{addr}/ws"), state)
    }

    /// An ephemeral state hosting fake firmware dists, one per (channel,
    /// version) pair. The sigs are opaque to the server (only the board
    /// verifies them), so any hex works.
    fn state_with_firmware(dists: &[(FwChannel, &str)]) -> AppState {
        let mut state = AppState::new_ephemeral();
        for (chan, version) in dists {
            let image = alloc_image(version);
            state.firmware.insert(
                *chan,
                std::sync::Arc::new(crate::FirmwareDist {
                    manifest: crate::FirmwareManifest {
                        version: (*version).into(),
                        channel: chan.as_str().into(),
                        size: image.len() as u32,
                        sha256: sha256_hex(&image),
                        sig: "cd".repeat(64),
                    },
                    bytes: image.into(),
                }),
            );
        }
        state
    }

    /// Distinct bytes per version so no two fake dists are identical.
    fn alloc_image(version: &str) -> Vec<u8> {
        let mut image = b"\xE9\x00ota image ".to_vec();
        image.extend_from_slice(version.as_bytes());
        image
    }

    async fn send(ws: &mut Client, m: &ClientMsg) {
        ws.send(TMessage::Text(serde_json::to_string(m).unwrap())).await.unwrap();
    }

    async fn recv(ws: &mut Client) -> Option<ServerMsg> {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(TMessage::Text(t)))) => {
                    if let Ok(m) = serde_json::from_str::<ServerMsg>(&t) {
                        return Some(m);
                    }
                }
                Ok(Some(Ok(_))) => continue, // skip binary frames / pings
                _ => return None,
            }
        }
    }

    /// Read until a message of the expected variant arrives (skipping manifests etc.).
    async fn recv_until(ws: &mut Client, pred: impl Fn(&ServerMsg) -> bool) -> Option<ServerMsg> {
        for _ in 0..8 {
            match recv(ws).await {
                Some(m) if pred(&m) => return Some(m),
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }

    /// Connect, sign the server's challenge with `secret`, return (ws, pubkey).
    async fn auth(url: &str, role: Role, secret: &[u8; 32]) -> (Client, String) {
        let (mut ws, _) = connect_async(url).await.unwrap();
        let challenge = match recv(&mut ws).await {
            Some(ServerMsg::Hello { challenge }) => challenge,
            _ => panic!("expected Hello"),
        };
        let pubkey = pubkey_hex(secret);
        let sig = sign_hex(secret, challenge.as_bytes());
        send(&mut ws, &ClientMsg::Auth { role, pubkey: pubkey.clone(), sig }).await;
        assert!(matches!(recv(&mut ws).await, Some(ServerMsg::AuthOk)));
        (ws, pubkey)
    }

    /// SetLeagueNameStyle upserts the league's SportConfig.name_style like the
    /// other per-league settings: Some sets it (creating the entry if needed),
    /// None reverts to the league default, and None on an unconfigured league is
    /// a no-op (no empty entry).
    #[tokio::test]
    async fn set_league_name_style_round_trips() {
        let state = AppState::new_ephemeral();
        let board = "board-namestyle";

        // Some on a league with no prior config creates the entry.
        apply_command(
            &state,
            board,
            Command::SetLeagueNameStyle { sport_id: "cfb".into(), style: Some(NameStyle::Abbrev) },
        );
        let cfg = state.config_for(board);
        assert_eq!(
            cfg.sports.iter().find(|s| s.sport_id == "cfb").and_then(|s| s.name_style),
            Some(NameStyle::Abbrev),
            "name_style set and persisted",
        );

        // None reverts to the league default without dropping the entry.
        apply_command(
            &state,
            board,
            Command::SetLeagueNameStyle { sport_id: "cfb".into(), style: None },
        );
        let cfg = state.config_for(board);
        assert_eq!(
            cfg.sports.iter().find(|s| s.sport_id == "cfb").and_then(|s| s.name_style),
            None,
            "name_style cleared back to the default",
        );

        // None on a league that was never configured stays a no-op.
        apply_command(
            &state,
            board,
            Command::SetLeagueNameStyle { sport_id: "nhl".into(), style: None },
        );
        let cfg = state.config_for(board);
        assert!(
            !cfg.sports.iter().any(|s| s.sport_id == "nhl"),
            "None on an unconfigured league creates no entry",
        );
    }

    #[tokio::test]
    async fn pair_flow_end_to_end() {
        let (url, _state) = spawn_server().await;

        // Device authenticates (its pubkey IS its board_id) and announces a code.
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        send(&mut dev, &ClientMsg::EnterSetup { code: "424242".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Controller authenticates and pairs using that code.
        let (mut ctrl, _ctrl) = auth(&url, Role::Controller, &[2u8; 32]).await;
        send(&mut ctrl, &ClientMsg::Pair { code: "424242".into() }).await;

        // Controller learns which board it paired with.
        let paired = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::Paired { .. })).await;
        assert!(matches!(paired, Some(ServerMsg::Paired { board_id: b }) if b == board_id));

        // Device is told to leave setup.
        let done = recv_until(&mut dev, |m| matches!(m, ServerMsg::SetupDone)).await;
        assert!(matches!(done, Some(ServerMsg::SetupDone)));

        // A wrong code is rejected.
        send(&mut ctrl, &ClientMsg::Pair { code: "000000".into() }).await;
        let failed = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::PairFailed { .. })).await;
        assert!(matches!(failed, Some(ServerMsg::PairFailed { .. })));

        // The paired controller can command its board.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 1,
            board_id: board_id.clone(),
            cmd: Command::SetBrightness { level: 50 },
        }).await;
        let ack = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await;
        assert!(matches!(ack, Some(ServerMsg::CommandAck { ok: true, .. })));

        // An unpaired controller cannot.
        let (mut other, _) = auth(&url, Role::Controller, &[3u8; 32]).await;
        send(&mut other, &ClientMsg::Command {
            req_id: 2,
            board_id: board_id.clone(),
            cmd: Command::SetBrightness { level: 10 },
        }).await;
        let ack2 = recv_until(&mut other, |m| matches!(m, ServerMsg::CommandAck { .. })).await;
        assert!(matches!(ack2, Some(ServerMsg::CommandAck { ok: false, .. })));
    }

    /// "Remove this scoreboard" over the wire: the grant a controller hands
    /// back is really gone — its next command is refused, and it takes a fresh
    /// code to get back in. Grants used to be permanent (nothing ever removed
    /// one), so removal was app-UI only.
    #[tokio::test]
    async fn unpair_revokes_the_grant_over_the_wire() {
        let (url, state) = spawn_server().await;
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        send(&mut dev, &ClientMsg::EnterSetup { code: "424242".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (mut ctrl, ctrl_key) = auth(&url, Role::Controller, &[2u8; 32]).await;
        send(&mut ctrl, &ClientMsg::Pair { code: "424242".into() }).await;
        let _ = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::Paired { .. })).await;
        assert!(state.is_allowed(&board_id, &ctrl_key));

        // Unpairing a board this controller was never paired with changes
        // nothing (no error, no collateral revocation).
        send(&mut ctrl, &ClientMsg::Unpair { board_id: "some-other-board".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(state.is_allowed(&board_id, &ctrl_key), "no-op unpair left the real grant alone");

        send(&mut ctrl, &ClientMsg::Unpair { board_id: board_id.clone() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!state.is_allowed(&board_id, &ctrl_key), "grant revoked");

        // The very next command from that controller is refused.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 7,
            board_id: board_id.clone(),
            cmd: Command::SetBrightness { level: 50 },
        }).await;
        let ack = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await;
        assert!(matches!(ack, Some(ServerMsg::CommandAck { ok: false, .. })));
    }

    /// Export from one board, import onto a second: the durable preferences
    /// (leagues, spoilers, timing) transfer; the target board's identity (name)
    /// and rotation one-offs survive untouched. Bad blobs are rejected with a
    /// readable error and change nothing.
    #[tokio::test]
    async fn settings_export_import_round_trip() {
        let (url, state) = spawn_server().await;

        // Two boards; one controller paired with both.
        let (mut dev_a, board_a) = auth(&url, Role::Device, &[1u8; 32]).await;
        send(&mut dev_a, &ClientMsg::EnterSetup { code: "111111".into() }).await;
        let (mut dev_b, board_b) = auth(&url, Role::Device, &[4u8; 32]).await;
        send(&mut dev_b, &ClientMsg::EnterSetup { code: "222222".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (mut ctrl, _me) = auth(&url, Role::Controller, &[2u8; 32]).await;
        send(&mut ctrl, &ClientMsg::Pair { code: "111111".into() }).await;
        assert!(recv_until(&mut ctrl, |m| matches!(m, ServerMsg::Paired { .. })).await.is_some());
        send(&mut ctrl, &ClientMsg::Pair { code: "222222".into() }).await;
        assert!(recv_until(&mut ctrl, |m| matches!(m, ServerMsg::Paired { .. })).await.is_some());

        // Board A carries the preferences to transfer; board B has its own name
        // and rotation tweaks that the import must NOT touch.
        {
            let mut configs = state.configs.lock().unwrap();
            let a = configs.entry(board_a.clone()).or_default();
            a.brightness = 40;
            a.dwell_secs = 20;
            a.sleep_hours = 8;
            a.off_state = OffState::Blank;
            a.spoiler_mode = SpoilerMode::Strict;
            a.spoiler_all = true;
            // Durable, but deliberately NOT portable: importing someone's
            // settings must never enroll the target board in beta firmware.
            a.fw_channel = FwChannel::Beta;
            a.sports = vec![SportConfig {
                sport_id: "nhl".into(),
                favorite_team_ids: vec!["nhl-bos".into()],
                championship_option_id: Some("playoffs".into()),
                dwell_secs: Some(30),
                leaderboard_rows: None,
                spoiler: true,
                spoiler_team_ids: vec!["nhl-tor".into()],
                name_style: None,
            }];
            let b = configs.entry(board_b.clone()).or_default();
            b.name = Some("Living room".into());
            b.extra_game_ids = vec!["extra-1".into()];
            b.excluded_game_ids = vec!["excluded-1".into()];
            b.focused_game_id = Some("focus-1".into());
        }

        // Export board A's settings.
        send(&mut ctrl, &ClientMsg::ExportSettings { board_id: board_a.clone() }).await;
        let json = match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::SettingsExport { .. })).await {
            Some(ServerMsg::SettingsExport { board_id, json }) => {
                assert_eq!(board_id, board_a);
                json
            }
            other => panic!("expected SettingsExport, got {other:?}"),
        };
        assert!(json.contains("\"scoreboard_settings\": 1"), "export is version-tagged: {json}");

        // Import them onto board B.
        send(&mut ctrl, &ClientMsg::ImportSettings { board_id: board_b.clone(), json: json.clone() }).await;
        let res = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::ImportResult { .. })).await;
        assert!(
            matches!(&res, Some(ServerMsg::ImportResult { board_id, error: None }) if *board_id == board_b),
            "import should succeed: {res:?}"
        );

        let a = state.config_for(&board_a);
        let b = state.config_for(&board_b);
        // The durable half matches the source board exactly...
        assert_eq!(PortableSettings::from(&b), PortableSettings::from(&a));
        assert_eq!(b.dwell_secs, 20);
        assert_eq!(b.spoiler_mode, SpoilerMode::Strict);
        assert!(b.spoiler_all);
        assert_eq!(b.sports, a.sports);
        // ...while B's identity + rotation one-offs survive untouched.
        assert_eq!(b.name.as_deref(), Some("Living room"));
        assert_eq!(b.extra_game_ids, vec!["extra-1".to_string()]);
        assert_eq!(b.excluded_game_ids, vec!["excluded-1".to_string()]);
        assert_eq!(b.focused_game_id.as_deref(), Some("focus-1"));
        assert!(b.power);
        // A's beta enrollment did NOT travel with the export.
        assert_eq!(b.fw_channel, FwChannel::Stable);
        // And the source board's name never travels.
        assert_eq!(a.name, None);

        // Malformed JSON: readable error, config untouched.
        let before = serde_json::to_string(&b).unwrap();
        send(&mut ctrl, &ClientMsg::ImportSettings { board_id: board_b.clone(), json: "{not json".into() }).await;
        match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::ImportResult { .. })).await {
            Some(ServerMsg::ImportResult { error: Some(e), .. }) => {
                assert!(e.contains("settings export"), "unexpected error text: {e}")
            }
            other => panic!("expected ImportResult with an error, got {other:?}"),
        }
        assert_eq!(serde_json::to_string(&state.config_for(&board_b)).unwrap(), before);

        // Wrong version: rejected too.
        send(&mut ctrl, &ClientMsg::ImportSettings {
            board_id: board_b.clone(),
            json: "{\"scoreboard_settings\": 2}".into(),
        }).await;
        match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::ImportResult { .. })).await {
            Some(ServerMsg::ImportResult { error: Some(e), .. }) => {
                assert!(e.contains("version"), "unexpected error text: {e}")
            }
            other => panic!("expected ImportResult with an error, got {other:?}"),
        }
        assert_eq!(serde_json::to_string(&state.config_for(&board_b)).unwrap(), before);

        // An unpaired controller can neither import...
        let (mut other, _) = auth(&url, Role::Controller, &[3u8; 32]).await;
        send(&mut other, &ClientMsg::ImportSettings { board_id: board_b.clone(), json }).await;
        match recv_until(&mut other, |m| matches!(m, ServerMsg::ImportResult { .. })).await {
            Some(ServerMsg::ImportResult { error: Some(e), .. }) => assert!(e.contains("not paired")),
            other => panic!("expected ImportResult with an error, got {other:?}"),
        }
        // ...nor export — and the refusal is an explicit error, not silence
        // (a stale pairing would otherwise be a dead button in the app).
        send(&mut other, &ClientMsg::ExportSettings { board_id: board_a.clone() }).await;
        match recv_until(&mut other, |m| {
            matches!(m, ServerMsg::SettingsExport { .. } | ServerMsg::ImportResult { .. })
        })
        .await
        {
            Some(ServerMsg::ImportResult { error: Some(e), .. }) => {
                assert!(e.contains("pairing"), "unexpected error text: {e}")
            }
            other => panic!("expected an ImportResult error (no export for unpaired), got {other:?}"),
        }
    }

    /// Pair a fresh controller with `dev`'s board and subscribe it, consuming
    /// the initial BoardState push. Returns the subscribed controller.
    async fn paired_subscriber(url: &str, dev: &mut Client, board_id: &str, secret: &[u8; 32]) -> Client {
        send(dev, &ClientMsg::EnterSetup { code: "313131".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut ctrl, _) = auth(url, Role::Controller, secret).await;
        send(&mut ctrl, &ClientMsg::Pair { code: "313131".into() }).await;
        assert!(recv_until(&mut ctrl, |m| matches!(m, ServerMsg::Paired { .. })).await.is_some());
        send(&mut ctrl, &ClientMsg::Subscribe { board_ids: vec![board_id.to_string()] }).await;
        let first = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::BoardState { .. })).await;
        match first {
            Some(ServerMsg::BoardState { firmware_version, firmware_update, .. }) => {
                assert_eq!(firmware_version, None, "no version before the device reports one");
                assert_eq!(firmware_update, None, "no offer without a reported version");
            }
            other => panic!("expected the initial BoardState, got {other:?}"),
        }
        ctrl
    }

    /// DeviceVersion flows into BoardState: the reported version is echoed,
    /// and `firmware_update` carries the hosted dist's version ONLY while that
    /// dist is strictly newer (CalVer) than the report.
    #[tokio::test]
    async fn device_version_drives_board_state_update_offer() {
        let (url, _state) =
            spawn_server_with(state_with_firmware(&[(FwChannel::Stable, "2026.7.0")])).await;
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // An older report -> version echoed + the dist offered as an update.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.6.0".into() }).await;
        let pushed = recv_until(&mut ctrl, |m| {
            matches!(m, ServerMsg::BoardState { firmware_version: Some(_), .. })
        })
        .await;
        match pushed {
            Some(ServerMsg::BoardState { firmware_version, firmware_update, .. }) => {
                assert_eq!(firmware_version.as_deref(), Some("2026.6.0"));
                assert_eq!(firmware_update.as_deref(), Some("2026.7.0"), "dist is newer -> offered");
            }
            other => panic!("expected a BoardState push after DeviceVersion, got {other:?}"),
        }

        // Reporting the dist's own version -> echoed, but nothing to offer.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        let pushed = recv_until(&mut ctrl, |m| {
            matches!(m, ServerMsg::BoardState { firmware_version: Some(v), .. } if v == "2026.7.0")
        })
        .await;
        match pushed {
            Some(ServerMsg::BoardState { firmware_update, .. }) => {
                assert_eq!(firmware_update, None, "equal version is NOT an update");
            }
            other => panic!("expected a BoardState push after DeviceVersion, got {other:?}"),
        }
    }

    /// Command::UpdateFirmware re-runs the offer pick at command time and
    /// relays the winning dist to the board's device socket as
    /// ServerMsg::UpdateFirmware (version/size/sha256/sig + the CHANNEL path
    /// the board must GET).
    #[tokio::test]
    async fn update_firmware_command_reaches_the_device() {
        // Beta board; the newest allowed dist is the beta one — the offer must
        // carry beta's metadata and /firmware/beta.bin.
        let (url, state) = spawn_server_with(state_with_firmware(&[
            (FwChannel::Stable, "2026.7.0"),
            (FwChannel::Beta, "2026.8.0"),
        ]))
        .await;
        let fw = state.firmware.get(&FwChannel::Beta).expect("test state hosts a beta dist").clone();
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        state.configs.lock().unwrap().entry(board_id.clone()).or_default().fw_channel =
            FwChannel::Beta;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // The pick needs the board's reported version (no report, no offer).
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.6.0".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        send(&mut ctrl, &ClientMsg::Command {
            req_id: 7,
            board_id: board_id.clone(),
            cmd: Command::UpdateFirmware,
        })
        .await;
        let ack = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await;
        assert!(matches!(ack, Some(ServerMsg::CommandAck { req_id: 7, ok: true, .. })));

        let offer = recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateFirmware { .. })).await;
        match offer {
            Some(ServerMsg::UpdateFirmware { version, size, sha256, sig, path, channel }) => {
                assert_eq!(version, fw.manifest.version);
                assert_eq!(size, fw.manifest.size);
                assert_eq!(sha256, fw.manifest.sha256);
                assert_eq!(sig, fw.manifest.sig);
                assert!(
                    path.starts_with("/firmware/beta.bin?u="),
                    "path names the dist's channel + carries a download token, got {path:?}"
                );
                assert_eq!(channel, "beta", "offer names the dist's channel (signed on-device)");
            }
            other => panic!("expected UpdateFirmware at the device, got {other:?}"),
        }
    }

    /// SetFwChannel flows through apply_command like any config command: the
    /// channel persists into the board's config AND the very next BoardState
    /// push recomputes the offer — switching to Beta with a newer beta dist
    /// hosted lights the Update button immediately, no reconnect; switching
    /// back to Stable clears it just as immediately.
    #[tokio::test]
    async fn set_fw_channel_round_trips_and_reoffers_immediately() {
        let (url, state) = spawn_server_with(state_with_firmware(&[
            (FwChannel::Stable, "2026.7.0"),
            (FwChannel::Beta, "2026.8.0"),
        ]))
        .await;
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // Running the current stable: a Stable-channel board has no update.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        match recv_until(&mut ctrl, |m| {
            matches!(m, ServerMsg::BoardState { firmware_version: Some(_), .. })
        })
        .await
        {
            Some(ServerMsg::BoardState { firmware_update, config, .. }) => {
                assert_eq!(config.fw_channel, FwChannel::Stable);
                assert_eq!(firmware_update, None, "stable board never sees the beta dist");
            }
            other => panic!("expected BoardState after DeviceVersion, got {other:?}"),
        }

        // Switch to Beta: the SAME command's state reflection must already
        // carry the recomputed offer.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 8,
            board_id: board_id.clone(),
            cmd: Command::SetFwChannel { channel: FwChannel::Beta },
        })
        .await;
        let ack = recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await;
        assert!(matches!(ack, Some(ServerMsg::CommandAck { req_id: 8, ok: true, .. })));
        match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::BoardState { .. })).await {
            Some(ServerMsg::BoardState { firmware_update, config, .. }) => {
                assert_eq!(config.fw_channel, FwChannel::Beta, "channel round-trips into config");
                assert_eq!(
                    firmware_update.as_deref(),
                    Some("2026.8.0"),
                    "beta offer appears on the very push that acked the switch"
                );
            }
            other => panic!("expected BoardState after SetFwChannel, got {other:?}"),
        }
        // And it persisted (what a reconnect would read back).
        assert_eq!(state.config_for(&board_id).fw_channel, FwChannel::Beta);

        // Switch back: the offer disappears just as immediately.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 9,
            board_id: board_id.clone(),
            cmd: Command::SetFwChannel { channel: FwChannel::Stable },
        })
        .await;
        assert!(matches!(
            recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await,
            Some(ServerMsg::CommandAck { req_id: 9, ok: true, .. })
        ));
        match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::BoardState { .. })).await {
            Some(ServerMsg::BoardState { firmware_update, config, .. }) => {
                assert_eq!(config.fw_channel, FwChannel::Stable);
                assert_eq!(firmware_update, None, "back on stable: nothing to offer");
            }
            other => panic!("expected BoardState after SetFwChannel, got {other:?}"),
        }
    }

    // ---- streaming OTA download + progress ----

    /// A server hosting a stable dist big enough to stream in several chunks,
    /// plus the WS routes AND the firmware HTTP routes on one listener — so a
    /// subscribed controller (WS) can observe progress from a real HTTP fetch.
    fn state_with_big_stable_dist() -> (AppState, Vec<u8>) {
        // ~40 KB -> three ~16 KB chunks; distinct byte pattern so a truncation
        // or off-by-one in the stream would fail the byte-identity check.
        let image: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let mut state = AppState::new_ephemeral();
        state.firmware.insert(
            FwChannel::Stable,
            std::sync::Arc::new(crate::FirmwareDist {
                manifest: crate::FirmwareManifest {
                    version: "2026.7.0".into(),
                    channel: "stable".into(),
                    size: image.len() as u32,
                    sha256: sha256_hex(&image),
                    sig: "ab".repeat(64),
                },
                bytes: image.clone().into(),
            }),
        );
        (state, image)
    }

    /// One listener serving both `/ws` and the firmware routes. Returns
    /// (ws url, http base, state).
    async fn spawn_full_server(state: AppState) -> (String, String, AppState) {
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .route("/firmware/:chan/latest", get(crate::get_firmware_latest))
            .route("/firmware/:file", get(crate::get_firmware_bin))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("ws://{addr}/ws"), format!("http://{addr}"), state)
    }

    /// A tokenized `GET /firmware/stable.bin?u=<nonce>` streams the image,
    /// retires the token once the stream drains, and delivers a body
    /// byte-identical to the dist. (Deliberately NO progress anywhere: the
    /// server-side estimate measured kernel/proxy buffers, and device-reported
    /// progress cost a concurrent socket the firmware's pool couldn't hold.
    /// The app shows a plain spinner.)
    #[tokio::test]
    async fn tokenized_download_serves_bytes_and_retires_its_token() {
        let (state, image) = state_with_big_stable_dist();
        let (ws_url, http, state) = spawn_full_server(state).await;
        let (mut dev, board_id) = auth(&ws_url, Role::Device, &[1u8; 32]).await;
        // The subscriber's own setup assertions still run; nothing is expected
        // on it afterwards (there is no download progress to observe).
        let _ctrl = paired_subscriber(&ws_url, &mut dev, &board_id, &[2u8; 32]).await;

        let nonce = "abad1deacafef00d".to_string();
        state
            .fw_download_tokens
            .lock()
            .unwrap()
            .insert(nonce.clone(), (board_id.clone(), std::time::Instant::now()));

        let body = reqwest::get(format!("{http}/firmware/stable.bin?u={nonce}"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&body[..], &image[..], "streamed body is byte-identical to the dist");

        // The one-time token is still retired once the stream completes.
        let mut cleared = false;
        for _ in 0..40 {
            if !state.fw_download_tokens.lock().unwrap().contains_key(&nonce) {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(cleared, "download token cleared after the stream drained");
    }

    /// The RAW HTTP framing of the tokenized stream, read exactly the way the
    /// board's bare-metal client does (find \r\n\r\n, then treat every byte as
    /// image). `reqwest` transparently decodes chunked transfer-encoding, so it
    /// would happily pass even if the response were chunked — but the firmware
    /// does NOT decode chunking and would hash the chunk-size lines into the
    /// image, failing every OTA. This asserts identity framing (Content-Length,
    /// NO Transfer-Encoding) and a byte-identical body.
    #[tokio::test]
    async fn tokenized_stream_is_content_length_framed_not_chunked() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (state, image) = state_with_big_stable_dist();
        let (_ws_url, http, state) = spawn_full_server(state).await;
        let nonce = "0badf00dcafe1234".to_string();
        state.fw_download_tokens.lock().unwrap().insert(
            nonce.clone(),
            ("board-x".to_string(), std::time::Instant::now()),
        );

        let addr = http.trim_start_matches("http://").to_string();
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        sock.write_all(
            format!("GET /firmware/stable.bin?u={nonce} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).await.unwrap();

        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("has headers");
        let headers = String::from_utf8_lossy(&raw[..sep]).to_lowercase();
        assert!(
            !headers.contains("transfer-encoding: chunked"),
            "the board can't decode chunked framing — response must be identity:\n{headers}"
        );
        assert!(
            headers.contains(&format!("content-length: {}", image.len())),
            "streamed response must carry the exact Content-Length:\n{headers}"
        );
        // Exactly what the firmware feeds to sha256/verify: raw post-header bytes.
        assert_eq!(&raw[sep + 4..], &image[..], "raw body byte-identical to the image");
    }

    /// An un-tokenized `GET /firmware/stable.bin` serves the same bytes and
    /// pushes NO progress to a subscribed controller (the plain path never
    /// enters the streaming branch).
    #[tokio::test]
    async fn untokenized_download_is_byte_identical_and_silent() {
        let (state, image) = state_with_big_stable_dist();
        let (ws_url, http, _state) = spawn_full_server(state).await;
        let (mut dev, board_id) = auth(&ws_url, Role::Device, &[1u8; 32]).await;
        // The subscriber's own setup assertions still run; nothing is expected
        // on it afterwards (there is no download progress to observe).
        let _ctrl = paired_subscriber(&ws_url, &mut dev, &board_id, &[2u8; 32]).await;

        let body = reqwest::get(format!("{http}/firmware/stable.bin"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&body[..], &image[..], "un-tokenized body is byte-identical");
    }

    /// ListFirmware returns one entry per hosted channel (stable-first), with
    /// each dist's version; unhosted channels are absent. Not board-scoped, so
    /// no pairing/subscription is required.
    #[tokio::test]
    async fn list_firmware_returns_all_hosted_channels() {
        let (url, _state) = spawn_server_with(state_with_firmware(&[
            (FwChannel::Stable, "2026.7.0"),
            (FwChannel::Dev, "2026.9.0"),
        ]))
        .await;
        let (mut ctrl, _) = auth(&url, Role::Controller, &[3u8; 32]).await;
        send(&mut ctrl, &ClientMsg::ListFirmware).await;
        match recv_until(&mut ctrl, |m| matches!(m, ServerMsg::FirmwareList { .. })).await {
            Some(ServerMsg::FirmwareList { images }) => {
                assert_eq!(images.len(), 2, "only hosted channels are listed (beta absent)");
                assert_eq!(images[0], FwImage { channel: "stable".into(), version: "2026.7.0".into() });
                assert_eq!(images[1], FwImage { channel: "dev".into(), version: "2026.9.0".into() });
            }
            other => panic!("expected FirmwareList, got {other:?}"),
        }
    }

    /// InstallFirmware{beta} relays the beta dist to the device as an
    /// UpdateFirmware offer, regardless of the board's reported version: the
    /// path names the beta channel + carries a token, and the version is the
    /// beta dist's.
    #[tokio::test]
    async fn install_firmware_sends_channel_offer() {
        let (url, state) = spawn_server_with(state_with_firmware(&[
            (FwChannel::Stable, "2026.7.0"),
            (FwChannel::Beta, "2026.8.0"),
        ]))
        .await;
        let beta = state.firmware.get(&FwChannel::Beta).expect("beta dist hosted").clone();
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // No DeviceVersion reported: InstallFirmware doesn't gate on it.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 11,
            board_id: board_id.clone(),
            cmd: Command::InstallFirmware { channel: FwChannel::Beta },
        })
        .await;
        assert!(matches!(
            recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await,
            Some(ServerMsg::CommandAck { req_id: 11, ok: true, .. })
        ));
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateFirmware { .. })).await {
            Some(ServerMsg::UpdateFirmware { version, path, channel, sha256, .. }) => {
                assert_eq!(version, beta.manifest.version, "offer carries the beta dist's version");
                assert_eq!(sha256, beta.manifest.sha256);
                assert!(path.starts_with("/firmware/beta.bin?u="), "tokenized beta path, got {path:?}");
                assert_eq!(channel, "beta");
            }
            other => panic!("expected UpdateFirmware at the device, got {other:?}"),
        }
    }

    /// A same-version InstallFirmware still sends an offer — the server never
    /// gates on version (the on-device same-or-newer guard governs whether it
    /// takes), so a deliberate reinstall reaches the device.
    #[tokio::test]
    async fn install_firmware_reinstalls_same_version() {
        let (url, _state) =
            spawn_server_with(state_with_firmware(&[(FwChannel::Stable, "2026.7.0")])).await;
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // The board already runs the hosted version.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        send(&mut ctrl, &ClientMsg::Command {
            req_id: 12,
            board_id: board_id.clone(),
            cmd: Command::InstallFirmware { channel: FwChannel::Stable },
        })
        .await;
        assert!(matches!(
            recv_until(&mut ctrl, |m| matches!(m, ServerMsg::CommandAck { .. })).await,
            Some(ServerMsg::CommandAck { req_id: 12, ok: true, .. })
        ));
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateFirmware { .. })).await {
            Some(ServerMsg::UpdateFirmware { version, path, .. }) => {
                assert_eq!(version, "2026.7.0", "reinstall sends the same version outright");
                assert!(path.starts_with("/firmware/stable.bin?u="), "got {path:?}");
            }
            other => panic!("expected a reinstall offer at the device, got {other:?}"),
        }
    }

    // ---- on-device settings menu: RequestUpdate / UpdateAvailable ----

    /// Every DeviceVersion report is answered with UpdateAvailable at the
    /// DEVICE: Some(dist version) when the channel offers something newer,
    /// None when up to date. The None case is a real push, not silence — the
    /// menu must tell "up to date" apart from "never heard back". And an
    /// UNCHANGED re-report is still answered: a reconnecting board (stale loop
    /// still holding its old entry) reports the same version but has a fresh
    /// menu with no answer.
    #[tokio::test]
    async fn device_version_is_answered_with_update_available() {
        let (url, _state) =
            spawn_server_with(state_with_firmware(&[(FwChannel::Stable, "2026.7.0")])).await;
        let (mut dev, _board_id) = auth(&url, Role::Device, &[1u8; 32]).await;

        // Older report -> the dist's version.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.6.0".into() }).await;
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await {
            Some(ServerMsg::UpdateAvailable { version }) => {
                assert_eq!(version.as_deref(), Some("2026.7.0"), "older report -> dist offered");
            }
            other => panic!("expected UpdateAvailable at the device, got {other:?}"),
        }

        // Up to date -> an explicit None answer.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await {
            Some(ServerMsg::UpdateAvailable { version }) => {
                assert_eq!(version, None, "up to date -> explicit None, not silence");
            }
            other => panic!("expected UpdateAvailable at the device, got {other:?}"),
        }

        // The SAME version again (a reconnect's re-report) is still answered.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        assert!(
            matches!(
                recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await,
                Some(ServerMsg::UpdateAvailable { version: None })
            ),
            "an unchanged re-report still gets its answer — the report doubles as the ask"
        );
    }

    /// RequestUpdate from the DEVICE runs the same pick the app's Update
    /// button does and lands the standard signed UpdateFirmware offer on the
    /// device socket — the on-device menu asks without a phone in the room.
    #[tokio::test]
    async fn request_update_receives_the_standard_offer() {
        let (url, state) =
            spawn_server_with(state_with_firmware(&[(FwChannel::Stable, "2026.7.0")])).await;
        let fw = state.firmware.get(&FwChannel::Stable).expect("stable dist hosted").clone();
        let (mut dev, _board_id) = auth(&url, Role::Device, &[1u8; 32]).await;

        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.6.0".into() }).await;
        send(&mut dev, &ClientMsg::RequestUpdate).await;
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateFirmware { .. })).await {
            Some(ServerMsg::UpdateFirmware { version, size, sha256, sig, path, channel }) => {
                assert_eq!(version, fw.manifest.version);
                assert_eq!(size, fw.manifest.size);
                assert_eq!(sha256, fw.manifest.sha256);
                assert_eq!(sig, fw.manifest.sig, "the offer is the SIGNED flow, unchanged");
                assert!(path.starts_with("/firmware/stable.bin?u="), "tokenized path, got {path:?}");
                assert_eq!(channel, "stable");
            }
            other => panic!("expected the standard UpdateFirmware offer, got {other:?}"),
        }
    }

    /// RequestUpdate with nothing newer hosted: no offer, no error, and the
    /// device loop stays fully live — the next report is still answered.
    #[tokio::test]
    async fn request_update_with_nothing_newer_is_a_quiet_noop() {
        let (url, _state) =
            spawn_server_with(state_with_firmware(&[(FwChannel::Stable, "2026.7.0")])).await;
        let (mut dev, _board_id) = auth(&url, Role::Device, &[1u8; 32]).await;

        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        assert!(matches!(
            recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await,
            Some(ServerMsg::UpdateAvailable { version: None })
        ));

        send(&mut dev, &ClientMsg::RequestUpdate).await;
        // Bound the wait: nothing newer must send NOTHING (no offer, no error).
        let offer = tokio::time::timeout(Duration::from_millis(400), async {
            loop {
                match dev.next().await {
                    Some(Ok(TMessage::Text(t))) => {
                        if let Ok(ServerMsg::UpdateFirmware { .. }) =
                            serde_json::from_str::<ServerMsg>(&t)
                        {
                            return;
                        }
                    }
                    Some(Ok(_)) => continue,
                    _ => panic!("device socket died on a no-op RequestUpdate"),
                }
            }
        })
        .await;
        assert!(offer.is_err(), "no offer may be sent when nothing newer is hosted");

        // The loop is still alive: a fresh report gets its answer.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.6.0".into() }).await;
        assert!(
            matches!(
                recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await,
                Some(ServerMsg::UpdateAvailable { version: Some(_) })
            ),
            "device loop still answers after the no-op"
        );
    }

    /// Switching release channels re-answers the DEVICE too: SetFwChannel
    /// changes what RequestUpdate would install, so the board's menu gets a
    /// fresh UpdateAvailable on the very command that switched it — no
    /// reconnect involved (mirrors the app-side reoffer test above).
    #[tokio::test]
    async fn set_fw_channel_repushes_update_available_to_the_device() {
        let (url, _state) = spawn_server_with(state_with_firmware(&[
            (FwChannel::Stable, "2026.7.0"),
            (FwChannel::Beta, "2026.8.0"),
        ]))
        .await;
        let (mut dev, board_id) = auth(&url, Role::Device, &[1u8; 32]).await;
        let mut ctrl = paired_subscriber(&url, &mut dev, &board_id, &[2u8; 32]).await;

        // Stable board running the current stable: the answer is None.
        send(&mut dev, &ClientMsg::DeviceVersion { version: "2026.7.0".into() }).await;
        assert!(matches!(
            recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await,
            Some(ServerMsg::UpdateAvailable { version: None })
        ));

        // App switches the board to Beta: the beta dist becomes the answer.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 21,
            board_id: board_id.clone(),
            cmd: Command::SetFwChannel { channel: FwChannel::Beta },
        })
        .await;
        match recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await {
            Some(ServerMsg::UpdateAvailable { version }) => {
                assert_eq!(
                    version.as_deref(),
                    Some("2026.8.0"),
                    "channel switch re-answers the device's menu"
                );
            }
            other => panic!("expected UpdateAvailable after SetFwChannel, got {other:?}"),
        }

        // And back to Stable: the answer clears just as immediately.
        send(&mut ctrl, &ClientMsg::Command {
            req_id: 22,
            board_id: board_id.clone(),
            cmd: Command::SetFwChannel { channel: FwChannel::Stable },
        })
        .await;
        assert!(
            matches!(
                recv_until(&mut dev, |m| matches!(m, ServerMsg::UpdateAvailable { .. })).await,
                Some(ServerMsg::UpdateAvailable { version: None })
            ),
            "switching back re-answers with None"
        );
    }
}

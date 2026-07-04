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
                            if let Some(px) = frames.get(&fid) {
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
                    state.pending.lock().unwrap().insert(code, board_id.clone());
                }
                Ok(ClientMsg::ButtonPress) => {
                    crate::handle_button(&state, &board_id);
                }
                _ => {}
            }
        }
    }

    state.devices.lock().unwrap().remove(&board_id);
    forward.abort();
    // Mirror the connect path: apps should see "offline" now, not at the next poll.
    push_board_state(&state, &board_id);
    tracing::info!("device '{board_id}' disconnected");
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
    let mut subscribed: Vec<String> = Vec::new();
    state.poll_wake.notify_one(); // fetch fresh games now (covers browse before subscribe)

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(t) = msg {
            match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::Pair { code }) => {
                    // Throttle pairing-code guesses — wrong guesses are otherwise free,
                    // so the 6-digit code would be brute-forceable.
                    if !state.pair_limiter.lock().unwrap().allow() {
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
                Ok(ClientMsg::Subscribe { board_ids }) => {
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
                Ok(ClientMsg::GetGames) => {
                    let games: Vec<GameSummary> = {
                        let g = state.games.lock().unwrap();
                        g.iter().filter(|g| crate::is_live(g)).map(crate::to_summary).collect()
                    };
                    let _ = out.send(Message::Text(
                        serde_json::to_string(&ServerMsg::Games { games }).unwrap(),
                    ));
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
    forward.abort();
    tracing::info!("controller '{pubkey}' disconnected");
}

pub(crate) fn apply_command(state: &AppState, board_id: &str, cmd: Command) {
    // Set when the app toggles power, so the board gets the same "SLEEPING" splash
    // (or notice cancel) the physical button produces. Handled outside the config
    // lock below.
    let mut power_toggle: Option<bool> = None;
    let updated = {
        let mut configs = state.configs.lock().unwrap();
        let cfg = configs.entry(board_id.to_string()).or_default();
        match cmd {
            Command::SetBrightness { level } => cfg.brightness = level,
            Command::SetPower { on } => {
                cfg.power = on;
                // Turning on from the app cancels any pending button-sleep timer.
                if on {
                    cfg.wake_at = None;
                }
                power_toggle = Some(on);
            }
            Command::SetSportConfig {
                sport_id,
                favorite_team_ids,
                championship_option_id,
            } => {
                if let Some(s) = cfg.sports.iter_mut().find(|s| s.sport_id == sport_id) {
                    s.favorite_team_ids = favorite_team_ids;
                    s.championship_option_id = championship_option_id;
                } else {
                    cfg.sports.push(SportConfig {
                        sport_id,
                        favorite_team_ids,
                        championship_option_id,
                    });
                }
            }
            // Add/Remove are symmetric one-off tweaks: adding un-excludes a game the
            // user removed earlier (or appends an extra); removing un-adds an extra
            // (or excludes a preference-driven game WITHOUT touching the preference).
            Command::AddGame { game_id } => {
                if cfg.excluded_game_ids.contains(&game_id) {
                    cfg.excluded_game_ids.retain(|id| id != &game_id);
                } else if !cfg.extra_game_ids.contains(&game_id) {
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
            }
            Command::Rename { name } => {
                let trimmed = name.trim();
                cfg.name = (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            Command::SetOffState { off_state } => cfg.off_state = off_state,
            Command::SetSleepHours { hours } => cfg.sleep_hours = hours.clamp(1, 72),
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

    async fn spawn_server() -> String {
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(AppState::new_ephemeral());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("ws://{addr}/ws")
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

    #[tokio::test]
    async fn pair_flow_end_to_end() {
        let url = spawn_server().await;

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
}

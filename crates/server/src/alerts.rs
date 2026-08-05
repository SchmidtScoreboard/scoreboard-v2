//! Email alerting and the daily report.
//!
//! # Why this can watch a wedged server
//!
//! The outage this exists for was not a crash. A background loop deadlocked on
//! a mutex, the process kept running, and everything that needed that mutex —
//! every websocket handler, every HTTP request — queued behind it forever.
//! Nothing was logged and nothing exited.
//!
//! A monitor that called the ordinary health path would have joined that queue
//! and never sent anything. So the probe here uses `try_lock` ONLY: it never
//! waits on a lock, and a lock it cannot take is itself the alert. Tokio's
//! multi-threaded runtime keeps this task on a different worker than the stuck
//! one, so it still runs.
//!
//! # What it cannot catch
//!
//! A process that is gone — killed, OOM, panicked at the top level — sends no
//! email, because there is nothing left to send it. That case belongs to the
//! platform healthcheck (`railway.json` points at `/health`) and to any
//! external uptime monitor. In-process alerting and an outside watcher cover
//! different failures; neither replaces the other.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::AppState;

/// How often the watchdog looks. Frequent enough to matter, rare enough that a
/// flapping upstream does not become a mailbox full of alerts.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// A problem must persist across this many consecutive checks before it is
/// worth an email. A single missed poll is not an incident.
const CONFIRM_CHECKS: u32 = 3;

/// Set once an alert has been sent, cleared on recovery — so an ongoing
/// incident sends ONE email, not one per minute.
static ALERTING: AtomicBool = AtomicBool::new(false);

pub struct Mailer {
    api_key: String,
    from: String,
    to: String,
}

impl Mailer {
    /// Configured entirely from the environment, and absent by default: a fork
    /// that sets nothing gets no alerting and no errors about it.
    ///
    /// `RESEND_API_KEY` + `ALERT_EMAIL_TO`, optional `ALERT_EMAIL_FROM`.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("RESEND_API_KEY").ok()?;
        let to = std::env::var("ALERT_EMAIL_TO").ok()?;
        if api_key.is_empty() || to.is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            from: std::env::var("ALERT_EMAIL_FROM")
                .unwrap_or_else(|_| "Scoreboard <onboarding@resend.dev>".into()),
            to,
        })
    }

    pub async fn send(&self, subject: &str, body: &str) -> Result<(), String> {
        let payload = serde_json::json!({
            "from": self.from,
            "to": [self.to],
            "subject": subject,
            "text": body,
        });
        let resp = reqwest::Client::new()
            .post("https://api.resend.com/emails")
            .bearer_auth(&self.api_key)
            .json(&payload)
            // A hung mail provider must not wedge the watchdog that exists to
            // notice hangs.
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("{} {}", resp.status(), resp.text().await.unwrap_or_default()))
        }
    }
}

/// What the watchdog could determine without ever blocking.
struct Probe {
    /// Locks it could not take. A populated list means something is holding a
    /// mutex far longer than any handler should.
    wedged: Vec<&'static str>,
    /// Loops whose heartbeat is older than its limit.
    stalled: Vec<String>,
    boards: Option<usize>,
    games: Option<usize>,
}

impl Probe {
    fn healthy(&self) -> bool {
        self.wedged.is_empty() && self.stalled.is_empty()
    }

    fn describe(&self) -> String {
        let mut out = String::new();
        if !self.wedged.is_empty() {
            out.push_str(&format!(
                "Locks that could not be acquired: {}.\nThe server is running but not \
                 answering — this is the signature of a deadlock, not a crash.\n\n",
                self.wedged.join(", ")
            ));
        }
        if !self.stalled.is_empty() {
            out.push_str(&format!("Background loops with no recent heartbeat: {}.\n\n", self.stalled.join(", ")));
        }
        out
    }
}

/// Look at the server WITHOUT waiting on anything.
fn probe(state: &AppState) -> Probe {
    let mut wedged = Vec::new();

    // try_lock, never lock. Waiting here would make the watchdog the next
    // casualty of whatever it is supposed to report.
    let boards = match state.devices.try_lock() {
        Ok(g) => Some(g.len()),
        Err(_) => {
            wedged.push("devices");
            None
        }
    };
    let games = match state.games.try_lock() {
        Ok(g) => Some(g.len()),
        Err(_) => {
            wedged.push("games");
            None
        }
    };
    if state.configs.try_lock().is_err() {
        wedged.push("configs");
    }

    let mut stalled = Vec::new();
    if let Ok(beats) = state.beats.try_lock() {
        for (name, limit) in crate::LOOP_LIMITS {
            if let Some(age) = beats.get(name).map(|t: &Instant| t.elapsed().as_secs()) {
                if age > limit {
                    stalled.push(format!("{name} ({age}s ago)"));
                }
            }
        }
    } else {
        wedged.push("beats");
    }

    Probe { wedged, stalled, boards, games }
}

/// Watch for trouble and mail about it. One email when a problem is confirmed,
/// one when it clears.
pub async fn watchdog(state: AppState, mailer: Mailer) {
    let mut consecutive = 0u32;
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        let p = probe(&state);
        if p.healthy() {
            consecutive = 0;
            // Only announce recovery if we announced the problem.
            if ALERTING.swap(false, Ordering::Relaxed) {
                let _ = mailer
                    .send(
                        "Scoreboard: recovered",
                        "The server is answering normally again.\n\nBoards connected: \
                         see /health for current numbers.",
                    )
                    .await;
                tracing::info!("alert: recovered");
            }
            continue;
        }

        consecutive += 1;
        if consecutive < CONFIRM_CHECKS || ALERTING.swap(true, Ordering::Relaxed) {
            continue;
        }
        let body = format!(
            "{}Boards connected: {}\nGames in slate: {}\n\nThis check runs inside the \
             server and never waits on a lock, so it can report a hang that is \
             blocking everything else. A server that has stopped entirely sends \
             nothing at all — the platform healthcheck covers that.\n",
            p.describe(),
            p.boards.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into()),
            p.games.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into()),
        );
        match mailer.send("Scoreboard: server is not answering", &body).await {
            Ok(()) => tracing::error!("alert sent: {}", p.describe().replace('\n', " ")),
            Err(e) => {
                tracing::error!("alert FAILED to send: {e}");
                // Let the next confirmed check try again rather than going
                // quiet about an unreported incident.
                ALERTING.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// One email a day with the numbers worth glancing at.
pub async fn daily_report(state: AppState, mailer: Mailer) {
    // Hour of the server's day to send at. UTC, because the server has no one
    // timezone — boards do.
    let hour: u32 = std::env::var("DAILY_REPORT_HOUR").ok().and_then(|h| h.parse().ok()).unwrap_or(14);
    let mut last_sent: Option<chrono::NaiveDate> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        use chrono::Timelike;
        let now = chrono::Utc::now();
        if now.hour() != hour || last_sent == Some(now.date_naive()) {
            continue;
        }
        last_sent = Some(now.date_naive());

        let (ok, report) = crate::health_report(&state);
        let body = format!(
            "Scoreboard daily report — {}\n\n\
             Status: {}\n\
             Boards connected: {}\n\
             Boards known: {}\n\
             Games in slate: {}\n\
             Frames cached: {}\n\
             Version: {}\n\n\
             Loop heartbeats (seconds since last pass):\n{}\n\
             Full detail any time at /health.\n",
            now.format("%Y-%m-%d"),
            if ok { "OK" } else { "DEGRADED" },
            report["boards_connected"],
            report["boards_known"],
            report["games"],
            report["frames_cached"],
            report["version"],
            report["loops"],
        );
        match mailer.send(&format!("Scoreboard daily report ({})", if ok { "ok" } else { "DEGRADED" }), &body).await {
            Ok(()) => tracing::info!("daily report sent"),
            Err(e) => tracing::warn!("daily report failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held lock must read as wedged rather than making the probe wait — the
    /// whole reason this can report a deadlock at all.
    #[test]
    fn the_probe_never_waits_on_a_held_lock() {
        let state = AppState::new_ephemeral();
        let held = state.devices.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let s2 = state.clone();
        std::thread::spawn(move || {
            let p = probe(&s2);
            let _ = tx.send(p.wedged.contains(&"devices") && !p.healthy());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "probe must report the held lock instead of blocking on it"
        );
        drop(held);
    }

    #[test]
    fn a_quiet_server_reads_as_healthy() {
        let state = AppState::new_ephemeral();
        let p = probe(&state);
        assert!(p.healthy(), "no held locks and no stale beats is healthy");
        assert!(p.wedged.is_empty());
    }

    /// Absent configuration must mean "no alerting", not a crash on boot.
    #[test]
    fn a_mailer_needs_both_a_key_and_a_recipient() {
        // Not asserting on the ambient env — just that both are required.
        if std::env::var("RESEND_API_KEY").is_err() || std::env::var("ALERT_EMAIL_TO").is_err() {
            assert!(Mailer::from_env().is_none());
        }
    }
}

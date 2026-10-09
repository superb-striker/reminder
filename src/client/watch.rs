//! `reminder watch` -- runs on the laptop, not the server. Polls
//! `GET /occurrences/pending-laptop` on a fixed interval, and for
//! anything it finds, runs `omarchy reminder <title>` locally, then
//! acks it back to the server so it isn't shown again.
//!
//! # Why this is pull, not push
//!
//! ntfy notifications (`notify/`) are push: the server can reach the
//! phone over the internet whenever it wants. It cannot do the same
//! for the laptop -- the laptop is behind NAT, asleep, or off most of
//! the time, and the server has no way to wake it up or reach it
//! directly. So instead the laptop reaches out on its own schedule,
//! whenever it happens to be online, and asks "what have I missed."
//! This is also exactly what makes "test while the laptop is
//! completely powered off" (Phase 7 of the project brief) work without
//! any special handling: nothing needs to notice the laptop went away,
//! because the server was never trying to reach it in the first
//! place. Occurrences just sit as `laptop_status = 'pending'` until
//! this loop comes back and asks for them.
//!
//! # Why a trait for the local-notify step
//!
//! Same reasoning as `notify::NotificationBackend`: [`LocalNotifier`]
//! keeps [`poll_once`] from knowing anything about `omarchy`
//! specifically, which is what makes it possible to test the
//! polling/acking logic (below) against a real fake HTTP server
//! without needing `omarchy` installed or a real reminder server
//! running -- most machines this test suite runs on, including
//! whatever CI or sandbox is building this, have neither.

use crate::client::ApiClient;
use anyhow::Context;
use std::time::Duration;

/// Anything that can show a reminder locally. The only real
/// implementation is [`OmarchyNotifier`]; the trait exists so
/// [`poll_once`] can be tested against a fake one.
pub trait LocalNotifier: Send + Sync {
    async fn notify(&self, title: &str) -> anyhow::Result<()>;
}

/// Shells out to `omarchy reminder <title>` (or whatever command
/// `REMINDER_OMARCHY_CMD` names, for testing or for a differently
/// named binary).
pub struct OmarchyNotifier {
    command: String,
}

impl OmarchyNotifier {
    pub fn new(command: String) -> Self {
        OmarchyNotifier { command }
    }
}

impl LocalNotifier for OmarchyNotifier {
    async fn notify(&self, title: &str) -> anyhow::Result<()> {
        let status = tokio::process::Command::new(&self.command)
            .arg("reminder")
            .arg(title)
            .status()
            .await
            .with_context(|| {
                format!("failed to run `{} reminder` -- is it installed and on PATH?", self.command)
            })?;
        if !status.success() {
            anyhow::bail!("`{} reminder` exited with {status}", self.command);
        }
        Ok(())
    }
}

/// Poll forever, on a fixed interval, until the process is killed.
/// Never returns under normal operation.
pub async fn run<N: LocalNotifier>(client: ApiClient, notifier: N, interval_secs: u64) -> anyhow::Result<()> {
    println!(
        "watching for reminders on this machine (polling every {interval_secs}s), Ctrl-C to stop"
    );
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    loop {
        interval.tick().await;
        // A poll failing (server unreachable, laptop just wasn't
        // online, whatever) is logged and retried next interval, not
        // fatal -- that's the whole point of this being pull-based.
        match poll_once(&client, &notifier).await {
            Ok(0) => {}
            Ok(n) => println!("delivered {n} reminder(s) locally"),
            Err(e) => eprintln!("watch: poll failed, will retry: {e:#}"),
        }
    }
}

/// One pass: fetch pending occurrences, notify locally for each, and
/// ack the ones that succeeded. Returns how many were delivered.
///
/// An occurrence whose local notification fails is deliberately left
/// un-acked, so the next poll tries it again -- the same
/// fire-is-recorded-independent-of-delivery pattern as the ntfy retry
/// loop (see `notify/mod.rs`), just without a fixed attempt cap, since
/// unlike a possibly-permanently-dead ntfy topic, "omarchy isn't
/// running right now" is expected to be transient.
pub async fn poll_once<N: LocalNotifier>(client: &ApiClient, notifier: &N) -> anyhow::Result<usize> {
    let pending = client.pending_laptop_occurrences().await?;
    let mut delivered = 0;
    for occ in pending {
        match notifier.notify(&occ.reminder_title).await {
            Ok(()) => {
                client.ack_laptop_occurrence(occ.id).await?;
                delivered += 1;
            }
            Err(e) => {
                eprintln!(
                    "watch: local notify failed for occurrence {} ({}): {e:#}",
                    occ.id, occ.reminder_title
                );
            }
        }
    }
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PendingLaptopOccurrence;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeServerState {
        pending: Arc<Mutex<Vec<PendingLaptopOccurrence>>>,
        acked: Arc<Mutex<Vec<i64>>>,
    }

    async fn start_fake_server(initial_pending: Vec<PendingLaptopOccurrence>) -> (String, FakeServerState) {
        let state = FakeServerState {
            pending: Arc::new(Mutex::new(initial_pending)),
            acked: Arc::new(Mutex::new(Vec::new())),
        };

        async fn pending_handler(
            State(state): State<FakeServerState>,
        ) -> Json<Vec<PendingLaptopOccurrence>> {
            Json(state.pending.lock().unwrap().clone())
        }

        async fn ack_handler(State(state): State<FakeServerState>, Path(id): Path<i64>) -> StatusCode {
            state.acked.lock().unwrap().push(id);
            state.pending.lock().unwrap().retain(|o| o.id != id);
            StatusCode::NO_CONTENT
        }

        let app = Router::new()
            .route("/occurrences/pending-laptop", get(pending_handler))
            .route("/occurrences/:id/ack-laptop", post(ack_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{addr}"), state)
    }

    struct CountingNotifier {
        calls: Arc<Mutex<Vec<String>>>,
        fail_titles: Vec<String>,
    }

    impl LocalNotifier for CountingNotifier {
        async fn notify(&self, title: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(title.to_string());
            if self.fail_titles.iter().any(|t| t == title) {
                anyhow::bail!("simulated local notify failure for {title}");
            }
            Ok(())
        }
    }

    fn sample(id: i64, title: &str) -> PendingLaptopOccurrence {
        PendingLaptopOccurrence {
            id,
            reminder_id: 100 + id,
            reminder_title: title.to_string(),
            occurrence_date: "2026-09-25".into(),
            fired_at: "2026-09-25T18:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn delivers_and_acks_every_pending_occurrence() {
        let (server_url, state) =
            start_fake_server(vec![sample(1, "Study Go"), sample(2, "Exercise")]).await;
        let client = ApiClient::new(server_url, "unused-in-fake-server".into());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let notifier = CountingNotifier { calls: calls.clone(), fail_titles: vec![] };

        let delivered = poll_once(&client, &notifier).await.unwrap();

        assert_eq!(delivered, 2);
        assert_eq!(*calls.lock().unwrap(), vec!["Study Go", "Exercise"]);
        assert_eq!(state.acked.lock().unwrap().len(), 2);
        assert!(state.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_local_notify_is_not_acked_and_stays_pending() {
        let (server_url, state) = start_fake_server(vec![sample(1, "Study Go")]).await;
        let client = ApiClient::new(server_url, "unused-in-fake-server".into());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let notifier =
            CountingNotifier { calls: calls.clone(), fail_titles: vec!["Study Go".into()] };

        let delivered = poll_once(&client, &notifier).await.unwrap();

        assert_eq!(delivered, 0);
        assert!(state.acked.lock().unwrap().is_empty());
        // Still there for the next poll to retry.
        assert_eq!(state.pending.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn nothing_pending_is_not_an_error() {
        let (server_url, _state) = start_fake_server(vec![]).await;
        let client = ApiClient::new(server_url, "unused-in-fake-server".into());
        let notifier = CountingNotifier { calls: Arc::new(Mutex::new(Vec::new())), fail_titles: vec![] };

        assert_eq!(poll_once(&client, &notifier).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn omarchy_notifier_reports_a_clear_error_when_command_is_missing() {
        let notifier = OmarchyNotifier::new("definitely-not-a-real-command-xyz".into());
        let err = notifier.notify("Study Go").await.unwrap_err();
        assert!(err.to_string().contains("definitely-not-a-real-command-xyz"));
    }
}

//! Notification delivery, kept deliberately separate from the
//! scheduler (see `scheduler.rs`'s doc comment and Phase 5 of the
//! project brief: "Don't make `omarchy reminder` part of the core
//! scheduler"). The scheduler's only job is deciding *when* a
//! reminder fires and recording that fact; this module's only job is
//! trying to push a notification for occurrences that have already
//! fired but haven't been delivered yet.
//!
//! # Why a trait at all, with only one implementation
//!
//! [`NtfyBackend`](ntfy::NtfyBackend) is the only backend today, so a
//! trait might look premature. It earns its place because it's what
//! keeps [`retry_pending`] -- and everything upstream of it, like
//! `run_serve` in `main.rs` -- from knowing anything about ntfy
//! specifically. Adding a second backend later (Pushover, say) means
//! writing one new file and changing one line in `main.rs`, not
//! touching the retry loop or the scheduler.
//!
//! The trait method is a plain `async fn` (stable since Rust 1.75),
//! not a boxed/dyn-compatible one, so [`retry_pending`] is generic
//! over the backend type rather than taking a trait object. Since
//! `main.rs` only ever runs one backend per process (selected once at
//! startup from config, not switched at runtime), static dispatch is
//! simpler and needs no extra crate (`async-trait`) to get there.
//!
//! # Why a flat retry interval instead of real exponential backoff
//!
//! The obvious "more correct" design tracks a last-attempt timestamp
//! per occurrence and computes an increasing delay before the next
//! retry is eligible. That's meaningfully more bookkeeping (a new
//! column, a query with a computed WHERE clause) to protect against a
//! problem this project doesn't really have: a personal ntfy topic
//! with a handful of reminders a day isn't going to hammer anyone's
//! server. Retrying every pending occurrence on every fixed-interval
//! pass, capped at [`MAX_NTFY_ATTEMPTS`] tries, gets the same practical
//! outcome -- delivery resumes automatically once ntfy is reachable
//! again, and a permanently-unreachable topic stops being retried
//! forever -- for a fraction of the code.

pub mod ntfy;

use crate::db::{self, Db};

/// Give up retrying an occurrence after this many attempts. Without a
/// cap, a topic that's misconfigured or permanently unreachable would
/// have the retry loop hammering it (and logging failures) forever.
/// The reminder itself is unaffected either way -- `reminder list` and
/// the rest of the API never look at `ntfy_status`.
pub const MAX_NTFY_ATTEMPTS: i64 = 10;

/// Anything that can push a single notification. `title` is the
/// reminder's title; `body` is a fixed short message -- there's no
/// per-reminder body text in this app's data model, so there's nothing
/// more specific to say.
pub trait NotificationBackend: Send + Sync {
    async fn send(&self, title: &str, body: &str) -> anyhow::Result<()>;
}

/// One pass over every occurrence still waiting on a notification:
/// attempt delivery through `backend`, and record the outcome. Safe to
/// call on a fixed interval indefinitely -- this is the entire
/// "notification retry" mechanism (see the module doc comment for why
/// there's nothing fancier here).
///
/// Never returns `Err` for an individual delivery failure -- those are
/// logged and recorded in `ntfy_status`, not propagated, because one
/// bad send shouldn't stop the rest of the batch from being attempted.
/// It only returns `Err` for a failure to even query/update the
/// database, which is a real "something is wrong" condition.
pub async fn retry_pending<B: NotificationBackend>(db: &Db, backend: &B) -> anyhow::Result<()> {
    for (occ, title) in db::occurrences_pending_ntfy_with_title(db).await? {
        if occ.ntfy_attempts >= MAX_NTFY_ATTEMPTS {
            continue;
        }
        let attempts = occ.ntfy_attempts + 1;
        match backend.send(&title, "It's time!").await {
            Ok(()) => db::mark_ntfy_status(db, occ.id, "sent", attempts).await?,
            Err(e) => {
                eprintln!("ntfy: failed to notify for occurrence {} ({title}): {e:#}", occ.id);
                db::mark_ntfy_status(db, occ.id, "failed", attempts).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewReminder;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingBackend {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl NotificationBackend for CountingBackend {
        async fn send(&self, _title: &str, _body: &str) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("simulated failure");
            }
            Ok(())
        }
    }

    async fn seeded_db_with_one_occurrence() -> (Db, i64) {
        let db = Db::connect(":memory:").await.unwrap();
        let reminder = db::create_reminder(
            &db,
            &NewReminder {
                title: "Study Go".into(),
                time: "18:00".into(),
                start_date: "2026-09-25".into(),
                repeat_days: 0,
                occurrences_total: None,
                end_date: None,
            },
        )
        .await
        .unwrap();
        let (occ, _) = db::record_occurrence(&db, reminder.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();
        (db, occ.id)
    }

    #[tokio::test]
    async fn successful_send_marks_occurrence_sent() {
        let (db, occ_id) = seeded_db_with_one_occurrence().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = CountingBackend { calls: calls.clone(), fail: false };

        retry_pending(&db, &backend).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(db::occurrences_pending_ntfy(&db).await.unwrap().len(), 0);
        let _ = occ_id;
    }

    #[tokio::test]
    async fn failed_send_leaves_occurrence_pending_for_next_pass() {
        let (db, _occ_id) = seeded_db_with_one_occurrence().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = CountingBackend { calls: calls.clone(), fail: true };

        retry_pending(&db, &backend).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(db::occurrences_pending_ntfy(&db).await.unwrap().len(), 1);

        // A second pass retries it again (this is the whole retry
        // mechanism: the next fixed-interval call just tries again).
        retry_pending(&db, &backend).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let (db, occ_id) = seeded_db_with_one_occurrence().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = CountingBackend { calls: calls.clone(), fail: true };

        for _ in 0..MAX_NTFY_ATTEMPTS {
            retry_pending(&db, &backend).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), MAX_NTFY_ATTEMPTS as usize);

        // One more pass: attempts == MAX_NTFY_ATTEMPTS now, so it's
        // skipped rather than retried again.
        retry_pending(&db, &backend).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_NTFY_ATTEMPTS as usize);

        // Still shows up as "not sent" -- giving up on push delivery
        // never hides the reminder from the rest of the app.
        assert_eq!(db::occurrences_pending_ntfy(&db).await.unwrap().len(), 1);
        let _ = occ_id;
    }
}

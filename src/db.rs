//! All SQL lives here. Nothing outside this module writes a query --
//! that keeps the schema's actual shape in one place.
//!
//! # Why one mutex-guarded connection instead of a pool
//!
//! SQLite allows many concurrent readers but only ever one writer at a
//! time; a connection pool mostly just moves that same serialization
//! into lock contention on the database file itself (`SQLITE_BUSY`)
//! instead of in-process. At this project's scale -- one user, at most
//! a handful of requests in flight -- a single `Mutex<Connection>` is
//! simpler to reason about, gives the same effective throughput, and
//! has no pool-sizing or connection-timeout knobs to get wrong.
//!
//! Every function here is `async fn`, but the actual SQLite work runs
//! synchronously inside `tokio::task::spawn_blocking`. That keeps the
//! call sites in `scheduler.rs` / `server/routes.rs` uniformly async
//! (so they compose with `axum` handlers and `tokio::time::sleep`
//! without extra ceremony) while being honest that rusqlite itself is
//! a blocking library under the hood.
//!
//! # Why hand-written SQL instead of an ORM/query builder
//!
//! The query surface here is small and fixed (a handful of CRUD
//! statements). A query builder would add a dependency and a layer of
//! indirection to save typing on SQL that's already about as simple as
//! it can be -- not a good trade for a project whose explicit goal is
//! staying easy to read end to end.

use crate::model::{NewReminder, Occurrence, Reminder, ReminderPatch};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

/// Migrations are plain SQL files, applied in order and tracked via
/// SQLite's built-in `PRAGMA user_version` -- no separate migrations
/// table, no external migration tool needed.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// Open (creating if necessary) the SQLite database at `path` and
    /// run any pending migrations. `path` may be `":memory:"` for
    /// tests.
    pub async fn connect(path: &str) -> anyhow::Result<Db> {
        let path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = if path == ":memory:" {
                Connection::open_in_memory()?
            } else {
                Connection::open(&path)?
            };

            conn.pragma_update(None, "foreign_keys", true)?;
            if path != ":memory:" {
                // WAL lets the scheduler tick, API requests, and the
                // notification retry pass share the database without
                // blocking each other on every statement. Irrelevant
                // for :memory: (there's no file to write a WAL for).
                conn.pragma_update(None, "journal_mode", "WAL")?;
            }

            run_migrations(&conn)?;

            Ok(Db(Arc::new(Mutex::new(conn))))
        })
        .await?
    }

    /// Run a closure with exclusive access to the connection, off the
    /// async runtime's worker threads. Every public function below is
    /// a thin wrapper around this.
    async fn with_conn<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let conn = db.lock().expect("db mutex poisoned");
            f(&conn).map_err(anyhow::Error::from)
        })
        .await?
    }
}

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > current {
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", version)?;
        }
    }
    Ok(())
}

const REMINDER_COLUMNS: &str = "id, title, time, start_date, repeat_days, \
     occurrences_total, end_date, enabled, created_at, updated_at";

const OCCURRENCE_COLUMNS: &str =
    "id, reminder_id, occurrence_date, fired_at, completed, ntfy_status, ntfy_attempts, laptop_status";

// ---------------------------------------------------------------------
// Reminders
// ---------------------------------------------------------------------

pub async fn create_reminder(db: &Db, new: &NewReminder) -> anyhow::Result<Reminder> {
    let new = new.clone();
    db.with_conn(move |conn| {
        conn.query_row(
            &format!(
                "INSERT INTO reminders (title, time, start_date, repeat_days, occurrences_total, end_date)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 RETURNING {REMINDER_COLUMNS}"
            ),
            params![
                new.title,
                new.time,
                new.start_date,
                new.repeat_days,
                new.occurrences_total,
                new.end_date,
            ],
            Reminder::from_row,
        )
    })
    .await
}

pub async fn get_reminder(db: &Db, id: i64) -> anyhow::Result<Option<Reminder>> {
    db.with_conn(move |conn| {
        conn.query_row(
            &format!("SELECT {REMINDER_COLUMNS} FROM reminders WHERE id = ?1"),
            params![id],
            Reminder::from_row,
        )
        .optional()
    })
    .await
}

/// List reminders. `only_enabled = true` is what the scheduler uses;
/// the CLI/TUI list view passes `false` to show everything.
pub async fn list_reminders(db: &Db, only_enabled: bool) -> anyhow::Result<Vec<Reminder>> {
    db.with_conn(move |conn| {
        let where_clause = if only_enabled { "WHERE enabled = 1" } else { "" };
        let mut stmt = conn.prepare(&format!(
            "SELECT {REMINDER_COLUMNS} FROM reminders {where_clause} ORDER BY time"
        ))?;
        let rows = stmt.query_map([], Reminder::from_row)?;
        rows.collect()
    })
    .await
}

pub async fn update_reminder(
    db: &Db,
    id: i64,
    patch: &ReminderPatch,
) -> anyhow::Result<Option<Reminder>> {
    let patch = patch.clone();
    db.with_conn(move |conn| {
        // A hand-rolled dynamic SET clause would be more "clever," but
        // COALESCE against a sentinel is simpler to read and test, and
        // the query planner cost here is irrelevant at personal-project
        // scale. `occurrences_total`/`end_date` need the CASE form
        // instead of plain COALESCE because `Some(None)` (explicit
        // clear) must be distinguishable from `None` (leave as is).
        let occurrences_total_set = patch.occurrences_total.is_some();
        let occurrences_total_val = patch.occurrences_total.flatten();
        let end_date_set = patch.end_date.is_some();
        let end_date_val = patch.end_date.clone().flatten();

        conn.query_row(
            &format!(
                "UPDATE reminders SET
                    title              = COALESCE(?1, title),
                    time               = COALESCE(?2, time),
                    start_date         = COALESCE(?3, start_date),
                    repeat_days        = COALESCE(?4, repeat_days),
                    occurrences_total  = CASE WHEN ?5 THEN ?6 ELSE occurrences_total END,
                    end_date           = CASE WHEN ?7 THEN ?8 ELSE end_date END,
                    enabled            = COALESCE(?9, enabled),
                    updated_at         = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?10
                 RETURNING {REMINDER_COLUMNS}"
            ),
            params![
                patch.title,
                patch.time,
                patch.start_date,
                patch.repeat_days,
                occurrences_total_set,
                occurrences_total_val,
                end_date_set,
                end_date_val,
                patch.enabled,
                id,
            ],
            Reminder::from_row,
        )
        .optional()
    })
    .await
}

pub async fn delete_reminder(db: &Db, id: i64) -> anyhow::Result<bool> {
    db.with_conn(move |conn| {
        let affected = conn.execute("DELETE FROM reminders WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    })
    .await
}

// ---------------------------------------------------------------------
// Occurrences
// ---------------------------------------------------------------------

/// Record that a reminder fired for `occurrence_date`. Idempotent by
/// design: if a row for this (reminder_id, occurrence_date) already
/// exists -- e.g. the scheduler ticked twice before the first write
/// committed, or simply because it's still the same day -- this is a
/// no-op that returns the existing row rather than erroring, so
/// callers never need their own "already fired?" check. This, together
/// with the UNIQUE constraint in the schema, is the entire
/// duplicate-notification guard.
///
/// Returns `(occurrence, was_newly_recorded)`. Callers that trigger
/// side effects on firing (printing, pushing a notification) should
/// only act when `was_newly_recorded` is true -- otherwise every tick
/// for the rest of the day would re-trigger them, even though the
/// database itself only ever holds the one row.
pub async fn record_occurrence(
    db: &Db,
    reminder_id: i64,
    occurrence_date: &str,
    fired_at: &str,
) -> anyhow::Result<(Occurrence, bool)> {
    let occurrence_date = occurrence_date.to_string();
    let fired_at = fired_at.to_string();
    db.with_conn(move |conn| {
        let rows_inserted = conn.execute(
            "INSERT INTO occurrences (reminder_id, occurrence_date, fired_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (reminder_id, occurrence_date) DO NOTHING",
            params![reminder_id, occurrence_date, fired_at],
        )?;

        let occ = conn.query_row(
            &format!(
                "SELECT {OCCURRENCE_COLUMNS} FROM occurrences
                 WHERE reminder_id = ?1 AND occurrence_date = ?2"
            ),
            params![reminder_id, occurrence_date],
            Occurrence::from_row,
        )?;

        Ok((occ, rows_inserted > 0))
    })
    .await
}

pub async fn occurrences_pending_ntfy(db: &Db) -> anyhow::Result<Vec<Occurrence>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {OCCURRENCE_COLUMNS} FROM occurrences
             WHERE ntfy_status != 'sent' ORDER BY fired_at"
        ))?;
        let rows = stmt.query_map([], Occurrence::from_row)?;
        rows.collect()
    })
    .await
}

/// Like [`occurrences_pending_ntfy`], but joined with `reminders` for
/// the title -- an `Occurrence` alone doesn't carry enough to write a
/// notification. Used only by the ntfy retry loop, which is the one
/// caller that needs the title.
pub async fn occurrences_pending_ntfy_with_title(
    db: &Db,
) -> anyhow::Result<Vec<(Occurrence, String)>> {
    db.with_conn(|conn| {
        let occ_cols_prefixed = "o.id, o.reminder_id, o.occurrence_date, o.fired_at, o.completed, \
             o.ntfy_status, o.ntfy_attempts, o.laptop_status";
        let mut stmt = conn.prepare(&format!(
            "SELECT {occ_cols_prefixed}, r.title
             FROM occurrences o JOIN reminders r ON r.id = o.reminder_id
             WHERE o.ntfy_status != 'sent'
             ORDER BY o.fired_at"
        ))?;
        let rows = stmt.query_map([], |row| {
            let occ = Occurrence::from_row(row)?;
            let title: String = row.get(8)?;
            Ok((occ, title))
        })?;
        rows.collect()
    })
    .await
}

pub async fn occurrences_pending_laptop(db: &Db) -> anyhow::Result<Vec<Occurrence>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {OCCURRENCE_COLUMNS} FROM occurrences
             WHERE laptop_status = 'pending' ORDER BY fired_at"
        ))?;
        let rows = stmt.query_map([], Occurrence::from_row)?;
        rows.collect()
    })
    .await
}

/// Like [`occurrences_pending_laptop`], but joined with `reminders`
/// for the title -- used by `GET /occurrences/pending-laptop`, whose
/// only consumer (`reminder watch`) needs something to show, not the
/// full occurrence row.
pub async fn occurrences_pending_laptop_with_title(
    db: &Db,
) -> anyhow::Result<Vec<crate::model::PendingLaptopOccurrence>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT o.id, o.reminder_id, r.title, o.occurrence_date, o.fired_at
             FROM occurrences o JOIN reminders r ON r.id = o.reminder_id
             WHERE o.laptop_status = 'pending'
             ORDER BY o.fired_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::model::PendingLaptopOccurrence {
                id: row.get(0)?,
                reminder_id: row.get(1)?,
                reminder_title: row.get(2)?,
                occurrence_date: row.get(3)?,
                fired_at: row.get(4)?,
            })
        })?;
        rows.collect()
    })
    .await
}

pub async fn mark_ntfy_status(
    db: &Db,
    occurrence_id: i64,
    status: &str,
    attempts: i64,
) -> anyhow::Result<()> {
    let status = status.to_string();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE occurrences SET ntfy_status = ?1, ntfy_attempts = ?2 WHERE id = ?3",
            params![status, attempts, occurrence_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_laptop_delivered(db: &Db, occurrence_id: i64) -> anyhow::Result<()> {
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE occurrences SET laptop_status = 'delivered' WHERE id = ?1",
            params![occurrence_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_completed(db: &Db, occurrence_id: i64) -> anyhow::Result<()> {
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE occurrences SET completed = 1 WHERE id = ?1",
            params![occurrence_id],
        )?;
        Ok(())
    })
    .await
}

/// How many occurrences a reminder has fired so far. Used by the
/// scheduler to enforce `occurrences_total`.
pub async fn count_occurrences(db: &Db, reminder_id: i64) -> anyhow::Result<i64> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM occurrences WHERE reminder_id = ?1",
            params![reminder_id],
            |r| r.get(0),
        )
    })
    .await
}

/// The most recently fired occurrence for a reminder, if any. Used by
/// `POST /reminders/:id/complete` -- "mark this reminder done" means
/// "mark its latest occurrence completed."
pub async fn latest_occurrence_for_reminder(
    db: &Db,
    reminder_id: i64,
) -> anyhow::Result<Option<Occurrence>> {
    db.with_conn(move |conn| {
        conn.query_row(
            &format!(
                "SELECT {OCCURRENCE_COLUMNS} FROM occurrences
                 WHERE reminder_id = ?1
                 ORDER BY occurrence_date DESC LIMIT 1"
            ),
            params![reminder_id],
            Occurrence::from_row,
        )
        .optional()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReminderPatch;

    async fn test_db() -> Db {
        Db::connect(":memory:").await.expect("in-memory db should open")
    }

    fn sample_new_reminder() -> NewReminder {
        NewReminder {
            title: "Study Go".into(),
            time: "18:00".into(),
            start_date: "2026-09-25".into(),
            repeat_days: 0,
            occurrences_total: None,
            end_date: None,
        }
    }

    #[tokio::test]
    async fn create_and_get_reminder_round_trips() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        assert_eq!(created.title, "Study Go");
        assert!(created.enabled);

        let fetched = get_reminder(&db, created.id).await.unwrap().unwrap();
        assert_eq!(fetched, created);
    }

    #[tokio::test]
    async fn get_missing_reminder_returns_none() {
        let db = test_db().await;
        assert!(get_reminder(&db, 999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_reminders_filters_by_enabled() {
        let db = test_db().await;
        let a = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        let mut second = sample_new_reminder();
        second.title = "Exercise".into();
        let b = create_reminder(&db, &second).await.unwrap();

        update_reminder(
            &db,
            b.id,
            &ReminderPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let all = list_reminders(&db, false).await.unwrap();
        assert_eq!(all.len(), 2);

        let enabled_only = list_reminders(&db, true).await.unwrap();
        assert_eq!(enabled_only.len(), 1);
        assert_eq!(enabled_only[0].id, a.id);
    }

    #[tokio::test]
    async fn update_reminder_patches_only_given_fields() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();

        let patched = update_reminder(
            &db,
            created.id,
            &ReminderPatch {
                time: Some("19:30".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(patched.time, "19:30");
        assert_eq!(patched.title, created.title); // untouched
    }

    #[tokio::test]
    async fn update_reminder_can_clear_nullable_fields() {
        let db = test_db().await;
        let mut new = sample_new_reminder();
        new.end_date = Some("2026-12-01".into());
        let created = create_reminder(&db, &new).await.unwrap();
        assert_eq!(created.end_date, Some("2026-12-01".into()));

        let patched = update_reminder(
            &db,
            created.id,
            &ReminderPatch {
                end_date: Some(None), // explicit clear
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(patched.end_date, None);
    }

    #[tokio::test]
    async fn delete_reminder_removes_it() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();

        assert!(delete_reminder(&db, created.id).await.unwrap());
        assert!(get_reminder(&db, created.id).await.unwrap().is_none());
        // Deleting again is a no-op, not an error.
        assert!(!delete_reminder(&db, created.id).await.unwrap());
    }

    #[tokio::test]
    async fn deleting_reminder_cascades_to_occurrences() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();

        delete_reminder(&db, created.id).await.unwrap();

        let count = count_occurrences(&db, created.id).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn record_occurrence_is_idempotent() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();

        let (first, first_new) =
            record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
                .await
                .unwrap();
        // Simulate the scheduler ticking twice for the same day, e.g.
        // after a crash/restart mid-tick.
        let (second, second_new) =
            record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:05Z")
                .await
                .unwrap();

        assert!(first_new);
        assert!(!second_new); // the second call found an existing row
        assert_eq!(first.id, second.id);
        assert_eq!(first.fired_at, second.fired_at); // fired_at NOT overwritten
        assert_eq!(count_occurrences(&db, created.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn occurrences_pending_ntfy_excludes_sent() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        let (occ, _) = record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();

        assert_eq!(occurrences_pending_ntfy(&db).await.unwrap().len(), 1);

        mark_ntfy_status(&db, occ.id, "sent", 1).await.unwrap();
        assert_eq!(occurrences_pending_ntfy(&db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn occurrences_pending_ntfy_with_title_joins_reminder_title() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();

        let pending = occurrences_pending_ntfy_with_title(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "Study Go"); // sample_new_reminder()'s title

        mark_ntfy_status(&db, pending[0].0.id, "sent", 1).await.unwrap();
        assert_eq!(
            occurrences_pending_ntfy_with_title(&db).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn occurrences_pending_laptop_excludes_delivered() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        let (occ, _) = record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();

        assert_eq!(occurrences_pending_laptop(&db).await.unwrap().len(), 1);

        mark_laptop_delivered(&db, occ.id).await.unwrap();
        assert_eq!(occurrences_pending_laptop(&db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn occurrences_pending_laptop_with_title_joins_reminder_title() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        let (occ, _) = record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();

        let pending = occurrences_pending_laptop_with_title(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, occ.id);
        assert_eq!(pending[0].reminder_title, "Study Go");

        mark_laptop_delivered(&db, occ.id).await.unwrap();
        assert_eq!(
            occurrences_pending_laptop_with_title(&db).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn mark_completed_sets_flag() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();
        let (occ, _) = record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
            .await
            .unwrap();
        assert!(!occ.completed);

        mark_completed(&db, occ.id).await.unwrap();

        let recs = occurrences_pending_ntfy(&db).await.unwrap();
        assert!(recs[0].completed);
    }

    #[tokio::test]
    async fn latest_occurrence_returns_most_recent_by_date() {
        let db = test_db().await;
        let created = create_reminder(&db, &sample_new_reminder()).await.unwrap();

        assert!(latest_occurrence_for_reminder(&db, created.id)
            .await
            .unwrap()
            .is_none());

        record_occurrence(&db, created.id, "2026-09-20", "2026-09-20T18:00:00Z")
            .await
            .unwrap();
        let (latest, _) =
            record_occurrence(&db, created.id, "2026-09-25", "2026-09-25T18:00:00Z")
                .await
                .unwrap();

        let found = latest_occurrence_for_reminder(&db, created.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, latest.id);
        assert_eq!(found.occurrence_date, "2026-09-25");
    }
}

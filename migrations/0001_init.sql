-- Reminders: the recurring/one-shot definitions the user manages directly.
CREATE TABLE reminders (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    title               TEXT NOT NULL,
    time                TEXT NOT NULL,              -- "HH:MM", local wall-clock (24h)
    start_date          TEXT NOT NULL,               -- "YYYY-MM-DD"
    repeat_days         INTEGER NOT NULL DEFAULT 0,  -- bitmask, bit0=Mon..bit6=Sun, 0=one-shot
    occurrences_total   INTEGER,                     -- NULL = unbounded (ignored if end_date set)
    end_date            TEXT,                        -- "YYYY-MM-DD", NULL = unbounded
    enabled             INTEGER NOT NULL DEFAULT 1,   -- 0/1
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    CHECK (occurrences_total IS NULL OR end_date IS NULL)
);

-- Occurrences: one row per instance the scheduler has actually fired.
-- The UNIQUE constraint below is the real duplicate-notification guard --
-- "already fired" is just "does this row exist", checked by the DB itself.
CREATE TABLE occurrences (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    reminder_id      INTEGER NOT NULL REFERENCES reminders(id) ON DELETE CASCADE,
    occurrence_date  TEXT NOT NULL,                -- "YYYY-MM-DD", the local day it fired for
    fired_at         TEXT NOT NULL,                -- UTC timestamp, set once, never updated
    completed        INTEGER NOT NULL DEFAULT 0,   -- 0/1, user-marked done
    ntfy_status      TEXT NOT NULL DEFAULT 'pending',   -- pending|sent|failed
    ntfy_attempts    INTEGER NOT NULL DEFAULT 0,
    laptop_status    TEXT NOT NULL DEFAULT 'pending',   -- pending|delivered

    UNIQUE (reminder_id, occurrence_date)
);

CREATE INDEX idx_reminders_enabled ON reminders(enabled);
CREATE INDEX idx_occurrences_ntfy_pending ON occurrences(ntfy_status) WHERE ntfy_status != 'sent';
CREATE INDEX idx_occurrences_laptop_pending ON occurrences(laptop_status) WHERE laptop_status = 'pending';

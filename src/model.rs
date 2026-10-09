//! Core data types shared by the scheduler, the API, and the CLI/TUI.
//!
//! Kept deliberately dumb: these are rows, not domain objects with
//! behavior. Scheduling logic (next-fire calculation) lives in
//! `scheduler.rs`, not here, so this module has no time-of-day logic
//! to get wrong.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

/// One bit per weekday. `repeat_days == 0` means "one-shot: fires once,
/// on `start_date`, and never again."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Weekday {
    Mon = 0b0000_0001,
    Tue = 0b0000_0010,
    Wed = 0b0000_0100,
    Thu = 0b0000_1000,
    Fri = 0b0001_0000,
    Sat = 0b0010_0000,
    Sun = 0b0100_0000,
}

impl Weekday {
    /// Convert from chrono's `Weekday` so callers never have to hand-roll
    /// the Mon=0..Sun=6 mapping themselves.
    pub fn from_chrono(w: chrono::Weekday) -> Self {
        match w {
            chrono::Weekday::Mon => Weekday::Mon,
            chrono::Weekday::Tue => Weekday::Tue,
            chrono::Weekday::Wed => Weekday::Wed,
            chrono::Weekday::Thu => Weekday::Thu,
            chrono::Weekday::Fri => Weekday::Fri,
            chrono::Weekday::Sat => Weekday::Sat,
            chrono::Weekday::Sun => Weekday::Sun,
        }
    }

    /// Parse a case-insensitive three-letter (or full) weekday name,
    /// e.g. "mon" or "Monday". Used by the CLI's `--repeat` flag.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mon" | "monday" => Some(Weekday::Mon),
            "tue" | "tues" | "tuesday" => Some(Weekday::Tue),
            "wed" | "weds" | "wednesday" => Some(Weekday::Wed),
            "thu" | "thur" | "thurs" | "thursday" => Some(Weekday::Thu),
            "fri" | "friday" => Some(Weekday::Fri),
            "sat" | "saturday" => Some(Weekday::Sat),
            "sun" | "sunday" => Some(Weekday::Sun),
            _ => None,
        }
    }
}

/// Parse a comma-separated weekday list ("mon,wed,fri") into a
/// `repeat_days` bitmask. Used by the CLI; returns the offending token
/// on failure so the error message can point at it.
pub fn parse_weekday_list(s: &str) -> Result<i64, String> {
    let mut mask = 0i64;
    for token in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match Weekday::from_str_loose(token) {
            Some(w) => mask |= w as i64,
            None => return Err(format!("'{token}' is not a weekday (try mon, tue, wed, ...)")),
        }
    }
    Ok(mask)
}

/// The inverse of [`parse_weekday_list`]: render a `repeat_days`
/// bitmask back as "mon,wed,fri", in weekday order. Used by the TUI to
/// both display a reminder's schedule and pre-fill its edit form.
/// Returns an empty string for a one-shot reminder (`mask == 0`).
pub fn mask_to_weekday_str(mask: i64) -> String {
    const ORDERED: [(Weekday, &str); 7] = [
        (Weekday::Mon, "mon"),
        (Weekday::Tue, "tue"),
        (Weekday::Wed, "wed"),
        (Weekday::Thu, "thu"),
        (Weekday::Fri, "fri"),
        (Weekday::Sat, "sat"),
        (Weekday::Sun, "sun"),
    ];
    ORDERED
        .iter()
        .filter(|(w, _)| mask & (*w as i64) != 0)
        .map(|(_, s)| *s)
        .collect::<Vec<_>>()
        .join(",")
}

/// Build a `repeat_days` bitmask from a set of weekdays, e.g.
/// `weekday_mask([Weekday::Mon, Weekday::Wed, Weekday::Fri])`.
pub fn weekday_mask(days: &[Weekday]) -> i64 {
    days.iter().fold(0i64, |acc, d| acc | (*d as i64))
}

/// Whether `date` falls on one of the days set in `mask`.
pub fn mask_includes(mask: i64, date: NaiveDate) -> bool {
    mask & (Weekday::from_chrono(date.weekday()) as i64) != 0
}

/// A reminder definition as stored in the `reminders` table.
///
/// `time` and `start_date`/`end_date` are stored as plain text
/// (`HH:MM` / `YYYY-MM-DD`) rather than as UTC timestamps: the whole
/// point is that they mean "6pm, local time, whatever the local
/// timezone/DST offset happens to be *on that day*" -- resolving that
/// to UTC is the scheduler's job, done at tick time, not stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    pub id: i64,
    pub title: String,
    pub time: String,
    pub start_date: String,
    pub repeat_days: i64,
    pub occurrences_total: Option<i64>,
    pub end_date: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Reminder {
    /// Build a `Reminder` from a `SELECT id, title, time, start_date,
    /// repeat_days, occurrences_total, end_date, enabled, created_at,
    /// updated_at` row, in that column order. Kept as one function so
    /// every query in `db.rs` stays in sync with this column list.
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Reminder {
            id: row.get(0)?,
            title: row.get(1)?,
            time: row.get(2)?,
            start_date: row.get(3)?,
            repeat_days: row.get(4)?,
            occurrences_total: row.get(5)?,
            end_date: row.get(6)?,
            enabled: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }
}

/// Fields needed to create a reminder. Separate from `Reminder` so
/// callers can't accidentally set `id`/`created_at`/etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewReminder {
    pub title: String,
    pub time: String,
    pub start_date: String,
    pub repeat_days: i64,
    pub occurrences_total: Option<i64>,
    pub end_date: Option<String>,
}

/// Partial update: every field is optional, `None` means "leave as is."
/// Used by both the `PUT /reminders/:id` handler and `reminder edit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReminderPatch {
    pub title: Option<String>,
    pub time: Option<String>,
    pub start_date: Option<String>,
    pub repeat_days: Option<i64>,
    pub occurrences_total: Option<Option<i64>>, // Some(None) = clear it
    pub end_date: Option<Option<String>>,       // Some(None) = clear it
    pub enabled: Option<bool>,
}

/// A single fired instance of a reminder, as stored in `occurrences`.
///
/// The `UNIQUE(reminder_id, occurrence_date)` constraint on this table
/// (see migrations/0001_init.sql) is what actually prevents duplicate
/// notifications -- "has this already fired" is just "does this row
/// exist," enforced by SQLite itself rather than application logic.
/// An occurrence waiting to be delivered locally on a laptop/desktop
/// (via `reminder watch` -> `omarchy reminder`), joined with its
/// reminder's title -- what `GET /occurrences/pending-laptop` returns.
/// A separate, flatter type from `Occurrence` because the watch loop's
/// only use for this data is "what do I show, and what id do I ack,"
/// not the full occurrence row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLaptopOccurrence {
    pub id: i64,
    pub reminder_id: i64,
    pub reminder_title: String,
    pub occurrence_date: String,
    pub fired_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub id: i64,
    pub reminder_id: i64,
    pub occurrence_date: String,
    pub fired_at: String,
    pub completed: bool,
    pub ntfy_status: String,
    pub ntfy_attempts: i64,
    pub laptop_status: String,
}

impl Occurrence {
    /// Build an `Occurrence` from a `SELECT id, reminder_id,
    /// occurrence_date, fired_at, completed, ntfy_status,
    /// ntfy_attempts, laptop_status` row, in that column order.
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Occurrence {
            id: row.get(0)?,
            reminder_id: row.get(1)?,
            occurrence_date: row.get(2)?,
            fired_at: row.get(3)?,
            completed: row.get(4)?,
            ntfy_status: row.get(5)?,
            ntfy_attempts: row.get(6)?,
            laptop_status: row.get(7)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn weekday_mask_builds_expected_bits() {
        let mask = weekday_mask(&[Weekday::Mon, Weekday::Wed, Weekday::Fri]);
        assert_eq!(mask, 0b0001_0101);
    }

    #[test]
    fn mask_includes_matches_correct_days() {
        let weekdays_only = weekday_mask(&[
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ]);
        let monday = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(); // a Monday
        let saturday = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap(); // a Saturday

        assert!(mask_includes(weekdays_only, monday));
        assert!(!mask_includes(weekdays_only, saturday));
    }

    #[test]
    fn zero_mask_matches_no_days() {
        let sunday = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        assert!(!mask_includes(0, sunday));
    }

    #[test]
    fn parse_weekday_list_builds_correct_mask() {
        assert_eq!(
            parse_weekday_list("mon,wed,fri").unwrap(),
            weekday_mask(&[Weekday::Mon, Weekday::Wed, Weekday::Fri])
        );
    }

    #[test]
    fn parse_weekday_list_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(
            parse_weekday_list(" Mon , WED ,Fri").unwrap(),
            weekday_mask(&[Weekday::Mon, Weekday::Wed, Weekday::Fri])
        );
    }

    #[test]
    fn parse_weekday_list_rejects_unknown_token() {
        let err = parse_weekday_list("mon,funday").unwrap_err();
        assert!(err.contains("funday"));
    }

    #[test]
    fn parse_weekday_list_empty_string_is_zero() {
        assert_eq!(parse_weekday_list("").unwrap(), 0);
    }

    #[test]
    fn mask_to_weekday_str_orders_by_weekday_not_input_order() {
        let mask = parse_weekday_list("fri,mon,wed").unwrap();
        assert_eq!(mask_to_weekday_str(mask), "mon,wed,fri");
    }

    #[test]
    fn mask_to_weekday_str_empty_for_one_shot() {
        assert_eq!(mask_to_weekday_str(0), "");
    }

    #[test]
    fn weekday_mask_round_trips_through_string_and_back() {
        for input in ["mon", "mon,wed,fri", "sat,sun", "mon,tue,wed,thu,fri,sat,sun"] {
            let mask = parse_weekday_list(input).unwrap();
            let rendered = mask_to_weekday_str(mask);
            assert_eq!(parse_weekday_list(&rendered).unwrap(), mask);
        }
    }
}

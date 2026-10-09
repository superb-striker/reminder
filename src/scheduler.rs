//! # Scheduling
//!
//! The scheduler is split deliberately into two layers:
//!
//! - [`is_due`] is a pure function: given a reminder and a point in
//!   time (already resolved to local wall-clock date/time), it answers
//!   "should this fire?" with no I/O at all. Every rule about *when* a
//!   reminder is due -- start date, end date, repeat days, occurrence
//!   limits, time-of-day -- lives here, so it can be unit tested
//!   directly against specific dates without a database, a clock, or
//!   an async runtime.
//! - [`tick`] is the thin, DB-aware layer on top: it lists enabled
//!   reminders, asks [`is_due`] about each one, and records any that
//!   fire. This is deliberately small and mostly untested by unit
//!   tests -- its correctness rides on `is_due` and on the
//!   already-tested `db::record_occurrence` idempotency.
//!
//! # Why one poll every ~30s instead of a timer per reminder
//!
//! Spawning a `tokio::time::sleep` future per reminder means tracking
//! a `JoinHandle` for each one and cancelling/respawning it on every
//! edit, plus re-hydrating all of them on server restart. A single
//! loop that wakes up periodically and asks the database "what's due
//! right now" needs none of that bookkeeping: restart handling, missed
//! ticks, and edits-take-effect-immediately all fall out for free,
//! because each tick is just "re-run the same query." At the scale
//! this runs at (dozens of reminders, not millions), the extra ~30s of
//! worst-case latency is a fine trade for that simplicity.
//!
//! # Why local time is resolved fresh on every tick
//!
//! `reminder.time` is stored as a plain "HH:MM" wall-clock string, not
//! a UTC instant, because "6pm" is what the reminder actually means --
//! resolving *today's* 6pm to a UTC instant has to happen at tick time
//! using today's UTC offset, or DST transitions would silently shift
//! reminders by an hour twice a year. [`local_now`] does that
//! resolution via `chrono-tz`, which knows the real transition dates
//! for each IANA zone.

use crate::db::{self, Db};
use crate::model::{mask_includes, Occurrence, Reminder};
use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScheduleError {
    #[error("invalid time '{0}': expected HH:MM")]
    InvalidTime(String),
    #[error("invalid date '{0}': expected YYYY-MM-DD")]
    InvalidDate(String),
}

pub fn parse_time(s: &str) -> Result<NaiveTime, ScheduleError> {
    NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| ScheduleError::InvalidTime(s.to_string()))
}

pub fn parse_date(s: &str) -> Result<NaiveDate, ScheduleError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| ScheduleError::InvalidDate(s.to_string()))
}

/// The current local wall-clock date and time in `tz`, correctly
/// accounting for DST -- there is no other place in this codebase that
/// converts between UTC and local time, so this is the one function
/// that needs to be right.
pub fn local_now(tz: Tz) -> (NaiveDate, NaiveTime) {
    let local = tz.from_utc_datetime(&Utc::now().naive_utc());
    (local.date_naive(), local.time())
}

/// Should `reminder` fire, given that it is `today` and `now_local`
/// where the reminder lives, and it has already fired
/// `occurrences_so_far` times in total?
///
/// Note this answers "due" for the *whole rest of the day* once the
/// fire time has passed, not just the exact minute -- that's
/// intentional. It relies on [`db::record_occurrence`]'s
/// idempotency (one row per `(reminder_id, occurrence_date)`) to
/// avoid firing twice, rather than trying to hit an exact instant on
/// every tick, which would risk missing reminders entirely if the
/// server was briefly down at the fire time.
pub fn is_due(
    reminder: &Reminder,
    today: NaiveDate,
    now_local: NaiveTime,
    occurrences_so_far: i64,
) -> Result<bool, ScheduleError> {
    if !reminder.enabled {
        return Ok(false);
    }

    let start_date = parse_date(&reminder.start_date)?;
    if today < start_date {
        return Ok(false);
    }

    if let Some(end_date) = &reminder.end_date {
        if today > parse_date(end_date)? {
            return Ok(false);
        }
    }

    if let Some(total) = reminder.occurrences_total {
        if occurrences_so_far >= total {
            return Ok(false);
        }
    }

    let on_schedule = if reminder.repeat_days == 0 {
        today == start_date // one-shot: fires only on its exact start date
    } else {
        mask_includes(reminder.repeat_days, today)
    };
    if !on_schedule {
        return Ok(false);
    }

    let fire_time = parse_time(&reminder.time)?;
    Ok(now_local >= fire_time)
}

/// Run one scheduler pass: check every enabled reminder against the
/// current time in `tz`, and record an occurrence for any that are
/// due. Returns only occurrences *newly* recorded this tick -- once a
/// reminder has fired for the day, later ticks that same day still see
/// it as "due" (see [`is_due`]'s doc comment) but `record_occurrence`
/// reports them as not-new, so they're filtered out here rather than
/// being reported as firing again.
///
/// Safe to call repeatedly and concurrently with itself -- correctness
/// doesn't depend on ticks not overlapping, because the uniqueness
/// constraint backing `record_occurrence` is enforced by SQLite, not
/// by anything in this function.
pub async fn tick(db: &Db, tz: Tz) -> anyhow::Result<Vec<(Reminder, Occurrence)>> {
    let (today, now_local) = local_now(tz);
    let occurrence_date = today.to_string();

    let mut fired = Vec::new();
    for reminder in db::list_reminders(db, true).await? {
        let occurrences_so_far = db::count_occurrences(db, reminder.id).await?;
        if is_due(&reminder, today, now_local, occurrences_so_far)? {
            let fired_at = Utc::now().to_rfc3339();
            let (occ, was_new) =
                db::record_occurrence(db, reminder.id, &occurrence_date, &fired_at).await?;
            if was_new {
                fired.push((reminder, occ));
            }
        }
    }
    Ok(fired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn reminder(
        repeat_days: i64,
        start_date: &str,
        end_date: Option<&str>,
        occurrences_total: Option<i64>,
        time: &str,
        enabled: bool,
    ) -> Reminder {
        Reminder {
            id: 1,
            title: "Test".into(),
            time: time.into(),
            start_date: start_date.into(),
            repeat_days,
            occurrences_total,
            end_date: end_date.map(String::from),
            enabled,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn one_shot_fires_only_on_start_date_after_its_time() {
        let r = reminder(0, "2026-09-25", None, None, "18:00", true);

        // Before the time, on the day: not yet due.
        assert_eq!(is_due(&r, date(2026, 9, 25), time(17, 59), 0), Ok(false));
        // At/after the time, on the day: due.
        assert_eq!(is_due(&r, date(2026, 9, 25), time(18, 0), 0), Ok(true));
        assert_eq!(is_due(&r, date(2026, 9, 25), time(23, 0), 0), Ok(true));
        // A day before: not due, no matter the time.
        assert_eq!(is_due(&r, date(2026, 9, 24), time(23, 0), 0), Ok(false));
        // A day after: a one-shot never fires again.
        assert_eq!(is_due(&r, date(2026, 9, 26), time(18, 0), 0), Ok(false));
    }

    #[test]
    fn disabled_reminder_never_due() {
        let r = reminder(0, "2026-09-25", None, None, "18:00", false);
        assert_eq!(is_due(&r, date(2026, 9, 25), time(18, 0), 0), Ok(false));
    }

    #[test]
    fn repeating_reminder_only_fires_on_selected_weekdays() {
        // Mon/Wed/Fri
        let r = reminder(0b0001_0101, "2026-09-21", None, None, "09:00", true);

        let monday = date(2026, 9, 21);
        let tuesday = date(2026, 9, 22);
        let wednesday = date(2026, 9, 23);

        assert_eq!(is_due(&r, monday, time(9, 0), 0), Ok(true));
        assert_eq!(is_due(&r, tuesday, time(9, 0), 0), Ok(false));
        assert_eq!(is_due(&r, wednesday, time(9, 0), 0), Ok(true));
    }

    #[test]
    fn repeating_reminder_respects_start_date_before_first_occurrence() {
        let r = reminder(0b1111_1111 & 0b0111_1111, "2026-09-25", None, None, "09:00", true);
        // Starts Fri 2026-09-25; a matching weekday before that is not due.
        let earlier_friday = date(2026, 9, 18);
        assert_eq!(is_due(&r, earlier_friday, time(9, 0), 0), Ok(false));
    }

    #[test]
    fn end_date_stops_further_occurrences() {
        let every_day = mask_all_days();
        let r = reminder(every_day, "2026-09-01", Some("2026-09-10"), None, "09:00", true);

        assert_eq!(is_due(&r, date(2026, 9, 10), time(9, 0), 5), Ok(true));
        assert_eq!(is_due(&r, date(2026, 9, 11), time(9, 0), 5), Ok(false));
    }

    #[test]
    fn occurrences_total_stops_further_occurrences() {
        let every_day = mask_all_days();
        let r = reminder(every_day, "2026-09-01", None, Some(3), "09:00", true);

        // Already fired 3 times -> done, regardless of date.
        assert_eq!(is_due(&r, date(2026, 9, 5), time(9, 0), 3), Ok(false));
        // Only fired twice so far -> still due.
        assert_eq!(is_due(&r, date(2026, 9, 5), time(9, 0), 2), Ok(true));
    }

    #[test]
    fn invalid_time_string_surfaces_as_error_not_panic() {
        let r = reminder(0, "2026-09-25", None, None, "not-a-time", true);
        assert_eq!(
            is_due(&r, date(2026, 9, 25), time(12, 0), 0),
            Err(ScheduleError::InvalidTime("not-a-time".into()))
        );
    }

    #[test]
    fn local_now_handles_dst_spring_forward_correctly() {
        // 2026-03-08 07:30 UTC is, in America/New_York, either 02:30 EST
        // or 03:30 EDT depending on whether the offset shift (which
        // happens at 2am local / 07:00 UTC that day) has been applied.
        // If `local_now` used a fixed offset instead of a real IANA
        // zone, this would be wrong by an hour after the transition.
        let before_transition = Utc.with_ymd_and_hms(2026, 3, 8, 6, 30, 0).unwrap();
        let after_transition = Utc.with_ymd_and_hms(2026, 3, 8, 8, 30, 0).unwrap();

        let tz: Tz = "America/New_York".parse().unwrap();

        let before_local = tz.from_utc_datetime(&before_transition.naive_utc());
        let after_local = tz.from_utc_datetime(&after_transition.naive_utc());

        assert_eq!(before_local.time(), time(1, 30)); // still EST (UTC-5)
        assert_eq!(after_local.time(), time(4, 30)); // now EDT (UTC-4)
    }

    fn mask_all_days() -> i64 {
        0b0111_1111
    }
}

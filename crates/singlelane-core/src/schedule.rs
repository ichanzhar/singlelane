//! Clock, working hours and tick alignment (SPEC 8, G9).
//!
//! All functions take the current time as input; [`Clock`] is the injection point the
//! daemon implements with the system clock and tests implement with [`FixedClock`].
//!
//! Wall-clock rules in the configured time zone:
//! - work hours are the half-open interval `[start, end)`: a tick exactly at `end` is outside;
//! - ticks fall on local wall-clock times that are multiples of `poll_interval` after local
//!   midnight (`:00 :15 :30 :45` for 15 minutes);
//! - on a spring-forward day, ticks inside the skipped hour do not exist and are skipped;
//! - on a fall-back day, a repeated wall-clock time ticks twice (once per occurrence).

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Days, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> DateTime<Utc>;
}

/// A clock for tests: returns a settable instant.
#[derive(Debug)]
pub struct FixedClock(Mutex<DateTime<Utc>>);

impl FixedClock {
    /// A clock stopped at `at`.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(Mutex::new(at))
    }

    /// Moves the clock to `at`.
    pub fn set(&self, at: DateTime<Utc>) {
        if let Ok(mut g) = self.0.lock() {
            *g = at;
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut g) = self.0.lock()
            && let Ok(d) = chrono::Duration::from_std(by)
        {
            *g += d;
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        match self.0.lock() {
            Ok(g) => *g,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// Daily working hours, `[start, end)` in local time. `start < end` (validated in config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkHours {
    /// First minute in which new jobs may start.
    pub start: NaiveTime,
    /// First minute in which new jobs may no longer start.
    pub end: NaiveTime,
}

impl WorkHours {
    /// Whether a new job may start at `now` (G9).
    pub fn contains(&self, now: DateTime<Utc>, tz: Tz) -> bool {
        let local = now.with_timezone(&tz).time();
        self.start <= local && local < self.end
    }
}

/// Maps a local wall-clock time to the instants it denotes (0 in a gap, 2 when repeated).
fn local_instants(tz: Tz, naive: NaiveDateTime) -> Vec<DateTime<Utc>> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => vec![t.with_timezone(&Utc)],
        LocalResult::Ambiguous(a, b) => vec![a.with_timezone(&Utc), b.with_timezone(&Utc)],
        LocalResult::None => Vec::new(),
    }
}

/// The first tick strictly after `now`, aligned to local wall clock multiples of `interval`.
///
/// `interval` must be at least one minute and divide 24 hours (validated in config); other
/// values fall back to plain `now + interval`.
pub fn next_tick(now: DateTime<Utc>, tz: Tz, interval: Duration) -> DateTime<Utc> {
    let secs = interval.as_secs();
    let fallback = now + chrono::Duration::seconds(i64::try_from(secs.max(1)).unwrap_or(900));
    if secs < 60 || 86_400 % secs != 0 {
        return fallback;
    }
    let local = now.with_timezone(&tz).naive_local();
    let mut day = local.date().pred_opt().unwrap_or(local.date());
    // Collect candidates from the previous local day on and take the earliest one after
    // `now`. Local order is not instant order in a repeated hour, hence the minimum.
    let mut best: Option<DateTime<Utc>> = None;
    for _ in 0..3 {
        let mut offset = 0u64;
        while offset < 86_400 {
            if let Some(t) = u32::try_from(offset)
                .ok()
                .and_then(|o| NaiveTime::from_num_seconds_from_midnight_opt(o, 0))
            {
                for instant in local_instants(tz, day.and_time(t)) {
                    if instant > now && best.is_none_or(|b| instant < b) {
                        best = Some(instant);
                    }
                }
            }
            offset += secs;
        }
        if best.is_some() {
            break;
        }
        match day.checked_add_days(Days::new(1)) {
            Some(d) => day = d,
            None => break,
        }
    }
    best.unwrap_or(fallback)
}

/// The first instant strictly after `now` at local wall-clock time `at` (daily jobs: summary,
/// image update). If `at` falls in a DST gap on some day, that day runs at the first valid
/// instant after the gap.
pub fn next_daily(now: DateTime<Utc>, tz: Tz, at: NaiveTime) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    let mut day = today.pred_opt().unwrap_or(today);
    for _ in 0..4 {
        let naive = day.and_time(at);
        let mut instants = local_instants(tz, naive);
        if instants.is_empty() {
            // In a gap: take the first minute that exists after it.
            let mut probe = naive;
            for _ in 0..180 {
                probe += chrono::Duration::minutes(1);
                instants = local_instants(tz, probe);
                if !instants.is_empty() {
                    instants.truncate(1);
                    break;
                }
            }
        }
        // A repeated time runs once, at its first occurrence.
        if let Some(first) = instants.first()
            && *first > now
        {
            return *first;
        }
        match day.checked_add_days(Days::new(1)) {
            Some(d) => day = d,
            None => break,
        }
    }
    now + chrono::Duration::days(1)
}

/// Token-expiry reminder (SPEC 11 `token_expiry`): returns the days left when `today` is 14,
/// 7 or 1 day(s) before `created` turns one year old, else `None`.
pub fn token_expiry_days_left(created: NaiveDate, today: NaiveDate) -> Option<i64> {
    let days = days_until_token_expiry(created, today)?;
    matches!(days, 14 | 7 | 1).then_some(days)
}

/// Days until a Claude token created on `created` is one year old (negative when past).
/// `None` only for dates at the end of chrono's range.
pub fn days_until_token_expiry(created: NaiveDate, today: NaiveDate) -> Option<i64> {
    let expiry = created
        .checked_add_months(chrono::Months::new(12))
        .or_else(|| created.checked_add_days(Days::new(365)))?;
    Some((expiry - today).num_days())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Kyiv;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    const FIFTEEN: Duration = Duration::from_secs(15 * 60);

    fn hours() -> WorkHours {
        WorkHours {
            start: t(7, 0),
            end: t(23, 0),
        }
    }

    #[test]
    fn g9_inside_and_outside_work_hours() {
        // Kyiv is UTC+2 in winter.
        assert!(hours().contains(utc("2026-01-10T05:00:00Z"), Kyiv)); // 07:00 local
        assert!(!hours().contains(utc("2026-01-10T04:59:59Z"), Kyiv)); // 06:59:59
        assert!(hours().contains(utc("2026-01-10T20:59:59Z"), Kyiv)); // 22:59:59
        assert!(!hours().contains(utc("2026-01-10T03:00:00Z"), Kyiv)); // 05:00
    }

    #[test]
    fn g9_tick_exactly_at_work_hours_end_is_outside() {
        assert!(!hours().contains(utc("2026-01-10T21:00:00Z"), Kyiv)); // 23:00 local
        // Summer: UTC+3.
        assert!(!hours().contains(utc("2026-07-10T20:00:00Z"), Kyiv)); // 23:00 local
        assert!(hours().contains(utc("2026-07-10T19:45:00Z"), Kyiv)); // 22:45 local
    }

    #[test]
    fn g9_work_hours_follow_dst_change() {
        // 2026-03-29: Kyiv switches 03:00 -> 04:00 local (UTC+2 -> UTC+3).
        // 04:30 UTC is 06:30 before the switch day logic would say 07:30 after; on the
        // switch day it is 07:30 local, inside.
        assert!(hours().contains(utc("2026-03-29T04:30:00Z"), Kyiv));
        // The day before, the same UTC instant is 06:30 local, outside.
        assert!(!hours().contains(utc("2026-03-28T04:30:00Z"), Kyiv));
        // 2026-10-25: back to UTC+2. 04:30 UTC is 06:30 local, outside.
        assert!(!hours().contains(utc("2026-10-25T04:30:00Z"), Kyiv));
    }

    #[test]
    fn g9_ticks_align_to_wall_clock() {
        assert_eq!(
            next_tick(utc("2026-01-10T10:07:13Z"), Kyiv, FIFTEEN),
            utc("2026-01-10T10:15:00Z")
        );
        // Exactly on a tick: the next one, strictly after.
        assert_eq!(
            next_tick(utc("2026-01-10T10:15:00Z"), Kyiv, FIFTEEN),
            utc("2026-01-10T10:30:00Z")
        );
        // Across local midnight.
        assert_eq!(
            next_tick(utc("2026-01-10T21:59:00Z"), Kyiv, FIFTEEN),
            utc("2026-01-10T22:00:00Z")
        );
    }

    #[test]
    fn g9_ticks_align_in_time_zones_with_odd_offsets() {
        // Asia/Kathmandu is UTC+05:45: local :00 is UTC :15.
        let tz: Tz = "Asia/Kathmandu".parse().unwrap();
        // 10:20 UTC = 16:05 local; next hourly tick is 17:00 local = 11:15 UTC.
        assert_eq!(
            next_tick(utc("2026-01-10T10:20:00Z"), tz, Duration::from_secs(3600)),
            utc("2026-01-10T11:15:00Z")
        );
    }

    #[test]
    fn g9_ticks_skip_the_spring_forward_gap_in_kyiv() {
        // 2026-03-29 00:50 UTC = 02:50 local (UTC+2). Next 15-minute tick is 03:00 local,
        // which does not exist; 04:00 local (UTC+3) = 01:00 UTC is next.
        assert_eq!(
            next_tick(utc("2026-03-29T00:50:00Z"), Kyiv, FIFTEEN),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            next_tick(utc("2026-03-29T00:45:00Z"), Kyiv, FIFTEEN),
            utc("2026-03-29T01:00:00Z")
        );
    }

    #[test]
    fn g9_ticks_repeat_in_the_fall_back_hour_in_kyiv() {
        // 2026-10-25 04:00 local (UTC+3) goes back to 03:00 (UTC+2).
        // 03:45 local first occurrence = 00:45 UTC; next tick is 03:00 local again = 01:00 UTC.
        assert_eq!(
            next_tick(utc("2026-10-25T00:45:00Z"), Kyiv, FIFTEEN),
            utc("2026-10-25T01:00:00Z")
        );
        assert_eq!(
            next_tick(utc("2026-10-25T01:00:00Z"), Kyiv, FIFTEEN),
            utc("2026-10-25T01:15:00Z")
        );
        // 03:20 local, first occurrence (00:20 UTC): next is 03:30 first occurrence, not the
        // second occurrence of 03:00.
        assert_eq!(
            next_tick(utc("2026-10-25T00:20:00Z"), Kyiv, FIFTEEN),
            utc("2026-10-25T00:30:00Z")
        );
        // Every tick is strictly increasing and 15 minutes apart through the change.
        let mut at = utc("2026-10-24T22:00:00Z");
        for _ in 0..24 {
            let next = next_tick(at, Kyiv, FIFTEEN);
            assert_eq!(next - at, chrono::Duration::minutes(15));
            at = next;
        }
    }

    #[test]
    fn g9_non_dividing_interval_falls_back_to_plain_addition() {
        let now = utc("2026-01-10T10:07:00Z");
        assert_eq!(
            next_tick(now, Kyiv, Duration::from_secs(7 * 60)),
            now + chrono::Duration::minutes(7)
        );
    }

    #[test]
    fn g9_next_daily_time() {
        // 21:00 local in winter = 19:00 UTC.
        assert_eq!(
            next_daily(utc("2026-01-10T10:00:00Z"), Kyiv, t(21, 0)),
            utc("2026-01-10T19:00:00Z")
        );
        assert_eq!(
            next_daily(utc("2026-01-10T19:00:00Z"), Kyiv, t(21, 0)),
            utc("2026-01-11T19:00:00Z")
        );
    }

    #[test]
    fn g9_next_daily_in_dst_gap_and_overlap() {
        // 03:30 local does not exist on 2026-03-29: runs at 04:00 local = 01:00 UTC.
        assert_eq!(
            next_daily(utc("2026-03-28T22:00:00Z"), Kyiv, t(3, 30)),
            utc("2026-03-29T01:00:00Z")
        );
        // 03:30 local happens twice on 2026-10-25: runs once, at the first (00:30 UTC).
        assert_eq!(
            next_daily(utc("2026-10-24T22:00:00Z"), Kyiv, t(3, 30)),
            utc("2026-10-25T00:30:00Z")
        );
        assert_eq!(
            next_daily(utc("2026-10-25T00:30:00Z"), Kyiv, t(3, 30)),
            utc("2026-10-26T01:30:00Z")
        );
    }

    #[test]
    fn fixed_clock_is_settable() {
        let c = FixedClock::new(utc("2026-01-10T10:00:00Z"));
        c.advance(FIFTEEN);
        assert_eq!(c.now(), utc("2026-01-10T10:15:00Z"));
        c.set(utc("2027-01-01T00:00:00Z"));
        assert_eq!(c.now(), utc("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn token_expiry_reminders() {
        let created = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(days_until_token_expiry(created, d(2026, 11, 24)), Some(364));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 9)), Some(14));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 16)), Some(7));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 22)), Some(1));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 21)), None);
        assert_eq!(days_until_token_expiry(created, d(2027, 11, 25)), Some(-2));
    }
}

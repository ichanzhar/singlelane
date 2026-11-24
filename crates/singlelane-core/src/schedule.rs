//! Clock, working hours and tick alignment (SPEC 8, G9).
//!
//! All functions take the current time as input; [`Clock`] is the injection point the
//! daemon implements with the system clock and tests implement with [`FixedClock`].
//! Time zones come from the tz database bundled into `jiff`, so nothing here reads the
//! filesystem; tz data updates arrive as `jiff-tzdb` releases.
//!
//! Wall-clock rules in the configured time zone:
//! - work hours are the half-open interval `[start, end)`: a tick exactly at `end` is outside;
//! - ticks fall on local wall-clock times that are multiples of `poll_interval` after local
//!   midnight (`:00 :15 :30 :45` for 15 minutes);
//! - on a spring-forward day, ticks inside the skipped hour do not exist and are skipped;
//! - on a fall-back day, a repeated wall-clock time ticks twice (once per occurrence).

use std::sync::Mutex;
use std::time::Duration;

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp, ToSpan};

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// A clock for tests: returns a settable instant.
#[derive(Debug)]
pub struct FixedClock(Mutex<Timestamp>);

impl FixedClock {
    /// A clock stopped at `at`.
    pub fn new(at: Timestamp) -> Self {
        Self(Mutex::new(at))
    }

    /// Moves the clock to `at`.
    pub fn set(&self, at: Timestamp) {
        if let Ok(mut g) = self.0.lock() {
            *g = at;
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut g) = self.0.lock()
            && let Ok(d) = SignedDuration::try_from(by)
            && let Ok(t) = g.checked_add(d)
        {
            *g = t;
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
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
    pub start: Time,
    /// First minute in which new jobs may no longer start.
    pub end: Time,
}

impl WorkHours {
    /// Whether a new job may start at `now` (G9).
    pub fn contains(&self, now: Timestamp, tz: &TimeZone) -> bool {
        let local = tz.to_datetime(now).time();
        self.start <= local && local < self.end
    }
}

/// Maps a local wall-clock time to the instants it denotes (0 in a gap, 2 when repeated,
/// earliest first).
fn local_instants(tz: &TimeZone, dt: DateTime) -> Vec<Timestamp> {
    let offsets = match tz.to_ambiguous_timestamp(dt).offset() {
        AmbiguousOffset::Unambiguous { offset } => vec![offset],
        AmbiguousOffset::Gap { .. } => Vec::new(),
        AmbiguousOffset::Fold { before, after } => vec![before, after],
    };
    let mut out: Vec<Timestamp> = offsets
        .into_iter()
        .filter_map(|o| o.to_timestamp(dt).ok())
        .collect();
    out.sort();
    out
}

fn plus(now: Timestamp, d: SignedDuration) -> Timestamp {
    now.checked_add(d).unwrap_or(Timestamp::MAX)
}

/// The first tick strictly after `now`, aligned to local wall clock multiples of `interval`.
///
/// `interval` must be whole minutes, at least one minute, and divide 24 hours (validated in
/// config); other values fall back to plain `now + interval`.
pub fn next_tick(now: Timestamp, tz: &TimeZone, interval: Duration) -> Timestamp {
    let secs = interval.as_secs();
    let fallback = plus(
        now,
        SignedDuration::from_secs(i64::try_from(secs.max(1)).unwrap_or(900)),
    );
    if secs < 60 || !secs.is_multiple_of(60) || !86_400u64.is_multiple_of(secs) {
        return fallback;
    }
    let today = tz.to_datetime(now).date();
    let mut day = today.yesterday().unwrap_or(today);
    // Collect candidates from the previous local day on and take the earliest one after
    // `now`. Local order is not instant order in a repeated hour, hence the minimum.
    let mut best: Option<Timestamp> = None;
    for _ in 0..3 {
        let mut offset = 0u64;
        while offset < 86_400 {
            if let Some(t) = time_of_day(offset) {
                for instant in local_instants(tz, day.to_datetime(t)) {
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
        match day.tomorrow() {
            Ok(d) => day = d,
            Err(_) => break,
        }
    }
    best.unwrap_or(fallback)
}

fn time_of_day(secs: u64) -> Option<Time> {
    let h = i8::try_from(secs / 3600).ok()?;
    let m = i8::try_from(secs % 3600 / 60).ok()?;
    let s = i8::try_from(secs % 60).ok()?;
    Time::new(h, m, s, 0).ok()
}

/// The first instant strictly after `now` at local wall-clock time `at` (daily jobs: summary,
/// image update). If `at` falls in a DST gap on some day, that day runs at the first valid
/// instant after the gap. A repeated time runs once, at its first occurrence.
pub fn next_daily(now: Timestamp, tz: &TimeZone, at: Time) -> Timestamp {
    let today = tz.to_datetime(now).date();
    let mut day = today.yesterday().unwrap_or(today);
    for _ in 0..4 {
        let dt = day.to_datetime(at);
        let mut instants = local_instants(tz, dt);
        if instants.is_empty() {
            // In a gap: take the first minute that exists after it.
            let mut probe = dt;
            for _ in 0..180 {
                let Ok(next) = probe.checked_add(SignedDuration::from_mins(1)) else {
                    break;
                };
                probe = next;
                instants = local_instants(tz, probe);
                if !instants.is_empty() {
                    break;
                }
            }
        }
        if let Some(first) = instants.first()
            && *first > now
        {
            return *first;
        }
        match day.tomorrow() {
            Ok(d) => day = d,
            Err(_) => break,
        }
    }
    plus(now, SignedDuration::from_hours(24))
}

/// Token-expiry reminder (SPEC 11 `token_expiry`): returns the days left when `today` is 14,
/// 7 or 1 day(s) before `created` turns one year old, else `None`.
pub fn token_expiry_days_left(created: Date, today: Date) -> Option<i64> {
    let days = days_until_token_expiry(created, today)?;
    matches!(days, 14 | 7 | 1).then_some(days)
}

/// Days until a Claude token created on `created` is one year old (negative when past).
/// `None` only for dates at the end of the supported range.
pub fn days_until_token_expiry(created: Date, today: Date) -> Option<i64> {
    let expiry = created.checked_add(1.year()).ok()?;
    Some(i64::from(expiry.since(today).ok()?.get_days()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn t(h: i8, m: i8) -> Time {
        Time::new(h, m, 0, 0).unwrap()
    }

    fn kyiv() -> TimeZone {
        TimeZone::get("Europe/Kyiv").unwrap()
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
        assert!(hours().contains(utc("2026-01-10T05:00:00Z"), &kyiv())); // 07:00 local
        assert!(!hours().contains(utc("2026-01-10T04:59:59Z"), &kyiv())); // 06:59:59
        assert!(hours().contains(utc("2026-01-10T20:59:59Z"), &kyiv())); // 22:59:59
        assert!(!hours().contains(utc("2026-01-10T03:00:00Z"), &kyiv())); // 05:00
    }

    #[test]
    fn g9_tick_exactly_at_work_hours_end_is_outside() {
        assert!(!hours().contains(utc("2026-01-10T21:00:00Z"), &kyiv())); // 23:00 local
        // Summer: UTC+3.
        assert!(!hours().contains(utc("2026-07-10T20:00:00Z"), &kyiv())); // 23:00 local
        assert!(hours().contains(utc("2026-07-10T19:45:00Z"), &kyiv())); // 22:45 local
    }

    #[test]
    fn g9_work_hours_follow_dst_change() {
        // 2026-03-29: Kyiv switches 03:00 -> 04:00 local (UTC+2 -> UTC+3).
        // 04:30 UTC is 06:30 before the switch day logic would say 07:30 after; on the
        // switch day it is 07:30 local, inside.
        assert!(hours().contains(utc("2026-03-29T04:30:00Z"), &kyiv()));
        // The day before, the same UTC instant is 06:30 local, outside.
        assert!(!hours().contains(utc("2026-03-28T04:30:00Z"), &kyiv()));
        // 2026-10-25: back to UTC+2. 04:30 UTC is 06:30 local, outside.
        assert!(!hours().contains(utc("2026-10-25T04:30:00Z"), &kyiv()));
    }

    #[test]
    fn g9_ticks_align_to_wall_clock() {
        assert_eq!(
            next_tick(utc("2026-01-10T10:07:13Z"), &kyiv(), FIFTEEN),
            utc("2026-01-10T10:15:00Z")
        );
        // Exactly on a tick: the next one, strictly after.
        assert_eq!(
            next_tick(utc("2026-01-10T10:15:00Z"), &kyiv(), FIFTEEN),
            utc("2026-01-10T10:30:00Z")
        );
        // Across local midnight.
        assert_eq!(
            next_tick(utc("2026-01-10T21:59:00Z"), &kyiv(), FIFTEEN),
            utc("2026-01-10T22:00:00Z")
        );
    }

    #[test]
    fn g9_ticks_align_in_time_zones_with_odd_offsets() {
        // Asia/Kathmandu is UTC+05:45: local :00 is UTC :15.
        let tz = TimeZone::get("Asia/Kathmandu").unwrap();
        // 10:20 UTC = 16:05 local; next hourly tick is 17:00 local = 11:15 UTC.
        assert_eq!(
            next_tick(utc("2026-01-10T10:20:00Z"), &tz, Duration::from_secs(3600)),
            utc("2026-01-10T11:15:00Z")
        );
    }

    #[test]
    fn g9_ticks_skip_the_spring_forward_gap_in_kyiv() {
        // 2026-03-29 00:50 UTC = 02:50 local (UTC+2). Next 15-minute tick is 03:00 local,
        // which does not exist; 04:00 local (UTC+3) = 01:00 UTC is next.
        assert_eq!(
            next_tick(utc("2026-03-29T00:50:00Z"), &kyiv(), FIFTEEN),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            next_tick(utc("2026-03-29T00:45:00Z"), &kyiv(), FIFTEEN),
            utc("2026-03-29T01:00:00Z")
        );
    }

    #[test]
    fn g9_ticks_repeat_in_the_fall_back_hour_in_kyiv() {
        // 2026-10-25 04:00 local (UTC+3) goes back to 03:00 (UTC+2).
        // 03:45 local first occurrence = 00:45 UTC; next tick is 03:00 local again = 01:00 UTC.
        assert_eq!(
            next_tick(utc("2026-10-25T00:45:00Z"), &kyiv(), FIFTEEN),
            utc("2026-10-25T01:00:00Z")
        );
        assert_eq!(
            next_tick(utc("2026-10-25T01:00:00Z"), &kyiv(), FIFTEEN),
            utc("2026-10-25T01:15:00Z")
        );
        // 03:20 local, first occurrence (00:20 UTC): next is 03:30 first occurrence, not the
        // second occurrence of 03:00.
        assert_eq!(
            next_tick(utc("2026-10-25T00:20:00Z"), &kyiv(), FIFTEEN),
            utc("2026-10-25T00:30:00Z")
        );
        // Every tick is strictly increasing and 15 minutes apart through the change.
        let mut at = utc("2026-10-24T22:00:00Z");
        for _ in 0..24 {
            let next = next_tick(at, &kyiv(), FIFTEEN);
            assert_eq!(next.duration_since(at), SignedDuration::from_mins(15));
            at = next;
        }
    }

    #[test]
    fn g9_non_dividing_interval_falls_back_to_plain_addition() {
        let now = utc("2026-01-10T10:07:00Z");
        assert_eq!(
            next_tick(now, &kyiv(), Duration::from_secs(7 * 60)),
            now + SignedDuration::from_mins(7)
        );
    }

    #[test]
    fn g9_next_daily_time() {
        // 21:00 local in winter = 19:00 UTC.
        assert_eq!(
            next_daily(utc("2026-01-10T10:00:00Z"), &kyiv(), t(21, 0)),
            utc("2026-01-10T19:00:00Z")
        );
        assert_eq!(
            next_daily(utc("2026-01-10T19:00:00Z"), &kyiv(), t(21, 0)),
            utc("2026-01-11T19:00:00Z")
        );
    }

    #[test]
    fn g9_next_daily_in_dst_gap_and_overlap() {
        // 03:30 local does not exist on 2026-03-29: runs at 04:00 local = 01:00 UTC.
        assert_eq!(
            next_daily(utc("2026-03-28T22:00:00Z"), &kyiv(), t(3, 30)),
            utc("2026-03-29T01:00:00Z")
        );
        // 03:30 local happens twice on 2026-10-25: runs once, at the first (00:30 UTC).
        assert_eq!(
            next_daily(utc("2026-10-24T22:00:00Z"), &kyiv(), t(3, 30)),
            utc("2026-10-25T00:30:00Z")
        );
        assert_eq!(
            next_daily(utc("2026-10-25T00:30:00Z"), &kyiv(), t(3, 30)),
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
        let created = Date::new(2026, 11, 23).unwrap();
        let d = |y, m, dd| Date::new(y, m, dd).unwrap();
        assert_eq!(days_until_token_expiry(created, d(2026, 11, 24)), Some(364));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 9)), Some(14));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 16)), Some(7));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 22)), Some(1));
        assert_eq!(token_expiry_days_left(created, d(2027, 11, 21)), None);
        assert_eq!(days_until_token_expiry(created, d(2027, 11, 25)), Some(-2));
    }
}

//! Calendar dates from Unix time, EU summer time, and a wall clock kept
//! from one time sync and a monotonic timer.

use core::fmt;

use embassy_time::Instant;

/// Calendar date and time (UTC, or local time from [`LocalTime`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    /// Year, e.g. 2026.
    pub year: i32,
    /// Month, 1..=12.
    pub month: u8,
    /// Day of the month, 1..=31.
    pub day: u8,
    /// Hour, 0..=23.
    pub hour: u8,
    /// Minute, 0..=59.
    pub minute: u8,
    /// Second, 0..=59.
    pub second: u8,
}

impl DateTime {
    /// Converts Unix seconds to a date (Howard Hinnant's `civil_from_days`).
    #[must_use]
    pub fn from_unix(unix: u64) -> Self {
        let days = (unix / 86_400) as i64;
        let secs = unix % 86_400;

        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;

        Self {
            year,
            month,
            day,
            hour: (secs / 3600) as u8,
            minute: (secs / 60 % 60) as u8,
            second: (secs % 60) as u8,
        }
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Days since 1970-01-01 of a calendar date (Howard Hinnant's `days_from_civil`).
#[must_use]
pub fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Day of the week for days since 1970-01-01: 0 = Monday … 6 = Sunday.
#[must_use]
pub fn weekday(days: i64) -> u8 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7) as u8
}

/// Days since 1970-01-01 of the last day of a month.
fn last_day_of_month(year: i32, month: u8) -> i64 {
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    days_from_civil(y, m, 1) - 1
}

/// Unix time of 01:00 UTC on the last Sunday of a month.
fn last_sunday_0100_utc(year: i32, month: u8) -> u64 {
    let last = last_day_of_month(year, month);
    let sunday = last - i64::from((weekday(last) + 1) % 7);
    (sunday * 86_400 + 3600) as u64
}

/// Whether EU summer time is in effect at Unix time `unix`: from 01:00 UTC
/// on the last Sunday of March to 01:00 UTC on the last Sunday of October.
#[must_use]
pub fn eu_summer_time(unix: u64) -> bool {
    let year = DateTime::from_unix(unix).year;
    (last_sunday_0100_utc(year, 3)..last_sunday_0100_utc(year, 10)).contains(&unix)
}

/// Local time with its offset from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Local date and time.
    pub date: DateTime,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    /// Offset from UTC in minutes, summer time included.
    pub offset_min: i64,
    /// Whether summer time applies.
    pub summer: bool,
}

impl LocalTime {
    /// Converts Unix time to local time: `standard_offset_min` from UTC, plus
    /// one hour during EU summer time if `eu_dst`.
    #[must_use]
    pub fn from_unix(unix: u64, standard_offset_min: i64, eu_dst: bool) -> Self {
        let summer = eu_dst && eu_summer_time(unix);
        let offset_min = standard_offset_min + if summer { 60 } else { 0 };
        let local = (unix as i64 + offset_min * 60) as u64;
        Self {
            date: DateTime::from_unix(local),
            weekday: weekday((local / 86_400) as i64),
            offset_min,
            summer,
        }
    }

    /// Minutes since local midnight.
    #[must_use]
    pub fn minute_of_day(&self) -> u16 {
        u16::from(self.date.hour) * 60 + u16::from(self.date.minute)
    }

    /// English short name of the weekday.
    #[must_use]
    pub fn weekday_name(&self) -> &'static str {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][usize::from(self.weekday)]
    }
}

/// Unix time kept from one time sync (e.g. SNTP) and a monotonic timer.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    unix_at_sync: u64,
    synced_at: Instant,
}

impl Clock {
    /// A clock that read `unix` at `now`.
    #[must_use]
    pub fn new(unix: u64, now: Instant) -> Self {
        Self {
            unix_at_sync: unix,
            synced_at: now,
        }
    }

    /// Unix time in seconds at `now` (not before the sync).
    #[must_use]
    pub fn unix_at(&self, now: Instant) -> u64 {
        self.unix_at_sync
            + now
                .checked_duration_since(self.synced_at)
                .map_or(0, |d| d.as_secs())
    }

    /// When the clock was set.
    #[must_use]
    pub fn synced_at(&self) -> Instant {
        self.synced_at
    }

    /// The next instant at which the Unix time ticks to a new second, after `now`.
    #[must_use]
    pub fn next_tick(&self, now: Instant) -> Instant {
        let elapsed = now
            .checked_duration_since(self.synced_at)
            .map_or(0, |d| d.as_secs());
        self.synced_at + embassy_time::Duration::from_secs(elapsed + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embassy_time::Duration;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(DateTime::from_unix(0).year, 1970);
        // 2026-11-08 12:34:56 UTC
        let d = DateTime::from_unix(1_794_141_296);
        assert_eq!(
            (d.year, d.month, d.day, d.hour, d.minute, d.second),
            (2026, 11, 8, 12, 34, 56)
        );
        // Leap day.
        let d = DateTime::from_unix(days_from_civil(2024, 2, 29) as u64 * 86_400);
        assert_eq!((d.year, d.month, d.day), (2024, 2, 29));
    }

    #[test]
    fn civil_round_trip() {
        for days in (0..60_000i64).step_by(37) {
            let d = DateTime::from_unix(days as u64 * 86_400);
            assert_eq!(days_from_civil(d.year, d.month, d.day), days);
        }
    }

    #[test]
    fn weekdays() {
        assert_eq!(weekday(0), 3); // 1970-01-01, Thursday
        assert_eq!(weekday(days_from_civil(2026, 11, 8)), 6); // Sunday
    }

    #[test]
    fn eu_summer_time_2026() {
        // 2026: from 29 March 01:00 UTC to 25 October 01:00 UTC.
        let start = days_from_civil(2026, 3, 29) as u64 * 86_400 + 3600;
        let end = days_from_civil(2026, 10, 25) as u64 * 86_400 + 3600;
        assert!(!eu_summer_time(start - 1));
        assert!(eu_summer_time(start));
        assert!(eu_summer_time(end - 1));
        assert!(!eu_summer_time(end));
    }

    #[test]
    fn last_day_handles_short_months() {
        assert_eq!(last_day_of_month(2026, 2), days_from_civil(2026, 2, 28));
        assert_eq!(last_day_of_month(2024, 2), days_from_civil(2024, 2, 29));
        assert_eq!(last_day_of_month(2026, 4), days_from_civil(2026, 4, 30));
        assert_eq!(last_day_of_month(2026, 12), days_from_civil(2026, 12, 31));
    }

    #[test]
    fn prague_local_time() {
        let summer = days_from_civil(2026, 7, 1) as u64 * 86_400 + 10 * 3600;
        let t = LocalTime::from_unix(summer, 60, true);
        assert_eq!((t.date.hour, t.offset_min, t.summer), (12, 120, true));
        let winter = days_from_civil(2026, 12, 1) as u64 * 86_400 + 10 * 3600;
        let t = LocalTime::from_unix(winter, 60, true);
        assert_eq!((t.date.hour, t.offset_min, t.summer), (11, 60, false));
        assert_eq!(t.minute_of_day(), 11 * 60);
    }

    #[test]
    fn clock_counts_from_sync() {
        let t0 = Instant::from_secs(100);
        let c = Clock::new(1_000_000, t0);
        assert_eq!(c.unix_at(t0), 1_000_000);
        assert_eq!(c.unix_at(t0 + Duration::from_millis(2500)), 1_000_002);
        assert_eq!(c.unix_at(Instant::from_secs(50)), 1_000_000);
        assert_eq!(
            c.next_tick(t0 + Duration::from_millis(2500)),
            t0 + Duration::from_secs(3)
        );
    }
}

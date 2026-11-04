//! Wall-clock time from SNTP, kept by the monotonic timer, and local time.
//!
//! The time zone is set at build time: `TZ_OFFSET_MIN` is the offset of local
//! standard time from UTC in minutes (default 60, Central European Time), and
//! `TZ_EU_DST` (default `1`) enables the EU summer-time rule: one hour more
//! from 01:00 UTC on the last Sunday of March to 01:00 UTC on the last Sunday
//! of October.

use embassy_time::Instant;

use crate::net::DateTime;

/// Local standard time offset from UTC, in minutes.
pub fn tz_offset_min() -> i64 {
    option_env!("TZ_OFFSET_MIN")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(60)
}

/// Whether the EU summer-time rule applies.
pub fn tz_eu_dst() -> bool {
    option_env!("TZ_EU_DST").is_none_or(|s| s.trim() != "0")
}

/// Days since 1970-01-01 of a calendar date (Howard Hinnant's `days_from_civil`).
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
pub fn weekday(days: i64) -> u8 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7) as u8
}

/// Unix time of 01:00 UTC on the last Sunday of `month` (which has 31 days).
fn last_sunday_0100_utc(year: i32, month: u8) -> u64 {
    let last = days_from_civil(year, month, 31);
    let sunday = last - i64::from((weekday(last) + 1) % 7);
    (sunday * 86_400 + 3600) as u64
}

/// Whether EU summer time is in effect at Unix time `unix`.
pub fn eu_summer_time(unix: u64) -> bool {
    let year = DateTime::from_unix(unix).year;
    (last_sunday_0100_utc(year, 3)..last_sunday_0100_utc(year, 10)).contains(&unix)
}

/// Local time with its offset from UTC.
#[derive(Debug, Clone, Copy)]
pub struct LocalTime {
    pub date: DateTime,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    pub offset_min: i64,
    pub summer: bool,
}

impl LocalTime {
    /// Converts Unix time to local time with the build-time zone settings.
    pub fn from_unix(unix: u64) -> Self {
        let summer = tz_eu_dst() && eu_summer_time(unix);
        let offset_min = tz_offset_min() + if summer { 60 } else { 0 };
        let local = (unix as i64 + offset_min * 60) as u64;
        Self {
            date: DateTime::from_unix(local),
            weekday: weekday((local / 86_400) as i64),
            offset_min,
            summer,
        }
    }

    /// Minutes since local midnight.
    pub fn minute_of_day(&self) -> u16 {
        u16::from(self.date.hour) * 60 + u16::from(self.date.minute)
    }

    /// English short name of the weekday.
    pub fn weekday_name(&self) -> &'static str {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][usize::from(self.weekday)]
    }
}

/// Unix time maintained from an SNTP reference and the monotonic timer.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    unix_at_sync: u64,
    synced_at: Instant,
}

impl Clock {
    /// Starts the clock from an SNTP result taken just now.
    pub fn new(unix_now: u64) -> Self {
        Self {
            unix_at_sync: unix_now,
            synced_at: Instant::now(),
        }
    }

    /// Current Unix time in seconds.
    pub fn unix(&self) -> u64 {
        self.unix_at_sync + self.synced_at.elapsed().as_secs()
    }

    /// Current local time.
    pub fn local(&self) -> LocalTime {
        LocalTime::from_unix(self.unix())
    }

    /// When the clock was last set.
    pub fn synced_at(&self) -> Instant {
        self.synced_at
    }
}

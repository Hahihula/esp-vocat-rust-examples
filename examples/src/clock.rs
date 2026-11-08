//! Wall-clock time from SNTP and local time, on top of
//! [`crate::time`] (where the date arithmetic is tested).
//!
//! The time zone is set at build time: `TZ_OFFSET_MIN` is the offset of local
//! standard time from UTC in minutes (default 60, Central European Time), and
//! `TZ_EU_DST` (default `1`) enables the EU summer-time rule: one hour more
//! from 01:00 UTC on the last Sunday of March to 01:00 UTC on the last Sunday
//! of October.

pub use crate::time::{Clock, DateTime, LocalTime, days_from_civil, eu_summer_time, weekday};
use embassy_time::Instant;

/// Local standard time offset from UTC, in minutes.
#[must_use]
pub fn tz_offset_min() -> i64 {
    option_env!("TZ_OFFSET_MIN")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(60)
}

/// Whether the EU summer-time rule applies.
#[must_use]
pub fn tz_eu_dst() -> bool {
    option_env!("TZ_EU_DST").is_none_or(|s| s.trim() != "0")
}

/// Local time of a Unix time with the build-time zone settings.
#[must_use]
pub fn local_time(unix: u64) -> LocalTime {
    LocalTime::from_unix(unix, tz_offset_min(), tz_eu_dst())
}

/// [`Clock`] read at the current instant.
pub trait ClockNow {
    /// Current Unix time in seconds.
    fn unix(&self) -> u64;
    /// Current local time.
    fn local(&self) -> LocalTime;
}

impl ClockNow for Clock {
    fn unix(&self) -> u64 {
        self.unix_at(Instant::now())
    }

    fn local(&self) -> LocalTime {
        local_time(self.unix())
    }
}

/// A clock set from a time sync that read `unix` just now.
#[must_use]
pub fn synced(unix: u64) -> Clock {
    Clock::new(unix, Instant::now())
}

//! Software motion detection on top of accelerometer and gyroscope samples.
//!
//! [`MotionDetector`] is fed samples at a fixed rate and reports tilt against a
//! reference "upright" direction, moving/still transitions, shakes and knocks.
//! All thresholds are in g, °/s and degrees; they were picked for a hand-held
//! toy and are easy to tune.

use embassy_time::{Duration, Instant};
use libm::{acosf, sqrtf};

/// Tilt from upright above which the device counts as tilted.
pub const TILT_ON_DEG: f32 = 35.0;
/// Tilt below which it counts as upright again.
pub const TILT_OFF_DEG: f32 = 20.0;
/// Tilt above which it counts as upside down.
pub const UPSIDE_DOWN_DEG: f32 = 140.0;
/// Rotation rate above which the device is moving.
pub const MOVING_DPS: f32 = 25.0;
/// Rotation rate below which it may be still.
pub const STILL_DPS: f32 = 8.0;
/// How long it has to stay below [`STILL_DPS`] to be still.
pub const STILL_TIME: Duration = Duration::from_millis(1000);
/// A shake is this many acceleration peaks within [`SHAKE_WINDOW`].
pub const SHAKE_PEAKS: u8 = 3;
/// |a| above which a peak is counted. Shaking the cat by hand gives 2-4.5 g.
pub const SHAKE_HIGH_G: f32 = 1.8;
/// |a| must fall below this between two peaks.
pub const SHAKE_LOW_G: f32 = 1.3;
pub const SHAKE_WINDOW: Duration = Duration::from_millis(800);
/// Change of acceleration between two samples that counts as a knock.
/// Knocks on the housing measured 0.4-1.3 g, handling 0.1-0.3 g.
pub const KNOCK_JERK_G: f32 = 0.35;
/// A knock must not come with more rotation than this (that's handling).
pub const KNOCK_MAX_DPS: f32 = 60.0;
/// Minimum time between two knocks.
pub const KNOCK_REFRACTORY: Duration = Duration::from_millis(250);

/// What a sample changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionEvent {
    /// Started rotating.
    Moving,
    /// Stopped rotating for [`STILL_TIME`].
    Still,
    /// Tilted more than [`TILT_ON_DEG`] from upright.
    Tilted(f32),
    /// Back within [`TILT_OFF_DEG`] of upright.
    Upright,
    /// Turned more than [`UPSIDE_DOWN_DEG`] from upright.
    UpsideDown,
    /// Shaken.
    Shake,
    /// A short knock on the housing, with the jerk in g.
    Knock(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posture {
    Upright,
    Tilted,
    UpsideDown,
}

/// Motion state machine.
pub struct MotionDetector {
    up: [f32; 3],
    last_accel: Option<[f32; 3]>,
    posture: Posture,
    moving: bool,
    quiet_since: Instant,
    peaks: u8,
    /// |a| fell below [`SHAKE_LOW_G`] since the last peak.
    peak_armed: bool,
    first_peak: Instant,
    last_knock: Instant,
    /// Largest jerk since the last [`take_peak_jerk`](Self::take_peak_jerk).
    peak_jerk: f32,
    tilt_deg: f32,
}

impl MotionDetector {
    /// Creates a detector with `up` (an accelerometer reading at rest) as the
    /// upright reference.
    pub fn new(up: [f32; 3]) -> Self {
        let now = Instant::now();
        Self {
            up: normalize(up),
            last_accel: None,
            posture: Posture::Upright,
            moving: false,
            quiet_since: now,
            peaks: 0,
            peak_armed: true,
            first_peak: now,
            last_knock: now,
            peak_jerk: 0.0,
            tilt_deg: 0.0,
        }
    }

    /// Makes `up` the new upright reference.
    pub fn set_upright(&mut self, up: [f32; 3]) {
        self.up = normalize(up);
        self.posture = Posture::Upright;
    }

    /// Angle between the latest gravity reading and upright, in degrees.
    pub fn tilt_deg(&self) -> f32 {
        self.tilt_deg
    }

    /// Returns and resets the largest jerk seen, for tuning [`KNOCK_JERK_G`].
    pub fn take_peak_jerk(&mut self) -> f32 {
        core::mem::take(&mut self.peak_jerk)
    }

    /// Feeds one sample (acceleration in g, rotation in °/s). Returns at most
    /// one event; the rest are reported on later samples.
    pub fn update(&mut self, accel: [f32; 3], gyro: [f32; 3]) -> Option<MotionEvent> {
        let now = Instant::now();
        let a_mag = norm(accel);
        let g_mag = norm(gyro);
        let jerk = self.last_accel.map_or(0.0, |last| norm(sub(accel, last)));
        self.last_accel = Some(accel);
        self.peak_jerk = self.peak_jerk.max(jerk);

        // Shake: several distinct peaks of |a| in a short time.
        if self.peaks > 0 && now - self.first_peak > SHAKE_WINDOW {
            self.peaks = 0;
        }
        if a_mag < SHAKE_LOW_G {
            self.peak_armed = true;
        } else if a_mag > SHAKE_HIGH_G && self.peak_armed {
            self.peak_armed = false;
            if self.peaks == 0 {
                self.first_peak = now;
            }
            self.peaks += 1;
            if self.peaks >= SHAKE_PEAKS {
                self.peaks = 0;
                return Some(MotionEvent::Shake);
            }
        }

        // Knock: a sharp change of acceleration without much rotation, and
        // not in the middle of a shake.
        if jerk > KNOCK_JERK_G
            && g_mag < KNOCK_MAX_DPS
            && self.peaks == 0
            && now - self.last_knock > KNOCK_REFRACTORY
        {
            self.last_knock = now;
            return Some(MotionEvent::Knock(jerk));
        }

        // Moving / still from the rotation rate.
        if g_mag > MOVING_DPS {
            self.quiet_since = now;
            if !self.moving {
                self.moving = true;
                return Some(MotionEvent::Moving);
            }
        } else if g_mag > STILL_DPS {
            self.quiet_since = now;
        } else if self.moving && now - self.quiet_since >= STILL_TIME {
            self.moving = false;
            return Some(MotionEvent::Still);
        }

        // Tilt, only from samples close to 1 g so that motion doesn't count.
        if (a_mag - 1.0).abs() < 0.15 {
            let cos = dot(normalize(accel), self.up).clamp(-1.0, 1.0);
            self.tilt_deg = acosf(cos).to_degrees();
            let posture = match self.posture {
                _ if self.tilt_deg > UPSIDE_DOWN_DEG => Posture::UpsideDown,
                Posture::Upright if self.tilt_deg > TILT_ON_DEG => Posture::Tilted,
                Posture::Tilted | Posture::UpsideDown if self.tilt_deg < TILT_OFF_DEG => {
                    Posture::Upright
                }
                Posture::UpsideDown if self.tilt_deg < UPSIDE_DOWN_DEG - 20.0 => Posture::Tilted,
                p => p,
            };
            if posture != self.posture {
                self.posture = posture;
                return Some(match posture {
                    Posture::Upright => MotionEvent::Upright,
                    Posture::Tilted => MotionEvent::Tilted(self.tilt_deg),
                    Posture::UpsideDown => MotionEvent::UpsideDown,
                });
            }
        }

        None
    }
}

/// Length of a vector.
pub fn norm(v: [f32; 3]) -> f32 {
    sqrtf(dot(v, v))
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let n = norm(v);
    if n == 0.0 {
        return [0.0, 0.0, 1.0];
    }
    v.map(|c| c / n)
}

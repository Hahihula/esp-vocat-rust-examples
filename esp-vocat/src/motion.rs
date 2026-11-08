//! Software motion detection on top of accelerometer and gyroscope samples.
//!
//! [`MotionDetector`] is fed samples at a fixed rate and reports tilt against a
//! reference "upright" direction, moving/still transitions, shakes and knocks.
//! All thresholds are in g, °/s and degrees; they were picked for a hand-held
//! toy and are easy to tune. Time comes in with each sample, so the state
//! machine is deterministic and testable.

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
/// Peaks must fall within this window to count as one shake.
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
    /// upright reference, at time `now`.
    #[must_use]
    pub fn new(up: [f32; 3], now: Instant) -> Self {
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

    /// Makes `up` the new upright reference. Also forgets the previous
    /// sample, so a pause in the samples (e.g. while averaging `up`) can't
    /// look like a knock.
    pub fn set_upright(&mut self, up: [f32; 3]) {
        self.up = normalize(up);
        self.posture = Posture::Upright;
        self.last_accel = None;
    }

    /// Angle between the latest gravity reading and upright, in degrees.
    #[must_use]
    pub fn tilt_deg(&self) -> f32 {
        self.tilt_deg
    }

    /// Returns and resets the largest jerk seen, for tuning [`KNOCK_JERK_G`].
    pub fn take_peak_jerk(&mut self) -> f32 {
        core::mem::take(&mut self.peak_jerk)
    }

    /// Feeds one sample taken at `now` (acceleration in g, rotation in °/s).
    /// Returns at most one event; the rest are reported on later samples.
    pub fn update(&mut self, now: Instant, accel: [f32; 3], gyro: [f32; 3]) -> Option<MotionEvent> {
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
            // Order matters: upside down wins from any posture, then the
            // hysteresis transitions, then no change.
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
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::*;

    const UP: [f32; 3] = [0.0, 0.0, 1.0];
    const REST: [f32; 3] = [0.0; 3];

    fn at(ms: u64) -> Instant {
        Instant::from_millis(1000 + ms)
    }

    #[test]
    fn resting_gives_no_events() {
        let mut m = MotionDetector::new(UP, at(0));
        for i in 0..200 {
            assert_eq!(m.update(at(i * 5), UP, REST), None);
        }
    }

    #[test]
    fn tilt_with_hysteresis() {
        let mut m = MotionDetector::new(UP, at(0));
        let tilt = |deg: f32| {
            let r = deg.to_radians();
            [libm::sinf(r), 0.0, libm::cosf(r)]
        };
        assert_eq!(m.update(at(0), tilt(30.0), REST), None);
        assert!(matches!(
            m.update(at(5), tilt(40.0), REST),
            Some(MotionEvent::Tilted(_))
        ));
        assert_eq!(m.update(at(10), tilt(25.0), REST), None);
        assert_eq!(
            m.update(at(15), tilt(10.0), REST),
            Some(MotionEvent::Upright)
        );
        // Turned over slowly (10 degrees per sample: not a knock).
        let mut last = None;
        for (i, deg) in (2..=17).map(|k| k as f32 * 10.0).enumerate() {
            if let Some(e) = m.update(at(20 + i as u64 * 5), tilt(deg), REST) {
                last = Some(e);
            }
        }
        assert_eq!(last, Some(MotionEvent::UpsideDown));
    }

    #[test]
    fn knock_needs_jerk_without_rotation() {
        // Knocks are ignored for KNOCK_REFRACTORY after start and after each knock.
        let mut m = MotionDetector::new(UP, at(0));
        m.update(at(300), UP, REST);
        let bump = [0.0, 0.5, 1.0];
        assert!(matches!(
            m.update(at(305), bump, REST),
            Some(MotionEvent::Knock(_))
        ));
        assert_eq!(m.update(at(310), UP, REST), None);
        // Same jerk while rotating fast is handling, not a knock.
        let mut m = MotionDetector::new(UP, at(0));
        m.update(at(300), UP, [0.0, 0.0, 100.0]);
        assert_ne!(
            m.update(at(305), bump, [0.0, 0.0, 100.0])
                .map(|e| matches!(e, MotionEvent::Knock(_))),
            Some(true)
        );
    }

    #[test]
    fn set_upright_forgets_the_last_sample() {
        // A long pause, then a sample far from the last one: not a knock.
        let mut m = MotionDetector::new(UP, at(0));
        m.update(at(300), [0.0, 0.6, 0.8], REST);
        m.set_upright(UP);
        assert_eq!(m.update(at(460), UP, REST), None);
    }

    #[test]
    fn shake_is_three_peaks() {
        let mut m = MotionDetector::new(UP, at(0));
        let hi = [0.0, 0.0, 2.5];
        let lo = [0.0, 0.0, 1.0];
        let mut events = 0;
        for (i, a) in [hi, lo, hi, lo, hi].iter().enumerate() {
            if m.update(at(i as u64 * 50), *a, [0.0, 0.0, 100.0]) == Some(MotionEvent::Shake) {
                events += 1;
            }
        }
        assert_eq!(events, 1);
    }

    #[test]
    fn moving_then_still() {
        let mut m = MotionDetector::new(UP, at(0));
        assert_eq!(
            m.update(at(0), UP, [0.0, 0.0, 30.0]),
            Some(MotionEvent::Moving)
        );
        assert_eq!(m.update(at(500), UP, REST), None);
        assert_eq!(m.update(at(1000), UP, REST), Some(MotionEvent::Still));
    }
}

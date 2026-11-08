//! Touch detection and gesture decoding for the two touch pads.
//!
//! A touch increases a pad's raw count. [`GestureDecoder`] tracks a slowly
//! adapting baseline per pad, detects touches against it with hysteresis and
//! decodes taps, long touches and swipes from one pad to the other. It is fed
//! raw counts and timestamps; `touch::TouchInput` does
//! that from the hardware.

use embassy_time::{Duration, Instant};

/// One of the two touch pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// TOUCH_PAD1 on GPIO6, at the back of the cat's head.
    Back,
    /// TOUCH_PAD2 on GPIO7, at the front of the cat's head.
    Front,
}

impl Pad {
    /// Both pads.
    pub const ALL: [Pad; 2] = [Pad::Back, Pad::Front];

    /// Touch sensor channel of the pad (equal to the GPIO number).
    #[must_use]
    pub const fn channel(self) -> usize {
        match self {
            Pad::Back => 6,
            Pad::Front => 7,
        }
    }

    const fn other(self) -> Pad {
        match self {
            Pad::Back => Pad::Front,
            Pad::Front => Pad::Back,
        }
    }

    /// Index of the pad (0 or 1), e.g. for per-pad arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Pad::Back => 0,
            Pad::Front => 1,
        }
    }

    /// "back" or "front".
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Pad::Back => "back",
            Pad::Front => "front",
        }
    }
}

/// A touch is detected when the reading rises this many per mille above the baseline.
///
/// Measured on the VoCat: a finger on a pad gives +50..+85 per mille, while
/// the neighbouring pad picks up +10..+30 per mille through the housing.
pub const TOUCH_PERMILLE: u32 = 40;
/// A touch ends when the reading falls below this many per mille above the baseline.
pub const RELEASE_PERMILLE: u32 = 20;
/// The baseline only follows rising readings up to this many per mille above it.
pub const DRIFT_PERMILLE: u32 = 5;
/// Touches held at least this long are long touches.
pub const LONG_TOUCH: Duration = Duration::from_millis(800);
/// Touching the other pad within this time after a touch starts is a swipe.
pub const SWIPE_WINDOW: Duration = Duration::from_millis(400);
/// Events queued between two [`GestureDecoder::pop`] calls; one update
/// produces at most four.
const QUEUE: usize = 8;

/// Filtered state of one pad.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadReading {
    /// Latest raw count.
    pub raw: u32,
    /// Raw counts averaged over the last few updates.
    pub smooth: u32,
    /// Slowly adapting value of the untouched pad.
    pub baseline: u32,
    /// Debounced touch state.
    pub touched: bool,
}

impl PadReading {
    /// An untouched pad reading `raw`.
    #[must_use]
    pub fn idle(raw: u32) -> Self {
        Self {
            raw,
            smooth: raw,
            baseline: raw,
            touched: false,
        }
    }

    /// How far the smoothed reading is above the baseline, in per mille.
    #[must_use]
    pub fn delta_permille(&self) -> i32 {
        if self.baseline == 0 {
            return 0;
        }
        ((i64::from(self.smooth) - i64::from(self.baseline)) * 1000 / i64::from(self.baseline))
            as i32
    }

    /// Feeds one raw sample into the filter. Returns the new touch state if
    /// it changed.
    pub fn update(&mut self, raw: u32) -> Option<bool> {
        self.raw = raw;
        self.smooth = (self.smooth * 3 + raw) / 4;

        let above = self.smooth.saturating_sub(self.baseline);
        let was = self.touched;
        let threshold = if self.touched {
            RELEASE_PERMILLE
        } else {
            TOUCH_PERMILLE
        };
        self.touched = above * 1000 > self.baseline * threshold;

        // Follow slow drift (temperature, humidity) only while untouched.
        // Falling readings are followed faster, so a baseline taken during a
        // touch recovers. Rising readings are only followed close to the
        // baseline: a hovering finger or a touch on the other pad raises the
        // count by 10-30 per mille without crossing the threshold, and must
        // not drag the baseline up.
        if !self.touched {
            let diff = i64::from(self.smooth) - i64::from(self.baseline);
            let step = if diff < 0 {
                diff / 8
            } else if above * 1000 <= self.baseline * DRIFT_PERMILLE {
                diff / 64
            } else {
                0
            };
            self.baseline = (i64::from(self.baseline) + step) as u32;
        }

        (self.touched != was).then_some(self.touched)
    }
}

/// Something that happened on the pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    /// A pad started being touched.
    Down(Pad),
    /// A pad stopped being touched after the given time.
    Up(Pad, Duration),
    /// A short touch of one pad with no swipe following.
    /// Reported [`SWIPE_WINDOW`] after the touch started at the earliest.
    Tap(Pad),
    /// A pad has been held for [`LONG_TOUCH`]; reported while it is still held.
    Long(Pad),
    /// A touch of `from` followed by `to` within [`SWIPE_WINDOW`].
    Swipe {
        /// The pad touched first.
        from: Pad,
        /// The pad touched second.
        to: Pad,
    },
}

#[derive(Clone, Copy, Default)]
struct PadGesture {
    down_at: Option<Instant>,
    /// The current touch is already part of a long touch or swipe.
    consumed: bool,
}

/// Touch detection and gesture decoding for both pads.
pub struct GestureDecoder {
    readings: [PadReading; 2],
    gestures: [PadGesture; 2],
    /// A released short touch waiting to see whether a swipe follows.
    pending_tap: Option<(Pad, Instant)>,
    queue: [Option<TouchEvent>; QUEUE],
}

impl GestureDecoder {
    /// A decoder with the given untouched baselines (back, front).
    #[must_use]
    pub fn new(baselines: [u32; 2]) -> Self {
        Self {
            readings: baselines.map(PadReading::idle),
            gestures: [PadGesture::default(); 2],
            pending_tap: None,
            queue: [None; QUEUE],
        }
    }

    /// The latest filtered state of a pad.
    #[must_use]
    pub fn reading(&self, pad: Pad) -> PadReading {
        self.readings[pad.index()]
    }

    /// Feeds both pads' raw counts (back, front) read at `now`, and queues
    /// the resulting events.
    pub fn update(&mut self, now: Instant, raw: [u32; 2]) {
        for pad in Pad::ALL {
            if let Some(touched) = self.readings[pad.index()].update(raw[pad.index()]) {
                if touched {
                    self.on_down(pad, now);
                } else {
                    self.on_up(pad, now);
                }
            }

            let g = &mut self.gestures[pad.index()];
            if let Some(down_at) = g.down_at
                && !g.consumed
                && now - down_at >= LONG_TOUCH
            {
                g.consumed = true;
                self.push(TouchEvent::Long(pad));
            }
        }

        if let Some((pad, down_at)) = self.pending_tap
            && now - down_at >= SWIPE_WINDOW
        {
            self.pending_tap = None;
            self.push(TouchEvent::Tap(pad));
        }
    }

    /// The oldest queued event.
    pub fn pop(&mut self) -> Option<TouchEvent> {
        let event = self.queue[0].take();
        self.queue.rotate_left(1);
        event
    }

    fn push(&mut self, event: TouchEvent) {
        match self.queue.iter_mut().find(|e| e.is_none()) {
            Some(slot) => *slot = Some(event),
            // Only if nobody pops: drop the oldest, keep the newest.
            None => {
                self.queue.rotate_left(1);
                self.queue[QUEUE - 1] = Some(event);
            }
        }
    }

    fn on_down(&mut self, pad: Pad, now: Instant) {
        self.push(TouchEvent::Down(pad));
        self.gestures[pad.index()] = PadGesture {
            down_at: Some(now),
            consumed: false,
        };

        // Swipe: the other pad was touched shortly before, and is either still
        // held or its short touch is waiting as a pending tap.
        let other = pad.other();
        let g = self.gestures[other.index()];
        let held_recently = g
            .down_at
            .is_some_and(|t| !g.consumed && now - t <= SWIPE_WINDOW);
        let pending =
            matches!(self.pending_tap, Some((p, t)) if p == other && now - t <= SWIPE_WINDOW);
        if held_recently || pending {
            self.pending_tap = None;
            self.gestures[other.index()].consumed = true;
            self.gestures[pad.index()].consumed = true;
            self.push(TouchEvent::Swipe {
                from: other,
                to: pad,
            });
        }
    }

    fn on_up(&mut self, pad: Pad, now: Instant) {
        let g = core::mem::take(&mut self.gestures[pad.index()]);
        let down_at = g.down_at.unwrap_or(now);
        self.push(TouchEvent::Up(pad, now - down_at));
        if !g.consumed {
            // A tap only once no swipe can follow any more. An older pending
            // tap can't become a swipe now, so report it.
            if let Some((p, _)) = self.pending_tap.replace((pad, down_at)) {
                self.push(TouchEvent::Tap(p));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 10_000;
    /// +80 per mille: a finger.
    const TOUCH: u32 = 10_800;

    /// Feeds `raw` every 20 ms from `t_ms` to `until_ms` and collects events.
    fn feed(
        d: &mut GestureDecoder,
        t_ms: &mut u64,
        until_ms: u64,
        raw: [u32; 2],
    ) -> Vec<TouchEvent> {
        let mut out = Vec::new();
        while *t_ms < until_ms {
            d.update(Instant::from_millis(*t_ms), raw);
            while let Some(e) = d.pop() {
                out.push(e);
            }
            *t_ms += 20;
        }
        out
    }

    extern crate std;
    use std::vec::Vec;

    #[test]
    fn tap() {
        let mut d = GestureDecoder::new([BASE; 2]);
        let mut t = 0;
        let mut ev = feed(&mut d, &mut t, 200, [TOUCH, BASE]);
        ev.extend(feed(&mut d, &mut t, 1000, [BASE, BASE]));
        assert!(matches!(ev[0], TouchEvent::Down(Pad::Back)));
        assert!(matches!(ev[1], TouchEvent::Up(Pad::Back, _)));
        assert_eq!(ev[2], TouchEvent::Tap(Pad::Back));
        assert_eq!(ev.len(), 3);
    }

    #[test]
    fn long_touch_is_not_a_tap() {
        let mut d = GestureDecoder::new([BASE; 2]);
        let mut t = 0;
        let mut ev = feed(&mut d, &mut t, 1200, [BASE, TOUCH]);
        ev.extend(feed(&mut d, &mut t, 2000, [BASE, BASE]));
        assert!(ev.contains(&TouchEvent::Long(Pad::Front)));
        assert!(!ev.iter().any(|e| matches!(e, TouchEvent::Tap(_))));
    }

    #[test]
    fn swipe_back_to_front() {
        let mut d = GestureDecoder::new([BASE; 2]);
        let mut t = 0;
        let mut ev = feed(&mut d, &mut t, 100, [TOUCH, BASE]);
        ev.extend(feed(&mut d, &mut t, 200, [BASE, TOUCH]));
        ev.extend(feed(&mut d, &mut t, 1500, [BASE, BASE]));
        assert!(ev.contains(&TouchEvent::Swipe {
            from: Pad::Back,
            to: Pad::Front
        }));
        assert!(
            !ev.iter()
                .any(|e| matches!(e, TouchEvent::Tap(_) | TouchEvent::Long(_)))
        );
    }

    #[test]
    fn hover_on_the_neighbour_does_not_touch_or_drag_the_baseline() {
        let mut d = GestureDecoder::new([BASE; 2]);
        let mut t = 0;
        // +25 per mille for 10 s: below the threshold.
        let ev = feed(&mut d, &mut t, 10_000, [10_250, BASE]);
        assert!(ev.is_empty());
        assert_eq!(d.reading(Pad::Back).baseline, BASE);
        // A real touch is still detected afterwards.
        let ev = feed(&mut d, &mut t, 10_200, [TOUCH, BASE]);
        assert_eq!(ev[0], TouchEvent::Down(Pad::Back));
    }

    #[test]
    fn baseline_follows_a_falling_reading() {
        let mut r = PadReading::idle(BASE);
        for _ in 0..200 {
            r.update(9_000);
        }
        assert!(r.baseline < 9_100, "baseline {}", r.baseline);
    }

    #[test]
    fn full_queue_keeps_the_newest() {
        let mut d = GestureDecoder::new([BASE; 2]);
        for i in 0..20 {
            d.push(TouchEvent::Tap(if i % 2 == 0 {
                Pad::Back
            } else {
                Pad::Front
            }));
        }
        let mut last = None;
        while let Some(e) = d.pop() {
            last = Some(e);
        }
        assert_eq!(last, Some(TouchEvent::Tap(Pad::Front)));
    }
}

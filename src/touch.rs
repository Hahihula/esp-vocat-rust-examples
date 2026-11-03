//! The two capacitive touch pads (TOUCH6 on GPIO6, TOUCH7 on GPIO7).
//!
//! esp-hal has no touch sensor driver for the ESP32-S3, so [`TouchPads`]
//! programs the touch FSM directly, following ESP-IDF's `touch_ll` for the
//! S3: the FSM runs in timer mode and scans both channels continuously, and
//! the CPU only reads the raw counts.
//!
//! A touch increases the raw count. [`TouchInput`] tracks a slowly adapting
//! baseline per pad, detects touches against it with hysteresis and decodes
//! taps, long touches and swipes from one pad to the other.

use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO6, GPIO7, RTC_CNTL, RTC_IO, SENS};

/// Charge/discharge time per channel and scan, in 8 MHz cycles (ESP-IDF default).
const MEAS_CYCLES: u16 = 500;
/// Pause between scans, in RTC slow clock cycles (ESP-IDF default).
const SLEEP_CYCLES: u16 = 0xF;
/// Charge current, 0..=7 (ESP-IDF default slope).
const DAC_SLOPE: u8 = 7;

/// One of the two touch pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// TOUCH_PAD1 on GPIO6, at the back of the cat's head.
    Back,
    /// TOUCH_PAD2 on GPIO7, at the front of the cat's head.
    Front,
}

impl Pad {
    pub const ALL: [Pad; 2] = [Pad::Back, Pad::Front];

    /// Touch sensor channel of the pad (equal to the GPIO number).
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
    pub const fn index(self) -> usize {
        match self {
            Pad::Back => 0,
            Pad::Front => 1,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Pad::Back => "back",
            Pad::Front => "front",
        }
    }
}

/// The touch sensor hardware with both pads enabled.
pub struct TouchPads<'d> {
    _sens: SENS<'d>,
    _rtc_io: RTC_IO<'d>,
    _pad1: GPIO6<'d>,
    _pad2: GPIO7<'d>,
}

impl<'d> TouchPads<'d> {
    /// Configures GPIO6/GPIO7 as touch pads and starts continuous scanning.
    ///
    /// The first valid readings are available a few milliseconds later.
    pub fn new(sens: SENS<'d>, rtc_io: RTC_IO<'d>, pad1: GPIO6<'d>, pad2: GPIO7<'d>) -> Self {
        let sens_regs = SENS::regs();
        let rtc = RTC_CNTL::regs();
        let io = RTC_IO::regs();

        // RTC IO registers are only writable with their clock enabled.
        sens_regs
            .sar_peri_clk_gate_conf()
            .modify(|_, w| w.iomux_clk_en().set_bit());

        // Stop the FSM and select timer mode (touch_ll_set_fsm_mode).
        rtc.touch_ctrl2().modify(|_, w| {
            w.touch_start_fsm_en().set_bit();
            w.touch_start_en().clear_bit();
            w.touch_start_force().clear_bit();
            w.touch_slp_timer_en().clear_bit()
        });
        force_timer_done();

        // SAFETY (all `bits` calls below): the values fit their fields.
        rtc.touch_ctrl1().write(|w| unsafe {
            w.touch_meas_num().bits(MEAS_CYCLES);
            w.touch_sleep_cycles().bits(SLEEP_CYCLES)
        });
        rtc.touch_ctrl2().modify(|_, w| unsafe {
            // Charge between 0.5 V and 2.7 V.
            w.touch_drefh().bits(3);
            w.touch_drefl().bits(0);
            w.touch_drange().bits(3);
            w.touch_xpd_wait().bits(0xFF);
            w.touch_clkgate_en().set_bit()
        });
        rtc.touch_scan_ctrl().modify(|_, w| {
            // Inactive pads are grounded, no denoise channel, no shield pad.
            w.touch_inactive_connection().set_bit();
            w.touch_denoise_en().clear_bit();
            w.touch_shield_pad_en().clear_bit()
        });

        let mut mask = 0u16;
        for pad in Pad::ALL {
            let ch = pad.channel();
            // Route the pad to the RTC domain, no pulls, no digital input.
            io.touch_pad(ch).modify(|_, w| unsafe {
                w.mux_sel().set_bit();
                w.fun_sel().bits(0);
                w.fun_ie().clear_bit();
                w.slp_sel().clear_bit();
                w.slp_ie().clear_bit();
                w.slp_oe().clear_bit();
                w.rue().clear_bit();
                w.rde().clear_bit();
                w.tie_opt().clear_bit()
            });
            mask |= 1 << ch;
        }
        rtc.touch_dac().modify(|_, w| unsafe {
            w.touch_pad6_dac().bits(DAC_SLOPE);
            w.touch_pad7_dac().bits(DAC_SLOPE)
        });

        rtc.touch_scan_ctrl()
            .modify(|r, w| unsafe { w.touch_scan_pad_map().bits(r.touch_scan_pad_map().bits() | mask) });
        sens_regs.sar_touch_conf().modify(|r, w| unsafe {
            w.sar_touch_outen().bits(r.sar_touch_outen().bits() | mask);
            // Read raw data, not the hardware baseline or smoothed value.
            w.sar_touch_data_sel().bits(0)
        });

        // Start the FSM in timer mode (touch_ll_start_fsm).
        force_timer_done();
        rtc.touch_ctrl2().modify(|_, w| w.touch_slp_timer_en().set_bit());

        Self {
            _sens: sens,
            _rtc_io: rtc_io,
            _pad1: pad1,
            _pad2: pad2,
        }
    }

    /// Returns the latest raw count of a pad. Higher means more capacitance.
    pub fn read_raw(&self, pad: Pad) -> u32 {
        // `sar_touch_status(0)` is channel 1; channel 0 has no status register.
        SENS::regs()
            .sar_touch_status(pad.channel() - 1)
            .read()
            .data()
            .bits()
    }
}

fn force_timer_done() {
    let rtc = RTC_CNTL::regs();
    // SAFETY: 3 = force done, 0 = normal; both fit the 2-bit field.
    rtc.touch_ctrl2()
        .modify(|_, w| unsafe { w.touch_timer_force_done().bits(3) });
    rtc.touch_ctrl2()
        .modify(|_, w| unsafe { w.touch_timer_force_done().bits(0) });
}

/// Poll period of [`TouchInput`].
pub const POLL: Duration = Duration::from_millis(20);
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

/// Filtered state of one pad.
#[derive(Debug, Clone, Copy, Default)]
pub struct PadReading {
    /// Latest raw count.
    pub raw: u32,
    /// Raw counts averaged over the last few polls.
    pub smooth: u32,
    /// Slowly adapting value of the untouched pad.
    pub baseline: u32,
    /// Debounced touch state.
    pub touched: bool,
}

impl PadReading {
    /// How far the smoothed reading is above the baseline, in per mille.
    pub fn delta_permille(&self) -> i32 {
        if self.baseline == 0 {
            return 0;
        }
        ((i64::from(self.smooth) - i64::from(self.baseline)) * 1000 / i64::from(self.baseline)) as i32
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
    Swipe { from: Pad, to: Pad },
}

#[derive(Clone, Copy, Default)]
struct PadGesture {
    down_at: Option<Instant>,
    /// The current touch is already part of a long touch or swipe.
    consumed: bool,
}

/// Touch detection and gesture decoding on top of [`TouchPads`].
pub struct TouchInput<'d> {
    pads: TouchPads<'d>,
    readings: [PadReading; 2],
    gestures: [PadGesture; 2],
    /// A released short touch waiting to see whether a swipe follows.
    pending_tap: Option<(Pad, Instant)>,
    queue: [Option<TouchEvent>; 4],
}

impl<'d> TouchInput<'d> {
    /// Starts the pads and measures the initial baselines. Don't touch the
    /// pads during the ~200 ms this takes.
    pub async fn new(pads: TouchPads<'d>) -> Self {
        // Let the FSM finish a few scans.
        Timer::after(Duration::from_millis(50)).await;
        let mut sums = [0u32; 2];
        const SAMPLES: u32 = 8;
        for _ in 0..SAMPLES {
            Timer::after(POLL).await;
            for pad in Pad::ALL {
                sums[pad.index()] += pads.read_raw(pad);
            }
        }
        let mut readings = [PadReading::default(); 2];
        for pad in Pad::ALL {
            let avg = sums[pad.index()] / SAMPLES;
            readings[pad.index()] = PadReading {
                raw: avg,
                smooth: avg,
                baseline: avg,
                touched: false,
            };
        }
        Self {
            pads,
            readings,
            gestures: [PadGesture::default(); 2],
            pending_tap: None,
            queue: [None; 4],
        }
    }

    /// Returns the latest filtered state of a pad.
    pub fn reading(&self, pad: Pad) -> PadReading {
        self.readings[pad.index()]
    }

    /// Waits for the next event, polling the pads every [`POLL`].
    pub async fn next_event(&mut self) -> TouchEvent {
        loop {
            if let Some(event) = self.pop() {
                return event;
            }
            Timer::after(POLL).await;
            self.poll();
        }
    }

    fn push(&mut self, event: TouchEvent) {
        if let Some(slot) = self.queue.iter_mut().find(|e| e.is_none()) {
            *slot = Some(event);
        }
    }

    fn pop(&mut self) -> Option<TouchEvent> {
        let event = self.queue[0].take();
        self.queue.rotate_left(1);
        event
    }

    /// Reads both pads once and queues the resulting events.
    fn poll(&mut self) {
        let now = Instant::now();
        for pad in Pad::ALL {
            let raw = self.pads.read_raw(pad);
            if let Some(touched) = update_reading(&mut self.readings[pad.index()], raw) {
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
        let pending = matches!(self.pending_tap, Some((p, t)) if p == other && now - t <= SWIPE_WINDOW);
        if held_recently || pending {
            self.pending_tap = None;
            self.gestures[other.index()].consumed = true;
            self.gestures[pad.index()].consumed = true;
            self.push(TouchEvent::Swipe { from: other, to: pad });
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

/// Feeds one raw sample into a pad's filter. Returns the new touch state if it changed.
fn update_reading(r: &mut PadReading, raw: u32) -> Option<bool> {
    r.raw = raw;
    r.smooth = (r.smooth * 3 + raw) / 4;

    let above = r.smooth.saturating_sub(r.baseline);
    let was = r.touched;
    if r.touched {
        r.touched = above * 1000 > r.baseline * RELEASE_PERMILLE;
    } else {
        r.touched = above * 1000 > r.baseline * TOUCH_PERMILLE;
    }

    // Follow slow drift (temperature, humidity) only while untouched. Falling
    // readings are followed faster, so a baseline taken during a touch
    // recovers. Rising readings are only followed close to the baseline: a
    // hovering finger or a touch on the other pad raises the count by 10-30
    // per mille without crossing the threshold, and must not drag the
    // baseline up.
    if !r.touched {
        let diff = i64::from(r.smooth) - i64::from(r.baseline);
        let step = if diff < 0 {
            diff / 8
        } else if above * 1000 <= r.baseline * DRIFT_PERMILLE {
            diff / 64
        } else {
            0
        };
        r.baseline = (i64::from(r.baseline) + step) as u32;
    }

    (r.touched != was).then_some(r.touched)
}

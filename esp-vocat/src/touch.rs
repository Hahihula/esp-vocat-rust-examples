//! The two capacitive touch pads (TOUCH6 on GPIO6, TOUCH7 on GPIO7).
//!
//! esp-hal has no touch sensor driver for the ESP32-S3, so [`TouchPads`]
//! programs the touch FSM directly, following ESP-IDF's `touch_ll` for the
//! S3: the FSM runs in timer mode and scans both channels continuously, and
//! the CPU only reads the raw counts.
//!
//! A touch increases the raw count. [`TouchInput`] polls both pads and feeds
//! the counts to [`GestureDecoder`] ([`crate::gesture`], tested on the
//! host), which tracks a baseline per pad and decodes taps, long touches
//! and swipes from one pad to the other.

pub use crate::gesture::{
    DRIFT_PERMILLE, GestureDecoder, LONG_TOUCH, Pad, PadReading, RELEASE_PERMILLE, SWIPE_WINDOW,
    TOUCH_PERMILLE, TouchEvent,
};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO6, GPIO7, RTC_CNTL, RTC_IO, SENS};

/// Charge/discharge time per channel and scan, in 8 MHz cycles (ESP-IDF default).
const MEAS_CYCLES: u16 = 500;
/// Pause between scans, in RTC slow clock cycles (ESP-IDF default).
const SLEEP_CYCLES: u16 = 0xF;
/// Charge current, 0..=7 (ESP-IDF default slope).
const DAC_SLOPE: u8 = 7;

/// [`TouchPads::new`] with the pads taken out of
/// `esp_hal::peripherals::Peripherals`: `TouchInput::new(touch_pads!(p)).await`.
#[macro_export]
macro_rules! touch_pads {
    ($p:expr) => {
        $crate::touch::TouchPads::new($p.SENS, $p.RTC_IO, $p.GPIO6, $p.GPIO7)
    };
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

        rtc.touch_scan_ctrl().modify(|r, w| unsafe {
            w.touch_scan_pad_map()
                .bits(r.touch_scan_pad_map().bits() | mask)
        });
        sens_regs.sar_touch_conf().modify(|r, w| unsafe {
            w.sar_touch_outen().bits(r.sar_touch_outen().bits() | mask);
            // Read raw data, not the hardware baseline or smoothed value.
            w.sar_touch_data_sel().bits(0)
        });

        // Start the FSM in timer mode (touch_ll_start_fsm).
        force_timer_done();
        rtc.touch_ctrl2()
            .modify(|_, w| w.touch_slp_timer_en().set_bit());

        Self {
            _sens: sens,
            _rtc_io: rtc_io,
            _pad1: pad1,
            _pad2: pad2,
        }
    }

    /// Returns the latest raw count of a pad. Higher means more capacitance.
    #[must_use]
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

/// Touch detection and gesture decoding on top of [`TouchPads`].
pub struct TouchInput<'d> {
    pads: TouchPads<'d>,
    decoder: GestureDecoder,
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
        Self {
            pads,
            decoder: GestureDecoder::new(sums.map(|s| s / SAMPLES)),
        }
    }

    /// Returns the latest filtered state of a pad.
    #[must_use]
    pub fn reading(&self, pad: Pad) -> PadReading {
        self.decoder.reading(pad)
    }

    /// Waits for the next event, polling the pads every [`POLL`]. Safe to
    /// cancel: queued events stay queued.
    pub async fn next_event(&mut self) -> TouchEvent {
        loop {
            if let Some(event) = self.decoder.pop() {
                return event;
            }
            Timer::after(POLL).await;
            let raw = Pad::ALL.map(|pad| self.pads.read_raw(pad));
            self.decoder.update(Instant::now(), raw);
        }
    }
}

//! Plumbing the demos share: start-up, LCD bring-up, flushing, halting,
//! speaker volume steps.

use core::fmt::Debug;

use embassy_time::{Duration, Timer};
use esp_hal::peripherals::{DMA_CH0, SPI2};
use esp_vocat::{
    board::LcdReset,
    display::{FrameBuffer, Lcd, LcdPins},
};
use log::{error, warn};
use oxstack_drivers::es8311::Volume;

/// Speaker volume steps in dB (DAC gain of the ES8311). The 3 W speaker is
/// loud, so the demos start at [`START_VOLUME`].
pub const VOLUME_DB: [i16; 5] = [-30, -20, -12, -6, 0];
/// Index into [`VOLUME_DB`] the demos start at: -12 dB.
pub const START_VOLUME: usize = 2;

/// The ES8311 setting for volume step `step` of [`VOLUME_DB`].
#[must_use]
pub fn volume_of(step: usize) -> Volume {
    Volume::from_half_db(VOLUME_DB[step] * 2)
}

/// Starts a demo: HAL at full clock, logger, internal heap (the RAM the
/// bootloader used, plus `extra_heap` bytes if given), esp-rtos on TIMG0,
/// then waits for the USB console and logs the title. Binds the
/// peripherals to `$p`.
///
/// ```ignore
/// esp_vocat_examples::start!(p, "Demo 01: touch pads");
/// esp_vocat_examples::start!(p, "Demo 09: radio alarm clock", extra_heap = 96 * 1024);
/// ```
#[macro_export]
macro_rules! start {
    ($p:ident, $title:expr $(, extra_heap = $extra:expr)?) => {
        let $p = esp_vocat::board::init();
        esp_vocat::board::init_logger();
        esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: esp_vocat::board::RECLAIMED_HEAP);
        $( esp_alloc::heap_allocator!(size: $extra); )?
        let timg0 = esp_hal::timer::timg::TimerGroup::new($p.TIMG0);
        esp_rtos::start(timg0.timer0, $p.FROM_CPU_INTR0);
        esp_vocat::board::wait_for_console().await;
        log::info!("=== {} ===", $title);
    };
}

/// Resets and initialises the LCD (and with it the touch controller, which
/// shares the reset line), or halts with the error.
pub async fn lcd<'d>(
    spi2: SPI2<'d>,
    dma: DMA_CH0<'d>,
    pins: LcdPins<'d>,
    reset: &mut LcdReset<'_>,
) -> Lcd<'d> {
    match Lcd::new(spi2, dma, pins, reset).await {
        Ok(lcd) => lcd,
        Err(e) => halt("LCD init failed", e).await,
    }
}

/// Sends the changed rows to the LCD; a failure is logged and the rows are
/// sent with the next flush.
#[allow(clippy::large_stack_frames, reason = "async LCD transfers")]
pub async fn flush(lcd: &mut Lcd<'_>, fb: &mut FrameBuffer) {
    if let Err(e) = lcd.flush(fb).await {
        warn!("flush failed: {e:?}");
    }
}

/// Logs what failed and stops here (the executor keeps running other tasks).
pub async fn halt(what: &str, e: impl Debug) -> ! {
    error!("{what}: {e:?}");
    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

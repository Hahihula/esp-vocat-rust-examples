//! Demo 01: capacitive touch pads.
//!
//! Reads the two touch pads (back: GPIO6, front: GPIO7) through a
//! register-level driver, prints their raw counts once per second, and decodes
//! taps, long touches and swipes. The green LED is on while a pad is touched.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use log::info;
use vocat_rust_examples::{
    board::{self, GreenLed},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
};

esp_bootloader_esp_idf::esp_app_desc!();

/// How often the raw readings are printed.
const REPORT: Duration = Duration::from_secs(1);

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the touch input future for the whole program"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    info!("=== Demo 01: touch pads (TOUCH6/GPIO6, TOUCH7/GPIO7) ===");
    info!("Calibrating, don't touch the pads...");

    let mut led = GreenLed::new(peripherals.GPIO43);
    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut touch = TouchInput::new(pads).await;
    for pad in Pad::ALL {
        info!("{} baseline {}", pad.name(), touch.reading(pad).baseline);
    }
    info!("Tap, hold or swipe from one pad to the other.");

    let mut taps = [0u32; 2];
    let mut longs = [0u32; 2];
    let mut swipes = [0u32; 2];
    let mut next_report = Instant::now() + REPORT;

    loop {
        let event = match select(touch.next_event(), Timer::at(next_report)).await {
            Either::First(event) => event,
            Either::Second(()) => {
                next_report += REPORT;
                report(&touch);
                continue;
            }
        };

        match event {
            TouchEvent::Down(pad) => {
                led.set(true);
                info!("{} down", pad.name());
            }
            TouchEvent::Up(pad, held) => {
                if Pad::ALL.iter().all(|&p| !touch.reading(p).touched) {
                    led.set(false);
                }
                info!("{} up after {} ms", pad.name(), held.as_millis());
            }
            TouchEvent::Tap(pad) => {
                let n = &mut taps[pad.index()];
                *n += 1;
                info!("TAP   {} #{n}", pad.name());
            }
            TouchEvent::Long(pad) => {
                let n = &mut longs[pad.index()];
                *n += 1;
                info!("LONG  {} #{n}", pad.name());
            }
            TouchEvent::Swipe { from, to } => {
                let n = &mut swipes[from.index()];
                *n += 1;
                info!("SWIPE {} -> {} #{n}", from.name(), to.name());
            }
        }
    }
}

fn report(touch: &TouchInput<'_>) {
    let [a, b] = Pad::ALL.map(|p| touch.reading(p));
    info!(
        "raw/baseline: back {}/{} ({:+} ‰){} | front {}/{} ({:+} ‰){}",
        a.raw,
        a.baseline,
        a.delta_permille(),
        if a.touched { " TOUCHED" } else { "" },
        b.raw,
        b.baseline,
        b.delta_permille(),
        if b.touched { " TOUCHED" } else { "" },
    );
}

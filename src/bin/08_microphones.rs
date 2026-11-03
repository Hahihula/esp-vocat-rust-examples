//! Demo 08: microphones.
//!
//! Records both microphones through the ES7210 (16 kHz, 16-bit stereo over
//! I2S) and shows them on the round LCD: an oscilloscope of both channels and
//! a level meter (RMS, with peak hold) per microphone. Levels are also logged
//! twice a second, and a loud clap after quiet flashes the green LED.
//!
//! - back pad tap: next microphone gain step
//! - front pad tap: freeze / unfreeze the oscilloscope

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use core::fmt::Write;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use core::cell::RefCell;

use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_10X20},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use libm::{log10f, sqrtf};
use log::{info, warn};
use vocat_rust_examples::{
    audio::{AudioIn, I2sInPins},
    board::{self, GreenLed, LcdReset},
    display::{Backlight, FrameBuffer, Lcd, LcdPins},
    es7210::{Es7210, Gain},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
};

esp_bootloader_esp_idf::esp_app_desc!();

const SAMPLE_RATE: u32 = 16_000;
/// Samples per level measurement: 50 ms.
const BLOCK: usize = 800;
/// Samples kept for the oscilloscope (every sample, about 19 ms).
const SCOPE: usize = 300;
/// Gain steps in dB.
const GAINS_DB: [f32; 5] = [0.0, 12.0, 24.0, 30.0, 37.5];
const START_GAIN: usize = 3;

const MIC1: Rgb565 = Rgb565::YELLOW;
const MIC2: Rgb565 = Rgb565::CYAN;

/// One measurement block.
#[derive(Clone, Copy)]
struct Levels {
    rms_db: [f32; 2],
    peak_db: [f32; 2],
}

static LEVELS: Signal<CriticalSectionRawMutex, Levels> = Signal::new();
/// The first [`SCOPE`] samples of the latest block, (MIC1, MIC2). A static
/// rather than part of [`Levels`], so the 1.2 KiB never travel over the stack.
static SCOPE_BUF: Mutex<CriticalSectionRawMutex, RefCell<[[i16; 2]; SCOPE]>> =
    Mutex::new(RefCell::new([[0; 2]; SCOPE]));

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the LCD, codec and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    board::wait_for_console().await;

    info!("=== Demo 08: microphones (ES7210, 16 kHz) ===");
    info!("back tap = mic gain, front tap = freeze scope; clap to flash the LED");

    board::psram_heap(peripherals.PSRAM);
    let _power = board::peripheral_power_on(peripherals.GPIO9);
    let _codec_power = board::codec_power_on(peripherals.GPIO48);
    Timer::after(Duration::from_millis(20)).await;

    // I2S first: the ES7210 needs MCLK before it can be configured.
    let pins = I2sInPins {
        mclk: peripherals.GPIO42,
        bclk: peripherals.GPIO40,
        ws: peripherals.GPIO39,
        din: peripherals.GPIO3,
    };
    let input = match AudioIn::new(peripherals.I2S0, peripherals.DMA_CH1, pins, SAMPLE_RATE) {
        Ok(i) => i,
        Err(e) => halt("I2S init failed", e).await,
    };
    // The receive ring starts filling at once; start reading before anything
    // else, or it overflows and the DMA stops.
    spawner.spawn(mic_task(input).unwrap());

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut adc = Es7210::new(i2c);
    let mut gain = START_GAIN;
    match adc.chip_id().await {
        Ok(id) => info!("ES7210 chip ID {:02X} {:02X}", id[0], id[1]),
        Err(e) => halt("ES7210 did not answer", e).await,
    }
    if let Err(e) = adc.init(Gain::from_db(GAINS_DB[gain])).await {
        halt("ES7210 init failed", e).await;
    }
    info!("ES7210 running, gain {} dB", Gain::from_db(GAINS_DB[gain]).db());

    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO44);
    let mut reset = LcdReset::new(peripherals.GPIO47);
    let lcd_pins = LcdPins {
        clk: peripherals.GPIO18,
        d0: peripherals.GPIO46,
        d1: peripherals.GPIO13,
        d2: peripherals.GPIO11,
        d3: peripherals.GPIO12,
        cs: peripherals.GPIO14,
        dc: peripherals.GPIO45,
    };
    let mut lcd = match Lcd::new(peripherals.SPI2, peripherals.DMA_CH0, lcd_pins, &mut reset).await {
        Ok(lcd) => lcd,
        Err(e) => halt("LCD init failed", e).await,
    };
    let mut fb = FrameBuffer::new();
    let _ = fb.clear(Rgb565::BLACK);
    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let _ = Text::with_alignment("Microphones", Point::new(180, 50), style, Alignment::Center)
        .draw(&mut fb);
    flush(&mut lcd, &mut fb).await;
    backlight.fade(0, 100, Duration::from_millis(400));

    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut touch = TouchInput::new(pads).await;
    let mut led = GreenLed::new(peripherals.GPIO43);

    let mut frozen = false;
    let mut hold = [-90.0f32; 2];
    let mut hold_at = [Instant::now(); 2];
    // When the last block with a peak above -30 dBFS was seen.
    let mut last_loud = Instant::now();
    let mut led_off_at: Option<Instant> = None;
    let mut log_at = Instant::now();

    loop {
        let levels = match select(LEVELS.wait(), touch.next_event()).await {
            Either::First(l) => l,
            Either::Second(TouchEvent::Tap(Pad::Back)) => {
                gain = (gain + 1) % GAINS_DB.len();
                let g = Gain::from_db(GAINS_DB[gain]);
                match adc.set_gain(g).await {
                    Ok(()) => info!("gain {} dB", g.db()),
                    Err(e) => warn!("gain change failed: {e:?}"),
                }
                continue;
            }
            Either::Second(TouchEvent::Tap(Pad::Front)) => {
                frozen = !frozen;
                info!("scope {}", if frozen { "frozen" } else { "running" });
                continue;
            }
            Either::Second(_) => continue,
        };
        let now = Instant::now();

        // Peak hold for 1.5 s, then fall.
        for ch in 0..2 {
            if levels.peak_db[ch] >= hold[ch] || now - hold_at[ch] > Duration::from_millis(1500) {
                hold[ch] = levels.peak_db[ch];
                hold_at[ch] = now;
            }
        }

        // Clap: a loud peak after at least 300 ms of quiet.
        let peak = levels.peak_db[0].max(levels.peak_db[1]);
        if peak > -6.0 && now - last_loud > Duration::from_millis(300) {
            info!("CLAP ({peak:.1} dBFS)");
            led.set(true);
            led_off_at = Some(now + Duration::from_millis(150));
        }
        if peak >= -30.0 {
            last_loud = now;
        }
        if led_off_at.is_some_and(|t| now >= t) {
            led.set(false);
            led_off_at = None;
        }

        if now - log_at >= Duration::from_millis(500) {
            info!(
                "MIC1 {:6.1} dBFS (peak {:6.1})  MIC2 {:6.1} dBFS (peak {:6.1})",
                levels.rms_db[0], levels.peak_db[0], levels.rms_db[1], levels.peak_db[1]
            );
            log_at = now;
        }

        let scope = SCOPE_BUF.lock(|b| *b.borrow());
        draw(&mut fb, &levels, &scope, &hold, frozen, GAINS_DB[gain]);
        flush(&mut lcd, &mut fb).await;
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "embedded-graphics primitives and text formatting; called from main only"
)]
fn draw(
    fb: &mut FrameBuffer,
    levels: &Levels,
    scope: &[[i16; 2]; SCOPE],
    hold: &[f32; 2],
    frozen: bool,
    gain_db: f32,
) {
    // Oscilloscope, rows 75..195.
    const MID: i32 = 135;
    const HALF: i32 = 58;
    if !frozen {
        let _ = Rectangle::new(Point::new(30, MID - HALF - 2), Size::new(300, (HALF * 2 + 5) as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(fb);
        let _ = Line::new(Point::new(30, MID), Point::new(329, MID))
            .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, 1))
            .draw(fb);
        for (ch, color) in [(0, MIC1), (1, MIC2)] {
            let y = |i: usize| MID - i32::from(scope[i][ch]) * HALF / 32768;
            for i in 1..SCOPE {
                let _ = Line::new(Point::new(29 + i as i32, y(i - 1)), Point::new(30 + i as i32, y(i)))
                    .into_styled(PrimitiveStyle::with_stroke(color, 1))
                    .draw(fb);
            }
        }
    }

    // Level bars, -60..0 dBFS over x 50..310.
    const X0: i32 = 50;
    const W: i32 = 260;
    let x_of = |db: f32| X0 + ((db.clamp(-60.0, 0.0) + 60.0) / 60.0 * W as f32) as i32;
    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    for (ch, (y, color)) in [(215, MIC1), (255, MIC2)].into_iter().enumerate() {
        let _ = Rectangle::new(Point::new(20, y - 2), Size::new(320, 30))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(fb);
        let label = if ch == 0 { "1" } else { "2" };
        let _ = Text::new(label, Point::new(28, y + 18), MonoTextStyle::new(&FONT_10X20, color))
            .draw(fb);
        let _ = Rectangle::new(Point::new(X0, y), Size::new(W as u32, 24))
            .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, 1))
            .draw(fb);
        let x = x_of(levels.rms_db[ch]);
        if x > X0 {
            let _ = Rectangle::new(Point::new(X0 + 1, y + 1), Size::new((x - X0) as u32, 22))
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(fb);
        }
        let hx = x_of(hold[ch]);
        let _ = Line::new(Point::new(hx, y + 1), Point::new(hx, y + 22))
            .into_styled(PrimitiveStyle::with_stroke(Rgb565::RED, 2))
            .draw(fb);
    }

    let mut text = alloc::string::String::with_capacity(24);
    let _ = write!(text, "gain {gain_db:.1} dB{}", if frozen { "  [frozen]" } else { "" });
    let _ = Rectangle::new(Point::new(60, 292), Size::new(240, 26))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(fb);
    let _ = Text::with_alignment(&text, Point::new(180, 310), style, Alignment::Center).draw(fb);
}

/// Reads the microphones continuously and publishes a [`Levels`] block every
/// [`BLOCK`] samples.
#[embassy_executor::task]
async fn mic_task(mut input: AudioIn) {
    let mut sum_sq = [0f32; 2];
    let mut peak = [0i32; 2];
    let mut count = 0usize;
    let mut levels = Levels {
        rms_db: [-90.0; 2],
        peak_db: [-90.0; 2],
    };
    // Remove the DC offset with a slow high-pass (one-pole) per channel.
    let mut dc = [0f32; 2];

    loop {
        let result = input
            .read(|l, r| {
                let mut v = [0f32; 2];
                for (ch, s) in [l, r].into_iter().enumerate() {
                    let x = f32::from(s);
                    dc[ch] += (x - dc[ch]) * 0.001;
                    v[ch] = x - dc[ch];
                    sum_sq[ch] += v[ch] * v[ch];
                    peak[ch] = peak[ch].max(v[ch].abs() as i32);
                }
                if count < SCOPE {
                    SCOPE_BUF.lock(|b| b.borrow_mut()[count] = [v[0] as i16, v[1] as i16]);
                }
                count += 1;
                if count == BLOCK {
                    for ch in 0..2 {
                        let rms = sqrtf(sum_sq[ch] / BLOCK as f32);
                        levels.rms_db[ch] = db(rms);
                        levels.peak_db[ch] = db(peak[ch] as f32);
                        sum_sq[ch] = 0.0;
                        peak[ch] = 0;
                    }
                    LEVELS.signal(levels);
                    count = 0;
                }
            })
            .await;
        if let Err(e) = result {
            warn!("mic stream error: {e:?}");
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}

/// Amplitude to dBFS (full scale = 32768).
fn db(amplitude: f32) -> f32 {
    20.0 * log10f(amplitude.max(1.0) / 32768.0)
}

#[allow(clippy::large_stack_frames, reason = "async LCD transfers")]
async fn flush(lcd: &mut Lcd, fb: &mut FrameBuffer) {
    if let Err(e) = lcd.flush(fb).await {
        warn!("flush failed: {e:?}");
    }
}

async fn halt(what: &str, e: impl core::fmt::Debug) -> ! {
    warn!("{what}: {e:?}");
    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

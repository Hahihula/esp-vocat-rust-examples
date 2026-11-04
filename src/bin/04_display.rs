//! Demo 04: round 360×360 LCD (ST77916 over QSPI).
//!
//! Draws with embedded-graphics into a PSRAM frame buffer and sends the
//! changed rows over QSPI with DMA. Four screens, switched with a front tap:
//!
//! 1. test pattern: colour bars, grey ramp, circles and text
//! 2. solid colours, one per second (check the colour order)
//! 3. a ball bouncing off the round edge, flushing only the rows that changed
//! 4. full-frame benchmark: a moving gradient, every frame redrawn and sent
//!
//! A back tap steps the backlight brightness (100, 50, 20, 5 %).

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
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_10X20},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use log::{info, warn};
use vocat_rust_examples::{
    board::{self, LcdReset},
    display::{Backlight, FRAME_BYTES, FrameBuffer, HEIGHT, Lcd, LcdPins, SPI_CLOCK, WIDTH},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
};

esp_bootloader_esp_idf::esp_app_desc!();

const CENTER: Point = Point::new(WIDTH as i32 / 2, HEIGHT as i32 / 2);
const BRIGHTNESS: [u8; 4] = [100, 50, 20, 5];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    TestPattern,
    SolidColors,
    Ball,
    Benchmark,
}

impl Screen {
    fn next(self) -> Self {
        match self {
            Screen::TestPattern => Screen::SolidColors,
            Screen::SolidColors => Screen::Ball,
            Screen::Ball => Screen::Benchmark,
            Screen::Benchmark => Screen::TestPattern,
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the LCD and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    board::wait_for_console().await;

    info!("=== Demo 04: round LCD 360x360 (ST77916, QSPI) ===");
    info!("front tap = next screen, back tap = brightness");

    let psram = board::psram_heap(peripherals.PSRAM);
    info!("PSRAM heap: {} KiB (80 MHz)", psram / 1024);

    let _power = board::peripheral_power_on(peripherals.GPIO9);
    Timer::after(Duration::from_millis(20)).await;
    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO44);
    let mut reset = LcdReset::new(peripherals.GPIO47);

    let pins = LcdPins {
        clk: peripherals.GPIO18,
        d0: peripherals.GPIO46,
        d1: peripherals.GPIO13,
        d2: peripherals.GPIO11,
        d3: peripherals.GPIO12,
        cs: peripherals.GPIO14,
        dc: peripherals.GPIO45,
    };
    let mut lcd = match Lcd::new(peripherals.SPI2, peripherals.DMA_CH0, pins, &mut reset).await {
        Ok(lcd) => lcd,
        Err(e) => {
            warn!("LCD init failed: {e:?}");
            loop {
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    };
    info!("LCD initialised, SPI clock {} MHz", SPI_CLOCK.as_mhz());

    let mut fb = FrameBuffer::new();
    draw_test_pattern(&mut fb);
    let t0 = Instant::now();
    if let Err(e) = lcd.flush(&mut fb).await {
        warn!("flush failed: {e:?}");
    }
    let us = t0.elapsed().as_micros().max(1);
    info!(
        "full frame ({} KiB) sent in {} ms (~{} MiB/s)",
        FRAME_BYTES / 1024,
        us / 1000,
        FRAME_BYTES as u64 * 1_000_000 / us / (1024 * 1024)
    );

    let mut brightness = 0;
    backlight.fade(0, BRIGHTNESS[brightness], Duration::from_millis(500));

    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut touch = TouchInput::new(pads).await;

    let mut screen = Screen::TestPattern;
    loop {
        info!("screen: {screen:?}");
        let run = run_screen(screen, &mut lcd, &mut fb);
        // A screen runs until a tap; brightness changes keep it running.
        let next = async {
            loop {
                match touch.next_event().await {
                    TouchEvent::Tap(Pad::Front) => break,
                    TouchEvent::Tap(Pad::Back) => {
                        brightness = (brightness + 1) % BRIGHTNESS.len();
                        backlight.set(BRIGHTNESS[brightness]);
                        info!("brightness {} %", BRIGHTNESS[brightness]);
                    }
                    _ => {}
                }
            }
        };
        match select(run, next).await {
            Either::First(()) | Either::Second(()) => {}
        }
        screen = screen.next();
    }
}

/// Runs one screen forever (until the future is dropped).
#[allow(
    clippy::large_stack_frames,
    reason = "async LCD transfers; one screen future at a time"
)]
async fn run_screen(screen: Screen, lcd: &mut Lcd, fb: &mut FrameBuffer) {
    match screen {
        Screen::TestPattern => {
            draw_test_pattern(fb);
            flush(lcd, fb).await;
            core::future::pending::<()>().await;
        }
        Screen::SolidColors => {
            const COLORS: [(Rgb565, &str); 5] = [
                (Rgb565::RED, "red"),
                (Rgb565::GREEN, "green"),
                (Rgb565::BLUE, "blue"),
                (Rgb565::WHITE, "white"),
                (Rgb565::BLACK, "black"),
            ];
            loop {
                for (color, name) in COLORS {
                    info!("  {name}");
                    let _ = fb.clear(color);
                    flush(lcd, fb).await;
                    Timer::after(Duration::from_secs(1)).await;
                }
            }
        }
        Screen::Ball => bouncing_ball(lcd, fb).await,
        Screen::Benchmark => benchmark(lcd, fb).await,
    }
}

#[allow(clippy::large_stack_frames, reason = "async LCD transfers")]
async fn flush(lcd: &mut Lcd, fb: &mut FrameBuffer) {
    if let Err(e) = lcd.flush(fb).await {
        warn!("flush failed: {e:?}");
    }
}

fn draw_test_pattern(fb: &mut FrameBuffer) {
    let _ = fb.clear(Rgb565::BLACK);

    // Eight colour bars across the upper half.
    const BARS: [Rgb565; 8] = [
        Rgb565::WHITE,
        Rgb565::YELLOW,
        Rgb565::CYAN,
        Rgb565::GREEN,
        Rgb565::MAGENTA,
        Rgb565::RED,
        Rgb565::BLUE,
        Rgb565::BLACK,
    ];
    let bar_w = u32::from(WIDTH) / BARS.len() as u32;
    for (i, color) in BARS.iter().enumerate() {
        let _ = Rectangle::new(
            Point::new((i as u32 * bar_w) as i32, 60),
            Size::new(bar_w, 110),
        )
        .into_styled(PrimitiveStyle::with_fill(*color))
        .draw(fb);
    }

    // A 32-step grey ramp below.
    for i in 0..32u8 {
        let v = i * 8 + 4;
        let color = Rgb565::new(v >> 3, v >> 2, v >> 3);
        let _ = Rectangle::new(Point::new(20 + i32::from(i) * 10, 190), Size::new(10, 40))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(fb);
    }

    // The visible edge of the round panel and a centre mark.
    let _ = Circle::with_center(CENTER, u32::from(WIDTH) - 2)
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 2))
        .draw(fb);
    let _ = Circle::with_center(CENTER, 9)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
        .draw(fb);

    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let _ =
        Text::with_alignment("ESP-VoCat", Point::new(180, 40), style, Alignment::Center).draw(fb);
    let _ = Text::with_alignment(
        "ST77916 360x360",
        Point::new(180, 270),
        style,
        Alignment::Center,
    )
    .draw(fb);
    let _ = Text::with_alignment(
        "no_std Rust",
        Point::new(180, 300),
        style,
        Alignment::Center,
    )
    .draw(fb);
}

#[allow(
    clippy::large_stack_frames,
    reason = "async LCD transfers; one screen future at a time"
)]
async fn bouncing_ball(lcd: &mut Lcd, fb: &mut FrameBuffer) {
    const R: f32 = 22.0;
    // The ball bounces off a circle just inside the round panel edge.
    const BOUND: f32 = WIDTH as f32 / 2.0 - 3.0 - R;
    let (cx, cy) = (CENTER.x as f32, CENTER.y as f32);

    let _ = fb.clear(Rgb565::BLACK);
    let _ = Circle::with_center(CENTER, u32::from(WIDTH) - 4)
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, 2))
        .draw(fb);

    let (mut x, mut y) = (cx + 40.0, cy - 30.0);
    let (mut vx, mut vy) = (4.3f32, 3.1f32);
    let colors = [Rgb565::YELLOW, Rgb565::CYAN, Rgb565::MAGENTA, Rgb565::GREEN];
    let mut color = 0;
    let mut frames = 0u32;
    let mut fps_text = alloc::string::String::with_capacity(16);
    let mut window_start = Instant::now();
    let ball =
        |x: f32, y: f32| Circle::with_center(Point::new(x as i32, y as i32), (R * 2.0) as u32);

    loop {
        let _ = ball(x, y)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(fb);
        x += vx;
        y += vy;

        // Reflect the velocity on the circle's normal and put the ball back
        // onto the boundary.
        let (dx, dy) = (x - cx, y - cy);
        let dist = libm::sqrtf(dx * dx + dy * dy);
        if dist > BOUND {
            let (nx, ny) = (dx / dist, dy / dist);
            let dot = vx * nx + vy * ny;
            vx -= 2.0 * dot * nx;
            vy -= 2.0 * dot * ny;
            x = cx + nx * BOUND;
            y = cy + ny * BOUND;
            color = (color + 1) % colors.len();
        }
        let _ = ball(x, y)
            .into_styled(PrimitiveStyle::with_fill(colors[color]))
            .draw(fb);

        frames += 1;
        if window_start.elapsed() >= Duration::from_secs(1) {
            let fps = frames as u64 * 1000 / window_start.elapsed().as_millis().max(1);
            info!("  ball: {fps} fps");
            fps_text.clear();
            let _ = write!(fps_text, "{fps:3} fps");
            let _ = Rectangle::new(Point::new(130, 166), Size::new(100, 26))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
                .draw(fb);
            frames = 0;
            window_start = Instant::now();
        }
        // The fps label sits in the centre, under the ball's path.
        let _ = Text::with_alignment(
            &fps_text,
            Point::new(180, 186),
            MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_DIM_GRAY),
            Alignment::Center,
        )
        .draw(fb);

        // Sends the band of rows touched by the erase, the redraw and the text.
        flush(lcd, fb).await;
        // About 60 frames per second at most.
        Timer::after(Duration::from_millis(16)).await;
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "async LCD transfers; one screen future at a time"
)]
async fn benchmark(lcd: &mut Lcd, fb: &mut FrameBuffer) {
    let mut phase = 0u8;
    let mut frames = 0u32;
    let mut draw_us = 0u64;
    let mut send_us = 0u64;
    let mut window_start = Instant::now();
    loop {
        let t0 = Instant::now();
        // Vertical green bands scrolling upwards, every pixel changes. A
        // triangle wave keeps the gradient seamless.
        for y in 0..HEIGHT {
            let g = triangle((y / 2).wrapping_add(u16::from(phase) * 2), 64);
            let _ = Rectangle::new(Point::new(0, y.into()), Size::new(WIDTH.into(), 1))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::new(
                    triangle(u16::from(phase), 32),
                    g,
                    (31 - y * 31 / (HEIGHT - 1)) as u8,
                )))
                .draw(fb);
        }
        fb.mark_all_dirty();
        let t1 = Instant::now();
        flush(lcd, fb).await;
        let t2 = Instant::now();
        draw_us += (t1 - t0).as_micros();
        send_us += (t2 - t1).as_micros();
        phase = phase.wrapping_add(1);
        frames += 1;

        if window_start.elapsed() >= Duration::from_secs(2) {
            let ms = window_start.elapsed().as_millis().max(1);
            info!(
                "  benchmark: {} fps, per frame {} ms drawing + {} ms sending",
                frames as u64 * 1000 / ms,
                draw_us / u64::from(frames) / 1000,
                send_us / u64::from(frames) / 1000,
            );
            frames = 0;
            draw_us = 0;
            send_us = 0;
            window_start = Instant::now();
        }
        // Let the touch task run.
        Timer::after(Duration::from_millis(1)).await;
    }
}

/// A triangle wave over `0..levels`: rises for `levels` steps, then falls.
fn triangle(v: u16, levels: u16) -> u8 {
    let v = v % (2 * levels);
    (if v < levels { v } else { 2 * levels - 1 - v }) as u8
}

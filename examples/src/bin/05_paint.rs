//! Demo 05: finger painting on the touchscreen.
//!
//! Reads the CST816T touch controller (I2C 0x15, INT GPIO10) and paints
//! strokes into the frame buffer of the round LCD. A row of colour dots along
//! the bottom edge selects the colour.
//!
//! - front pad tap: clear the canvas
//! - back pad tap: next brush size

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, Line, PrimitiveStyle},
};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_vocat::{
    board::{self, LcdReset},
    display::{Backlight, FrameBuffer, WIDTH},
    lcd_pins,
    touch::{Pad, TouchEvent, TouchInput},
    touch_pads,
    touchscreen::{Cst816, Gesture, IrqLatch},
};
use log::{info, warn};

use esp_vocat_examples::{demo, demo::flush, demo::halt};

esp_bootloader_esp_idf::esp_app_desc!();

const CENTER: Point = Point::new(WIDTH as i32 / 2, WIDTH as i32 / 2);
/// Palette colours, shown as dots along the bottom edge.
const PALETTE: [Rgb565; 7] = [
    Rgb565::WHITE,
    Rgb565::RED,
    Rgb565::CSS_ORANGE,
    Rgb565::YELLOW,
    Rgb565::GREEN,
    Rgb565::CYAN,
    Rgb565::CSS_VIOLET,
];
/// Radius of a palette dot.
const DOT_R: i32 = 15;
/// Brush diameters in pixels.
const BRUSHES: [u32; 4] = [4, 8, 14, 24];
/// How often the controller is read while a finger is down, in case an INT
/// pulse is missed.
const POLL: Duration = Duration::from_millis(15);

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the LCD, touch and pad futures for the whole program"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_vocat_examples::start!(peripherals, "Demo 05: touchscreen paint (CST816T)");

    info!("paint with a finger, touch a colour dot to pick it");
    info!("front pad tap = clear, back pad tap = brush size");

    board::psram_heap(peripherals.PSRAM);
    let _power = board::peripheral_power_on(peripherals.GPIO9);
    Timer::after(Duration::from_millis(20)).await;
    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO44);
    let mut reset = LcdReset::new(peripherals.GPIO47);
    let pins = lcd_pins!(peripherals);
    // This also resets the touch controller, which shares the reset line.
    let mut lcd = demo::lcd(peripherals.SPI2, peripherals.DMA_CH0, pins, &mut reset).await;

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut ts = Cst816::new(i2c);
    match ts.init().await {
        Ok((id, fw)) => info!("touch controller: chip ID {id:02X}, firmware {fw:02X}"),
        Err(e) => halt("touch controller did not answer", e).await,
    }
    let ts_int = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Up),
    );
    spawner.spawn(touch_irq_task(ts_int).unwrap());

    let mut fb = FrameBuffer::new();
    let mut color = 0usize;
    let mut brush = 1usize;
    draw_canvas(&mut fb, color, brush);
    flush(&mut lcd, &mut fb).await;
    backlight.fade(0, 100, Duration::from_millis(400));

    let pads = touch_pads!(peripherals);
    let mut pads = TouchInput::new(pads).await;

    let mut last: Option<Point> = None;
    // False for a touch that started on the palette.
    let mut painting = false;
    let mut stroke_start = Instant::now();
    let mut stroke_points = 0u32;
    let mut last_gesture = Gesture::None;

    loop {
        // Wait for an INT pulse from the controller (latched by
        // touch_irq_task, so none are lost while drawing), or poll while a
        // finger is down.
        let wait_touch = async {
            if last.is_some() {
                let _ = select(TOUCH_IRQ.wait(), Timer::after(POLL)).await;
            } else {
                TOUCH_IRQ.wait().await;
            }
        };
        match select(wait_touch, pads.next_event()).await {
            Either::First(()) => {}
            Either::Second(TouchEvent::Tap(Pad::Front)) => {
                info!("clear");
                draw_canvas(&mut fb, color, brush);
                flush(&mut lcd, &mut fb).await;
                continue;
            }
            Either::Second(TouchEvent::Tap(Pad::Back)) => {
                brush = (brush + 1) % BRUSHES.len();
                info!("brush {} px", BRUSHES[brush]);
                draw_palette(&mut fb, color, brush);
                flush(&mut lcd, &mut fb).await;
                continue;
            }
            Either::Second(_) => continue,
        }

        let report = match ts.read().await {
            Ok(r) => r,
            Err(e) => {
                warn!("touch read failed: {e:?}");
                Timer::after(POLL).await;
                continue;
            }
        };
        // The gesture register keeps its value until the next touch.
        if report.gesture != last_gesture {
            last_gesture = report.gesture;
            if report.gesture != Gesture::None {
                info!(
                    "gesture {:?} at ({}, {})",
                    report.gesture, report.x, report.y
                );
            }
        }

        if !report.touched {
            if let Some(p) = last.take() {
                info!(
                    "up   at ({:3}, {:3}), {} points in {} ms",
                    p.x,
                    p.y,
                    stroke_points,
                    stroke_start.elapsed().as_millis()
                );
            }
            continue;
        }

        let p = Point::new(report.x.into(), report.y.into());
        if last.is_none() {
            info!("down at ({:3}, {:3})", p.x, p.y);
            stroke_start = Instant::now();
            stroke_points = 0;
            painting = match palette_hit(p) {
                Some(i) => {
                    color = i;
                    info!("colour {i}");
                    draw_palette(&mut fb, color, brush);
                    flush(&mut lcd, &mut fb).await;
                    false
                }
                None => true,
            };
        }
        stroke_points += 1;

        if painting {
            let style = PrimitiveStyle::with_stroke(PALETTE[color], BRUSHES[brush]);
            let from = last.unwrap_or(p);
            let _ = Line::new(from, p).into_styled(style).draw(&mut fb);
            // Round caps, so strokes and single dots look smooth.
            let _ = Circle::with_center(p, BRUSHES[brush])
                .into_styled(PrimitiveStyle::with_fill(PALETTE[color]))
                .draw(&mut fb);
            draw_palette(&mut fb, color, brush);
            flush(&mut lcd, &mut fb).await;
        }
        last = Some(p);
    }
}

/// The touch controller's INT pulses (GPIO10), latched so none are lost
/// while a stroke is drawn and flushed.
static TOUCH_IRQ: IrqLatch = IrqLatch::new();

#[embassy_executor::task]
async fn touch_irq_task(pin: Input<'static>) {
    TOUCH_IRQ.watch(pin).await
}

/// Centre of palette dot `i`, on an arc along the bottom edge.
fn palette_center(i: usize) -> Point {
    // Spread from 135 to 45 degrees (0 = right; y points down, so this is the
    // lower edge, left to right).
    let n = PALETTE.len() as f32;
    let angle = (135.0 - 90.0 * (i as f32 + 0.5) / n).to_radians();
    let r = (WIDTH as f32 / 2.0) - DOT_R as f32 - 8.0;
    Point::new(
        CENTER.x + (r * libm::cosf(angle)) as i32,
        CENTER.y + (r * libm::sinf(angle)) as i32,
    )
}

fn palette_hit(p: Point) -> Option<usize> {
    (0..PALETTE.len()).find(|&i| {
        let d = p - palette_center(i);
        // A bit larger than the dot, fingers are not precise.
        d.x * d.x + d.y * d.y <= (DOT_R + 6) * (DOT_R + 6)
    })
}

fn draw_palette(fb: &mut FrameBuffer, selected: usize, brush: usize) {
    for (i, &c) in PALETTE.iter().enumerate() {
        let center = palette_center(i);
        let ring = if i == selected {
            Rgb565::WHITE
        } else {
            Rgb565::CSS_DIM_GRAY
        };
        let _ = Circle::with_center(center, (DOT_R * 2 + 6) as u32)
            .into_styled(PrimitiveStyle::with_fill(ring))
            .draw(fb);
        let _ = Circle::with_center(center, (DOT_R * 2) as u32)
            .into_styled(PrimitiveStyle::with_fill(c))
            .draw(fb);
    }
    // Brush preview at the top.
    let preview = Point::new(CENTER.x, 30);
    let _ = Circle::with_center(preview, 30)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(fb);
    let _ = Circle::with_center(preview, BRUSHES[brush])
        .into_styled(PrimitiveStyle::with_fill(PALETTE[selected]))
        .draw(fb);
}

fn draw_canvas(fb: &mut FrameBuffer, selected: usize, brush: usize) {
    let _ = fb.clear(Rgb565::BLACK);
    draw_palette(fb, selected, brush);
}

//! Demo 10: battery gauge on the round LCD.
//!
//! Reads the BQ27220 fuel gauge every 2 s and shows it on the screen:
//!
//! - gauge screen: a 270° segmented ring filled up to the state of charge
//!   (red to green), the charge in large digits, the charging state with the
//!   current, voltage, power, capacity, temperature and the time to full or
//!   empty. While charging, a segment runs from the charge level to the end
//!   of the ring.
//! - details screen: every gauge value, including health, cycle count and
//!   the status flags.
//!
//! Touch pads: front tap switches the screen, back tap steps the backlight
//! brightness (100, 50, 20, 5 %).

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use alloc::{format, string::String};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Delay, Duration, Ticker, Timer};
use embedded_graphics::{
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_9X15, FONT_10X20},
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use esp_backtrace as _;
use esp_hal::{Async, i2c::master::I2c};
use esp_vocat::{
    battery::{Bq27220, DEVICE_NUMBER, Status},
    board::{self, LcdReset},
    display::{Backlight, FrameBuffer, HEIGHT, WIDTH},
    lcd_pins,
    touch::{Pad, TouchEvent, TouchInput},
    touch_pads,
};
use esp_vocat_examples::demo::{self, flush};
use log::{info, warn};

esp_bootloader_esp_idf::esp_app_desc!();

/// Animation frame period.
const FRAME: Duration = Duration::from_millis(250);
/// The gauge is read every this many frames (2 s).
const READ_EVERY: u32 = 8;
/// Currents within this band count as idle (full battery, or gauge noise).
const IDLE_BAND_MA: i16 = 15;
/// Below this charge the gauge turns red and the state text blinks.
const LOW_PCT: u16 = 15;
const BRIGHTNESS: [u8; 4] = [100, 50, 20, 5];

const CENTER: Point = Point::new(WIDTH as i32 / 2, HEIGHT as i32 / 2);

// Colours. This panel lifts dark colours a lot, so the dark ones are very dark.
const BG: Rgb565 = Rgb565::BLACK;
const OFF: Rgb565 = Rgb565::new(1, 2, 2);
const TEXT: Rgb565 = Rgb565::new(26, 52, 26);
const DIM: Rgb565 = Rgb565::new(12, 24, 12);
const RED: Rgb565 = Rgb565::new(31, 8, 4);
const AMBER: Rgb565 = Rgb565::new(31, 40, 0);
const GREEN: Rgb565 = Rgb565::new(4, 58, 8);
const CYAN: Rgb565 = Rgb565::new(4, 50, 31);

/// Ring geometry: number of segments, the angle of the first segment's start
/// (screen coordinates, clockwise from +x; 135° is bottom left), the sweep,
/// inner and outer radius.
const SEGMENTS: u16 = 50;
const RING_START_DEG: f32 = 135.0;
const RING_SWEEP_DEG: f32 = 270.0;
const RING_INNER: f32 = 146.0;
const RING_OUTER: f32 = 172.0;
const RING_STROKE: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChargeState {
    Charging,
    Discharging,
    Idle,
}

impl ChargeState {
    fn from_current(ma: i16) -> Self {
        if ma > IDLE_BAND_MA {
            ChargeState::Charging
        } else if ma < -IDLE_BAND_MA {
            ChargeState::Discharging
        } else {
            ChargeState::Idle
        }
    }

    fn label(self, s: &Status) -> &'static str {
        match self {
            ChargeState::Charging => "CHARGING",
            ChargeState::Discharging => "ON BATTERY",
            ChargeState::Idle if s.flags.full_charge() => "FULL",
            ChargeState::Idle => "IDLE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Gauge,
    Details,
}

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the LCD, gauge and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_vocat_examples::start!(peripherals, "Demo 10: battery gauge (BQ27220 on the LCD)");

    info!("front tap = gauge/details, back tap = brightness");

    let psram = board::psram_heap(peripherals.PSRAM);
    info!("PSRAM heap: {} KiB", psram / 1024);

    let _power = board::peripheral_power_on(peripherals.GPIO9);
    Timer::after(Duration::from_millis(20)).await;
    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO44);
    let mut reset = LcdReset::new(peripherals.GPIO47);
    let pins = lcd_pins!(peripherals);
    let mut lcd = demo::lcd(peripherals.SPI2, peripherals.DMA_CH0, pins, &mut reset).await;

    let mut fb = FrameBuffer::new();
    let _ = fb.clear(BG);
    text(&mut fb, "reading gauge...", CENTER.y, DIM, true);
    flush(&mut lcd, &mut fb).await;
    let mut brightness = 0;
    backlight.fade(0, BRIGHTNESS[brightness], Duration::from_millis(500));

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut gauge = Bq27220::new(i2c, esp_vocat::board::i2c_addr::BQ27220);
    match (
        gauge.device_number(&mut Delay).await,
        gauge.firmware_version(&mut Delay).await,
    ) {
        (Ok(DEVICE_NUMBER), Ok(fw)) => info!("BQ27220 found, firmware {fw:04X}"),
        (Ok(other), _) => warn!("unexpected device number {other:04X}"),
        (Err(e), _) => warn!("fuel gauge did not answer: {e:?}"),
    }

    let pads = touch_pads!(peripherals);
    let mut touch = TouchInput::new(pads).await;

    let mut screen = Screen::Gauge;
    let mut status: Option<Status> = None;
    let mut state = ChargeState::Idle;
    let mut frame = 0u32;
    let mut ticker = Ticker::every(FRAME);

    loop {
        if frame.is_multiple_of(READ_EVERY) {
            match read_status(&mut gauge).await {
                Some(s) => {
                    let new_state = ChargeState::from_current(s.current_ma);
                    if status.is_none() || new_state != state {
                        info!("state: {}", new_state.label(&s));
                    }
                    state = new_state;
                    status = Some(s);
                }
                None => status = None,
            }
        }

        match (&status, screen) {
            (Some(s), Screen::Gauge) => draw_gauge(&mut fb, s, state, frame),
            (Some(s), Screen::Details) => draw_details(&mut fb, s, state),
            (None, _) => draw_no_gauge(&mut fb),
        }
        flush(&mut lcd, &mut fb).await;
        frame = frame.wrapping_add(1);

        // Wait for the next frame; taps are handled as they come.
        loop {
            match select(ticker.next(), touch.next_event()).await {
                Either::First(()) => break,
                Either::Second(TouchEvent::Tap(Pad::Front)) => {
                    screen = match screen {
                        Screen::Gauge => Screen::Details,
                        Screen::Details => Screen::Gauge,
                    };
                    info!("screen: {screen:?}");
                    break;
                }
                Either::Second(TouchEvent::Tap(Pad::Back)) => {
                    brightness = (brightness + 1) % BRIGHTNESS.len();
                    backlight.set(BRIGHTNESS[brightness]);
                    info!("brightness {} %", BRIGHTNESS[brightness]);
                }
                Either::Second(_) => {}
            }
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "the async I2C transfers of a full gauge read"
)]
async fn read_status(gauge: &mut Bq27220<I2c<'static, Async>>) -> Option<Status> {
    match gauge.status().await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("gauge read failed: {e:?}");
            None
        }
    }
}

// ---------------------------------------------------------------- drawing

/// Colour of ring segment `i`: red at the start, through amber, to green.
fn segment_color(i: u16) -> Rgb565 {
    let t = f32::from(i) / f32::from(SEGMENTS - 1);
    let lerp = |a: u8, b: u8, t: f32| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    let (a, b, t) = if t < 0.5 {
        (RED, AMBER, t * 2.0)
    } else {
        (AMBER, GREEN, (t - 0.5) * 2.0)
    };
    Rgb565::new(
        lerp(a.r(), b.r(), t),
        lerp(a.g(), b.g(), t),
        lerp(a.b(), b.b(), t),
    )
}

/// The colour that stands for a charge level.
fn level_color(pct: u16) -> Rgb565 {
    if pct <= LOW_PCT {
        RED
    } else if pct < 50 {
        AMBER
    } else {
        GREEN
    }
}

/// Point at `angle_deg` (screen coordinates) and `r` from the centre.
fn polar(angle_deg: f32, r: f32) -> Point {
    let a = angle_deg.to_radians();
    Point::new(
        CENTER.x + libm::roundf(r * libm::cosf(a)) as i32,
        CENTER.y + libm::roundf(r * libm::sinf(a)) as i32,
    )
}

fn draw_ring(fb: &mut FrameBuffer, pct: u16, state: ChargeState, frame: u32) {
    let lit = (u32::from(pct.min(100)) * u32::from(SEGMENTS)).div_ceil(100) as u16;
    // Low charge: the whole lit part is red.
    let low = pct <= LOW_PCT;
    // Charging: a segment runs from the charge level to the end of the ring.
    let runner = (state == ChargeState::Charging && lit < SEGMENTS)
        .then(|| lit + (frame % u32::from(SEGMENTS - lit)) as u16);
    let step = RING_SWEEP_DEG / f32::from(SEGMENTS);
    for i in 0..SEGMENTS {
        let angle = RING_START_DEG + (f32::from(i) + 0.5) * step;
        let color = if i < lit {
            if low { RED } else { segment_color(i) }
        } else if Some(i) == runner {
            CYAN
        } else {
            OFF
        };
        let _ = Line::new(polar(angle, RING_INNER), polar(angle, RING_OUTER))
            .into_styled(PrimitiveStyle::with_stroke(color, RING_STROKE))
            .draw(fb);
    }
}

/// Seven-segment digit geometry: width, height, segment thickness, top edge.
const DW: i32 = 44;
const DH: i32 = 78;
const DT: i32 = 9;
const DY: i32 = 78;
const DGAP: i32 = 10;
/// Segments a..g (bit 0..6) of each digit.
const SEVEN_SEG: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

#[allow(
    clippy::large_stack_frames,
    reason = "inlined frame buffer fills; not recursive"
)]
fn draw_digit(fb: &mut FrameBuffer, x: i32, digit: u8, on: Rgb565) {
    let (y, w, h, t) = (DY, DW, DH, DT);
    let half = h / 2;
    let vertical = half - t - t / 2;
    let rects = [
        (x + t, y, w - 2 * t, t),                       // a
        (x + w - t, y + t, t, vertical),                // b
        (x + w - t, y + half + t / 2 + 1, t, vertical), // c
        (x + t, y + h - t, w - 2 * t, t),               // d
        (x, y + half + t / 2 + 1, t, vertical),         // e
        (x, y + t, t, vertical),                        // f
        (x + t, y + half - t / 2, w - 2 * t, t),        // g
    ];
    for (i, (rx, ry, rw, rh)) in rects.into_iter().enumerate() {
        let color = if SEVEN_SEG[usize::from(digit)] & (1 << i) != 0 {
            on
        } else {
            OFF
        };
        let _ = fb.fill_solid(
            &Rectangle::new(
                Point::new(rx + 1, ry + 1),
                Size::new(rw as u32 - 2, rh as u32 - 2),
            ),
            color,
        );
    }
}

/// The charge in large digits, centred with a "%" after it.
fn draw_percent(fb: &mut FrameBuffer, pct: u16, color: Rgb565) {
    let pct = pct.min(100);
    let digits: &[u8] = &[(pct / 100) as u8, (pct / 10 % 10) as u8, (pct % 10) as u8];
    // Leading zeros are dropped; "0" stays.
    let skip = if pct >= 100 {
        0
    } else if pct >= 10 {
        1
    } else {
        2
    };
    let digits = &digits[skip..];
    let n = digits.len() as i32;
    let percent_w = 22;
    let total = n * DW + (n - 1) * DGAP + DGAP + percent_w;
    let mut x = CENTER.x - total / 2;
    for &d in digits {
        draw_digit(fb, x, d, color);
        x += DW + DGAP;
    }
    text_left(fb, "%", Point::new(x, DY + DH), color, true);
}

#[allow(
    clippy::large_stack_frames,
    reason = "formatting of the gauge lines; called from main only"
)]
fn draw_gauge(fb: &mut FrameBuffer, s: &Status, state: ChargeState, frame: u32) {
    let _ = fb.clear(BG);
    let pct = s.state_of_charge_pct;
    draw_ring(fb, pct, state, frame);

    let low = pct <= LOW_PCT;
    draw_percent(fb, pct, level_color(pct));

    // Charging state and current; blinks when low and on battery.
    let blink_off = low && state == ChargeState::Discharging && frame % 4 >= 2;
    let state_color = match state {
        ChargeState::Charging => CYAN,
        ChargeState::Discharging if low => RED,
        ChargeState::Discharging => AMBER,
        ChargeState::Idle => GREEN,
    };
    if !blink_off {
        text(fb, state.label(s), 192, state_color, true);
    }

    // Voltage times current: the gauge's average power register disagrees
    // with its current on this board (see demo 02).
    let power_mw = i32::from(s.voltage_mv) * i32::from(s.current_ma) / 1000;
    text(
        fb,
        &format!("{:+} mA  {}", s.current_ma, watts(power_mw)),
        218,
        TEXT,
        false,
    );
    text(
        fb,
        &format!(
            "{}.{:02} V   {}",
            s.voltage_mv / 1000,
            s.voltage_mv % 1000 / 10,
            temp(s.temperature_dc)
        ),
        240,
        TEXT,
        false,
    );
    text(
        fb,
        &format!("{} / {} mAh", s.remaining_mah, s.full_charge_mah),
        262,
        DIM,
        false,
    );

    let estimate = match (state, s.time_to_full_min, s.time_to_empty_min) {
        (ChargeState::Charging, Some(m), _) => format!("full in {}", hm(m)),
        (ChargeState::Discharging, _, Some(m)) => format!("empty in {}", hm(m)),
        _ => String::new(),
    };
    text(fb, &estimate, 290, TEXT, true);

    // Health in the gap at the bottom of the ring.
    text(
        fb,
        &format!("health {} %", s.state_of_health_pct),
        330,
        DIM,
        false,
    );
}

#[allow(
    clippy::large_stack_frames,
    reason = "formatting of many lines; called from main only"
)]
fn draw_details(fb: &mut FrameBuffer, s: &Status, state: ChargeState) {
    let _ = fb.clear(BG);
    let _ = Circle::with_center(CENTER, u32::from(WIDTH) - 4)
        .into_styled(PrimitiveStyle::with_stroke(
            level_color(s.state_of_charge_pct),
            2,
        ))
        .draw(fb);
    text(fb, "FUEL GAUGE", 50, TEXT, true);

    let f = s.flags;
    let opt = |m: Option<u16>| m.map_or(String::from("-"), hm);
    let rows = [
        format!("state      {}", state.label(s)),
        format!("voltage    {} mV", s.voltage_mv),
        format!("current    {:+} mA", s.current_ma),
        format!("average    {:+} mA", s.average_current_ma),
        format!("avg power  {:+} mW", s.average_power_mw),
        format!("charge     {} %", s.state_of_charge_pct),
        format!("health     {} %", s.state_of_health_pct),
        format!("remaining  {} mAh", s.remaining_mah),
        format!("full/des.  {}/{} mAh", s.full_charge_mah, s.design_mah),
        format!("temp       {}", temp(s.temperature_dc)),
        format!("to empty   {}", opt(s.time_to_empty_min)),
        format!("to full    {}", opt(s.time_to_full_min)),
        format!("cycles     {}", s.cycle_count),
    ];
    // Rows are left-aligned on a column that fits inside the circle.
    let x = CENTER.x - 9 * 21 / 2;
    for (i, row) in rows.iter().enumerate() {
        text_left(fb, row, Point::new(x, 80 + i as i32 * 17), TEXT, false);
    }

    let mut flags = String::new();
    for (on, name) in [
        (!f.battery_present(), "NO-BATT"),
        (f.full_charge(), "full"),
        (f.fully_discharged(), "empty"),
        (f.system_down(), "sys-down"),
        (f.charge_inhibit(), "chg-inhibit"),
        (f.over_temp_charge(), "OT-chg"),
        (f.over_temp_discharge(), "OT-dsg"),
    ] {
        if on {
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str(name);
        }
    }
    text(fb, &format!("flags {:04X}", f.0), 315, DIM, false);
    text(fb, &flags, 333, AMBER, false);
}

fn draw_no_gauge(fb: &mut FrameBuffer) {
    let _ = fb.clear(BG);
    draw_ring(fb, 0, ChargeState::Idle, 0);
    text(fb, "NO GAUGE", CENTER.y - 6, RED, true);
    text(fb, "BQ27220 not answering", CENTER.y + 20, DIM, false);
}

// ---------------------------------------------------------------- text

/// Text centred on the screen's vertical axis, `y` is the baseline.
fn text(fb: &mut FrameBuffer, s: &str, y: i32, color: Rgb565, large: bool) {
    let font = if large { &FONT_10X20 } else { &FONT_9X15 };
    let _ = Text::with_alignment(
        s,
        Point::new(CENTER.x, y),
        MonoTextStyle::new(font, color),
        Alignment::Center,
    )
    .draw(fb);
}

/// Left-aligned text with its baseline at `at.y`.
fn text_left(fb: &mut FrameBuffer, s: &str, at: Point, color: Rgb565, large: bool) {
    let font = if large { &FONT_10X20 } else { &FONT_9X15 };
    let _ = Text::with_alignment(s, at, MonoTextStyle::new(font, color), Alignment::Left).draw(fb);
}

/// Minutes as `h:mm h`.
fn hm(m: u16) -> String {
    format!("{}:{:02} h", m / 60, m % 60)
}

/// Temperature in 0.1 °C as `45.3 C` (the fonts have no degree sign).
fn temp(dc: i16) -> String {
    let sign = if dc < 0 { "-" } else { "" };
    format!(
        "{sign}{}.{} C",
        dc.unsigned_abs() / 10,
        dc.unsigned_abs() % 10
    )
}

/// Power in mW as `+1.53 W`.
fn watts(mw: i32) -> String {
    let sign = if mw < 0 { '-' } else { '+' };
    let mw = mw.unsigned_abs();
    format!("{sign}{}.{:02} W", mw / 1000, mw % 1000 / 10)
}

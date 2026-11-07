//! Demo 06: googly cat eyes.
//!
//! Two cat eyes on the round LCD whose pupils are moved by the accelerometer:
//! they fall towards "down" and swing when the cat is moved, bouncing off the
//! edge of the eye. The eyes blink on their own, and react to input:
//!
//! - touch an eye on the screen: that eye blinks; touch elsewhere: both blink
//! - shake the cat: dizzy, spinning pupils
//! - back pad tap (petting): happy ^ ^ eyes

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embassy_time::{Delay, Duration, Instant, Ticker, Timer};
use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, Ellipse, Line, PrimitiveStyle, Rectangle, Triangle},
};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use libm::{cosf, sinf, sqrtf};
use log::{info, warn};
use oxstack_drivers::bmi270::{AccelRange, Bmi270, Config, DataRate, GyroRange};
use esp_vocat::{
    board::{self, LcdReset, i2c_addr},
    display::{Backlight, FrameBuffer, Lcd, LcdPins},
    motion::{MotionDetector, MotionEvent},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
    touchscreen::Cst816,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// Sensor axis and sign that point to the right of the screen.
const SCREEN_X: (usize, f32) = (0, 1.0);
/// Sensor axis and sign that point down the screen. Upright the
/// accelerometer reads [0, 0, -1] g, so down is the sensor's +Z.
const SCREEN_Y: (usize, f32) = (2, 1.0);

const FRAME: Duration = Duration::from_millis(20);
const BACKGROUND: Rgb565 = Rgb565::new(3, 6, 3);
const SCLERA: Rgb565 = Rgb565::new(30, 61, 28);
const IRIS: Rgb565 = Rgb565::new(14, 50, 2);
const IRIS_RIM: Rgb565 = Rgb565::new(6, 26, 1);
const LID: Rgb565 = Rgb565::new(8, 16, 8);
const LID_EDGE: Rgb565 = Rgb565::new(2, 4, 2);

/// Eye centres and radius.
const EYES: [Point; 2] = [Point::new(105, 150), Point::new(255, 150)];
const EYE_R: i32 = 68;
const IRIS_R: i32 = 38;
/// How far the iris centre may move from the eye centre.
const TRAVEL: f32 = (EYE_R - IRIS_R - 4) as f32;

/// Pupil acceleration per g of "down", in pixels/s².
const PULL: f32 = 2200.0;
/// Velocity damping per second.
const DAMPING: f32 = 3.0;
/// Fraction of the speed kept when a pupil hits the edge.
const BOUNCE: f32 = 0.45;

const BLINK: Duration = Duration::from_millis(260);
const DIZZY: Duration = Duration::from_millis(2000);
const HAPPY: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Default)]
struct Pupil {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
}

impl Pupil {
    fn step(&mut self, ax: f32, ay: f32, dt: f32) {
        self.vx += (ax * PULL - self.vx * DAMPING) * dt;
        self.vy += (ay * PULL - self.vy * DAMPING) * dt;
        self.x += self.vx * dt;
        self.y += self.vy * dt;

        let dist = sqrtf(self.x * self.x + self.y * self.y);
        if dist > TRAVEL {
            let (nx, ny) = (self.x / dist, self.y / dist);
            self.x = nx * TRAVEL;
            self.y = ny * TRAVEL;
            // Reflect the outward part of the velocity, losing some energy.
            let out = self.vx * nx + self.vy * ny;
            if out > 0.0 {
                self.vx -= (1.0 + BOUNCE) * out * nx;
                self.vy -= (1.0 + BOUNCE) * out * ny;
            }
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the LCD, IMU and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    board::wait_for_console().await;

    info!("=== Demo 06: googly cat eyes (BMI270 + LCD) ===");
    info!("tilt or move the cat; touch an eye to blink; shake = dizzy; back pad tap = happy");

    board::psram_heap(peripherals.PSRAM);
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
        Err(e) => halt("LCD init failed", e).await,
    };

    // The touch controller and the IMU share the I2C bus.
    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let bus: Mutex<NoopRawMutex, _> = Mutex::new(i2c);
    let mut ts = Cst816::new(I2cDevice::new(&bus));
    if let Err(e) = ts.init().await {
        warn!("touch controller did not answer: {e:?}");
    }
    let mut imu = Bmi270::new(I2cDevice::new(&bus), i2c_addr::BMI270);
    let config = Config {
        accel_range: AccelRange::G4,
        gyro_range: GyroRange::Dps1000,
        rate: DataRate::Hz100,
    };
    if let Err(e) = imu.init(config, &mut Delay).await {
        halt("BMI270 init failed", e).await;
    }

    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut pads = TouchInput::new(pads).await;

    let mut fb = FrameBuffer::new();
    draw_face(&mut fb);
    let mut pupils = [Pupil::default(); 2];
    let mut blink_at: [Option<Instant>; 2] = [None; 2];
    let mut next_blink = Instant::now() + Duration::from_secs(3);
    let mut dizzy_until = Instant::now();
    let mut happy_until = Instant::now();
    let mut was_touched = false;
    let mut rng = 0x2545_F491u32;

    let first = imu
        .read()
        .await
        .map(|s| s.accel)
        .unwrap_or([0.0, 0.0, -1.0]);
    let mut motion = MotionDetector::new(first);

    draw_eyes(&mut fb, &pupils, &blink_at, false);
    flush(&mut lcd, &mut fb).await;
    backlight.fade(0, 100, Duration::from_millis(500));

    let mut ticker = Ticker::every(FRAME);
    let mut last_frame = Instant::now();
    let mut frames = 0u32;
    let mut report_at = Instant::now() + Duration::from_secs(2);
    let mut spin = 0.0f32;

    loop {
        match select(ticker.next(), pads.next_event()).await {
            Either::First(()) => {}
            Either::Second(TouchEvent::Tap(Pad::Back) | TouchEvent::Long(Pad::Back)) => {
                info!("purr: happy eyes");
                happy_until = Instant::now() + HAPPY;
                continue;
            }
            Either::Second(_) => continue,
        }

        let now = Instant::now();
        let dt = (now - last_frame).as_micros() as f32 / 1e6;
        last_frame = now;

        // Down in screen coordinates, in g. The accelerometer measures the
        // reaction to gravity (pointing up) plus the opposite of any motion,
        // which is exactly what makes googly eyes swing.
        let (ax, ay) = match imu.read().await {
            Ok(s) => {
                if let Some(MotionEvent::Shake) = motion.update(s.accel, s.gyro) {
                    info!("shake: dizzy");
                    dizzy_until = now + DIZZY;
                }
                (
                    -s.accel[SCREEN_X.0] * SCREEN_X.1,
                    -s.accel[SCREEN_Y.0] * SCREEN_Y.1,
                )
            }
            Err(_) => (0.0, 0.0),
        };

        let dizzy = now < dizzy_until;
        if dizzy {
            spin += 14.0 * dt;
            for (i, p) in pupils.iter_mut().enumerate() {
                let a = if i == 0 { spin } else { -spin };
                *p = Pupil {
                    x: TRAVEL * cosf(a),
                    y: TRAVEL * sinf(a),
                    ..Pupil::default()
                };
            }
        } else {
            for p in &mut pupils {
                p.step(ax, ay, dt);
            }
        }

        // Touching the screen makes an eye blink.
        if let Ok(r) = ts.read().await {
            if r.touched && !was_touched {
                let p = Point::new(r.x.into(), r.y.into());
                let hit = EYES.iter().position(|&c| {
                    let d = p - c;
                    d.x * d.x + d.y * d.y <= EYE_R * EYE_R
                });
                match hit {
                    Some(i) => {
                        info!("touch on eye {i}: blink");
                        blink_at[i] = Some(now);
                    }
                    None => {
                        info!("touch: both blink");
                        blink_at = [Some(now); 2];
                    }
                }
            }
            was_touched = r.touched;
        }

        // Spontaneous blinks every 2-6 s.
        if now >= next_blink {
            blink_at = [Some(now); 2];
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            next_blink = now + Duration::from_millis(2000 + u64::from(rng % 4000));
        }
        for b in &mut blink_at {
            if b.is_some_and(|t| now - t > BLINK) {
                *b = None;
            }
        }

        draw_eyes(&mut fb, &pupils, &blink_at, now < happy_until);
        flush(&mut lcd, &mut fb).await;

        frames += 1;
        if now >= report_at {
            info!(
                "{} fps, down on screen ({:+.2}, {:+.2}) g",
                frames / 2,
                ax,
                ay
            );
            frames = 0;
            report_at = now + Duration::from_secs(2);
        }
    }
}

/// How far the lid is closed during a blink, 0..=1.
fn lid(blink_at: Option<Instant>) -> f32 {
    let Some(t) = blink_at else { return 0.0 };
    let ms = t.elapsed().as_millis() as f32;
    let total = BLINK.as_millis() as f32;
    // Close in a third of the time, open in the rest.
    if ms < total / 3.0 {
        ms / (total / 3.0)
    } else {
        (1.0 - (ms - total / 3.0) / (total * 2.0 / 3.0)).max(0.0)
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "embedded-graphics styled primitives; called from main only"
)]
fn draw_eyes(
    fb: &mut FrameBuffer,
    pupils: &[Pupil; 2],
    blink_at: &[Option<Instant>; 2],
    happy: bool,
) {
    for (i, &c) in EYES.iter().enumerate() {
        // Clear the eye area.
        let _ = Rectangle::with_center(c, Size::new(EYE_R as u32 * 2 + 8, EYE_R as u32 * 2 + 8))
            .into_styled(PrimitiveStyle::with_fill(BACKGROUND))
            .draw(fb);

        if happy {
            // ^ shaped closed eye.
            let style = PrimitiveStyle::with_stroke(SCLERA, 12);
            let _ = Line::new(c + Point::new(-45, 20), c + Point::new(0, -20))
                .into_styled(style)
                .draw(fb);
            let _ = Line::new(c + Point::new(0, -20), c + Point::new(45, 20))
                .into_styled(style)
                .draw(fb);
            continue;
        }

        let _ = Circle::with_center(c, EYE_R as u32 * 2)
            .into_styled(PrimitiveStyle::with_fill(SCLERA))
            .draw(fb);
        let iris = c + Point::new(pupils[i].x as i32, pupils[i].y as i32);
        let _ = Circle::with_center(iris, IRIS_R as u32 * 2)
            .into_styled(PrimitiveStyle::with_fill(IRIS_RIM))
            .draw(fb);
        let _ = Circle::with_center(iris, IRIS_R as u32 * 2 - 8)
            .into_styled(PrimitiveStyle::with_fill(IRIS))
            .draw(fb);
        // Vertical cat pupil and a highlight.
        let _ = Ellipse::with_center(iris, Size::new(14, 54))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(fb);
        let _ = Circle::with_center(iris + Point::new(-12, -14), 12)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
            .draw(fb);

        // Upper and lower lid close towards the middle, like a cartoon blink.
        let closed = lid(blink_at[i]);
        if closed > 0.0 {
            let k = (closed * EYE_R as f32) as i32;
            let cover = |fb: &mut FrameBuffer, y: i32| {
                let half = sqrtf((EYE_R * EYE_R - y * y).max(0) as f32) as i32 + 1;
                let _ = Rectangle::new(c + Point::new(-half, y), Size::new(half as u32 * 2, 1))
                    .into_styled(PrimitiveStyle::with_fill(LID))
                    .draw(fb);
            };
            for dy in 0..k {
                cover(fb, -EYE_R + dy);
                cover(fb, EYE_R - dy);
            }
            // Lid edges; they meet in one line when the eye is shut.
            for y in [-EYE_R + k, EYE_R - k] {
                let half = sqrtf((EYE_R * EYE_R - y * y).max(0) as f32) as i32;
                let _ = Line::new(c + Point::new(-half, y), c + Point::new(half, y))
                    .into_styled(PrimitiveStyle::with_stroke(LID_EDGE, 4))
                    .draw(fb);
            }
        }
    }
}

/// Background, nose and whiskers; drawn once.
fn draw_face(fb: &mut FrameBuffer) {
    let _ = fb.clear(BACKGROUND);
    let nose = Rgb565::new(31, 32, 18);
    let _ = Triangle::new(
        Point::new(166, 248),
        Point::new(194, 248),
        Point::new(180, 266),
    )
    .into_styled(PrimitiveStyle::with_fill(nose))
    .draw(fb);
    let mouth = PrimitiveStyle::with_stroke(nose, 3);
    let _ = Line::new(Point::new(180, 266), Point::new(166, 282))
        .into_styled(mouth)
        .draw(fb);
    let _ = Line::new(Point::new(180, 266), Point::new(194, 282))
        .into_styled(mouth)
        .draw(fb);
    let whisker = PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, 2);
    for (dy, tilt) in [(-6, -14), (6, 0), (18, 14)] {
        let y = 262 + dy;
        let _ = Line::new(Point::new(140, y), Point::new(50, y + tilt))
            .into_styled(whisker)
            .draw(fb);
        let _ = Line::new(Point::new(220, y), Point::new(310, y + tilt))
            .into_styled(whisker)
            .draw(fb);
    }
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

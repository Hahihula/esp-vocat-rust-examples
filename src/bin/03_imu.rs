//! Demo 03: BMI270 IMU.
//!
//! Uploads the BMI270 feature-engine config, samples accelerometer and
//! gyroscope at 200 Hz and detects tilt, moving/still, shakes and knocks in
//! software. Prints a status line once per second. The green LED is on while
//! the cat is moving.
//!
//! - front tap: take the current position as upright
//! - back tap: print a detailed reading with the IMU temperature

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Delay, Duration, Instant, Ticker};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use log::{info, warn};
use oxstack_drivers::bmi270::{AccelRange, Bmi270, Config, DataRate, GyroRange, Sample};
use vocat_rust_examples::{
    board::{self, GreenLed, i2c_addr},
    motion::{MotionDetector, MotionEvent, norm},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
};

esp_bootloader_esp_idf::esp_app_desc!();

/// Sample period, matching the 200 Hz output data rate.
const SAMPLE: Duration = Duration::from_millis(5);
/// Status line period.
const REPORT: Duration = Duration::from_secs(1);

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the IMU, touch and timer futures for the whole program"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    board::wait_for_console().await;

    info!("=== Demo 03: BMI270 IMU (I2C 0x68) ===");
    info!("front tap = set upright, back tap = detailed reading");

    let mut led = GreenLed::new(peripherals.GPIO43);
    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut imu = Bmi270::new(i2c, i2c_addr::BMI270);
    let config = Config {
        accel_range: AccelRange::G4,
        gyro_range: GyroRange::Dps1000,
        rate: DataRate::Hz200,
    };
    let t0 = Instant::now();
    match imu.init(config, &mut Delay).await {
        Ok(()) => info!(
            "BMI270 ready after {} ms (config upload), ±4 g, ±1000 °/s, 200 Hz",
            t0.elapsed().as_millis()
        ),
        Err(e) => {
            warn!("BMI270 init failed: {e:?}");
            loop {
                embassy_time::Timer::after(Duration::from_secs(1)).await;
            }
        }
    }

    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut touch = TouchInput::new(pads).await;

    // The position at start-up is upright; average a few samples for it.
    let up = average_accel(&mut imu).await;
    info!(
        "upright reference: [{:+.2} {:+.2} {:+.2}] g (sensor axes)",
        up[0], up[1], up[2]
    );
    let mut motion = MotionDetector::new(up);

    let mut sample_tick = Ticker::every(SAMPLE);
    let mut report_tick = Ticker::every(REPORT);
    let mut last = Sample::default();
    let mut errors = 0u32;

    loop {
        match select3(sample_tick.next(), report_tick.next(), touch.next_event()).await {
            Either3::First(()) => {
                let s = match imu.read().await {
                    Ok(s) => s,
                    Err(e) => {
                        errors += 1;
                        if errors.is_power_of_two() {
                            warn!("IMU read failed ({errors}x): {e:?}");
                        }
                        continue;
                    }
                };
                last = s;
                if let Some(event) = motion.update(s.accel, s.gyro) {
                    report_event(event, &mut led);
                }
            }
            Either3::Second(()) => {
                let a = last.accel;
                let g = last.gyro;
                info!(
                    "acc [{:+.2} {:+.2} {:+.2}] g |a| {:.2}  gyro [{:+6.1} {:+6.1} {:+6.1}] °/s  tilt {:5.1}°  peak jerk {:.2} g",
                    a[0],
                    a[1],
                    a[2],
                    norm(a),
                    g[0],
                    g[1],
                    g[2],
                    motion.tilt_deg(),
                    motion.take_peak_jerk(),
                );
            }
            Either3::Third(TouchEvent::Tap(Pad::Front)) => {
                let up = average_accel(&mut imu).await;
                motion.set_upright(up);
                info!(
                    "new upright reference: [{:+.2} {:+.2} {:+.2}] g",
                    up[0], up[1], up[2]
                );
            }
            Either3::Third(TouchEvent::Tap(Pad::Back)) => {
                let temp = imu.temperature().await.ok().flatten();
                info!("--- IMU ---");
                info!("accel  {:?} g", last.accel);
                info!("gyro   {:?} °/s", last.gyro);
                info!(
                    "|a|    {:.3} g, tilt {:.1}°",
                    norm(last.accel),
                    motion.tilt_deg()
                );
                match temp {
                    Some(t) => info!("temperature {t:.1} °C (IMU die)"),
                    None => info!("temperature not available yet"),
                }
            }
            Either3::Third(_) => {}
        }
    }
}

fn report_event(event: MotionEvent, led: &mut GreenLed<'_>) {
    match event {
        MotionEvent::Moving => {
            led.set(true);
            info!("MOVING");
        }
        MotionEvent::Still => {
            led.set(false);
            info!("STILL");
        }
        MotionEvent::Tilted(deg) => info!("TILTED {deg:.0}°"),
        MotionEvent::Upright => info!("UPRIGHT"),
        MotionEvent::UpsideDown => info!("UPSIDE DOWN"),
        MotionEvent::Shake => info!("SHAKE"),
        MotionEvent::Knock(jerk) => info!("KNOCK ({jerk:.2} g)"),
    }
}

/// Averages 32 accelerometer samples (~160 ms); hold the cat still meanwhile.
#[allow(
    clippy::large_stack_frames,
    reason = "async I2C transfer future; called from main only"
)]
async fn average_accel<I: embedded_hal_async::i2c::I2c>(imu: &mut Bmi270<I>) -> [f32; 3] {
    const N: u16 = 32;
    let mut sum = [0.0f32; 3];
    let mut tick = Ticker::every(SAMPLE);
    for _ in 0..N {
        tick.next().await;
        if let Ok(s) = imu.read().await {
            for (acc, v) in sum.iter_mut().zip(s.accel) {
                *acc += v;
            }
        }
    }
    sum.map(|v| v / f32::from(N))
}

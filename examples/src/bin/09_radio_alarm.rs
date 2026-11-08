//! Demo 09: internet radio alarm clock.
//!
//! Gets the time over Wi-Fi (SNTP, resynced every 6 hours), shows a clock
//! face on the round LCD and wakes you with an internet radio stream whose
//! volume rises over a minute. If the stream doesn't play (no network, server
//! down), it beeps instead.
//!
//! Touchscreen:
//! - clock: touch to open the alarm editor
//! - editor: +/- for hours and minutes (hold to repeat), ON/OFF, OK
//! - ringing: touch to stop
//!
//! Pads:
//! - back tap: alarm on/off (ringing: stop)
//! - front tap: radio on/off (ringing: snooze 5 minutes)
//! - front long touch: set the alarm to the next minute, for testing
//! - back long touch: next speaker volume step

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
use core::{
    cell::Cell,
    sync::atomic::{AtomicU8, Ordering},
};

use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select, select3};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::{Mutex as BlockingMutex, raw::CriticalSectionRawMutex},
    mutex::Mutex,
};
use embassy_time::{Delay, Duration, Instant, Timer};
use embedded_graphics::{
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_9X15, FONT_10X20},
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle},
    text::{Alignment, Text},
};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_vocat::{
    audio::{AudioOut, Error, Note, Synth, Wave},
    board::{self, LcdReset, i2c_addr},
    display::{Backlight, FrameBuffer},
    i2s_out_pins, lcd_pins,
    touch::{Pad, TouchEvent, TouchInput},
    touch_pads,
    touchscreen::{Cst816, IrqLatch},
};
use esp_vocat_examples::{
    clock::{self, Clock, ClockNow, LocalTime},
    demo::{self, START_VOLUME, VOLUME_DB, flush, halt, volume_of},
    net,
    radio::{self, Player, Stats},
};
use log::{info, warn};
use oxstack_drivers::es8311::{Es8311, Mode};

esp_bootloader_esp_idf::esp_app_desc!();

const NTP_SERVER: &str = "pool.ntp.org";
const NTP_RESYNC: Duration = Duration::from_secs(6 * 3600);
/// Codec DAC volume steps in dB; the software ramp works below these.
/// Volume ramp of the alarm, and of the radio switched on by hand.
const ALARM_RAMP: Duration = Duration::from_secs(60);
const MANUAL_RAMP: Duration = Duration::from_secs(3);
const SNOOZE_SECS: u64 = 5 * 60;
/// The alarm gives up after this long.
const RING_LIMIT_SECS: u64 = 30 * 60;
/// Beep instead if the stream hasn't produced audio for this long.
const STREAM_GRACE_SECS: u64 = 15;
/// The editor closes itself after this long without a touch.
const EDIT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ALARM: u16 = 7 * 60;
/// Backlight in percent, by day and between 22:00 and 07:00.
const BRIGHT_DAY: u8 = 100;
const BRIGHT_NIGHT: u8 = 15;
/// How long the screen stays bright at night after a touch.
const WAKE_BRIGHT: Duration = Duration::from_secs(20);
/// Touch controller poll interval while a finger is down.
const TOUCH_POLL: Duration = Duration::from_millis(30);
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_RATE: Duration = Duration::from_millis(90);
const STATS_PERIOD: u64 = 10;

/// The time, shared with the NTP task.
static CLOCK: BlockingMutex<CriticalSectionRawMutex, Cell<Option<Clock>>> =
    BlockingMutex::new(Cell::new(None));

fn clock() -> Clock {
    CLOCK.lock(|c| c.get()).expect("clock is set before use")
}

/// Fallback beeper volume in percent; 0 = off.
static BEEP_GAIN: AtomicU8 = AtomicU8::new(0);

const BEEP: &[Note] = &[
    Note::new(880.0, 120),
    Note::new(0.0, 80),
    Note::new(880.0, 120),
    Note::new(0.0, 80),
    Note::new(880.0, 120),
    Note::new(0.0, 700),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Clock,
    Edit,
    Ringing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    HourUp,
    HourDown,
    MinuteUp,
    MinuteDown,
    Toggle,
    Done,
}

const BUTTONS: [Button; 6] = [
    Button::HourUp,
    Button::HourDown,
    Button::MinuteUp,
    Button::MinuteDown,
    Button::Toggle,
    Button::Done,
];

impl Button {
    /// The +/- buttons repeat while held.
    fn repeats(self) -> bool {
        !matches!(self, Button::Toggle | Button::Done)
    }
}

struct App {
    screen: Screen,
    alarm_on: bool,
    /// Alarm time in minutes after midnight.
    alarm: u16,
    /// Unix minute the alarm last went off, so it fires once per minute.
    fired_minute: Option<u64>,
    snooze_until: Option<u64>,
    /// Unix time the alarm started ringing.
    ring_start: u64,
    /// Unix time the stream last produced audio while ringing.
    last_audio: u64,
    last_frames: u32,
    volume: usize,
    edit_touched: Instant,
}

impl App {
    fn status(&self) -> String {
        if let Some(s) = self.snooze_until {
            let t = clock::local_time(s).date;
            format!("snoozed until {:02}:{:02}", t.hour, t.minute)
        } else if self.alarm_on {
            format!("alarm {}", hhmm(self.alarm))
        } else {
            "alarm off".into()
        }
    }
}

fn hhmm(minute_of_day: u16) -> String {
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the network, LCD, touch and codec futures for the whole program"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // The Wi-Fi driver needs about 60 KiB of internal RAM on top of the
    // default heap.
    esp_vocat_examples::start!(
        peripherals,
        "Demo 09: radio alarm clock",
        extra_heap = 96 * 1024
    );

    info!("screen: touch = edit alarm | back pad: alarm on/off | front pad: radio on/off");
    info!("front long = alarm in 1 min (test) | back long = volume");
    info!("ringing: screen or back pad = stop, front pad = snooze");
    board::psram_heap(peripherals.PSRAM);

    // Audio first, so the DMA ring is running before the network starts.
    let _codec_power = board::codec_power_on(peripherals.GPIO48);
    let _lcd_power = board::peripheral_power_on(peripherals.GPIO9);
    Timer::after(Duration::from_millis(20)).await;
    let pins = i2s_out_pins!(peripherals);
    let out = match AudioOut::new(peripherals.I2S0, peripherals.DMA_CH1, pins) {
        Ok(out) => out,
        Err(e) => halt("I2S init failed", e).await,
    };
    spawner.spawn(audio_task(out, radio::player()).unwrap());

    // The LCD reset also resets the touch controller, so it goes before I2C.
    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO44);
    let mut reset = LcdReset::new(peripherals.GPIO47);
    let lcd_pins = lcd_pins!(peripherals);
    let mut lcd = demo::lcd(peripherals.SPI2, peripherals.DMA_CH0, lcd_pins, &mut reset).await;
    let mut fb = FrameBuffer::new();
    message(&mut fb, "connecting to Wi-Fi...");
    flush(&mut lcd, &mut fb).await;
    backlight.fade(0, BRIGHT_DAY, Duration::from_millis(400));

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let bus = Mutex::<CriticalSectionRawMutex, _>::new(i2c);
    let mut codec = Es8311::new(I2cDevice::new(&bus), i2c_addr::ES8311);
    let setup = async {
        codec.init(&mut Delay).await?;
        codec.set_dac_volume(volume_of(START_VOLUME)).await?;
        codec.set_mode(Mode::Playback).await?;
        codec.set_dac_mute(false).await
    };
    if let Err(e) = setup.await {
        halt("ES8311 init failed", e).await;
    }
    let _pa = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());
    info!("ES8311 ready, volume {} dB", VOLUME_DB[START_VOLUME]);

    let mut ts = Cst816::new(I2cDevice::new(&bus));
    if let Err(e) = ts.init().await {
        halt("touch controller did not answer", e).await;
    }
    let ts_int = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Up),
    );
    spawner.spawn(touch_irq_task(ts_int).unwrap());

    let pads = touch_pads!(peripherals);
    let mut pads = TouchInput::new(pads).await;

    // Network and time.
    let Some((ssid, password)) = net::credentials() else {
        message(&mut fb, "no Wi-Fi credentials");
        flush(&mut lcd, &mut fb).await;
        halt(
            "no Wi-Fi credentials",
            "build with WIFI_SSID=... WIFI_PASSWORD=... cargo run --release --bin 09_radio_alarm",
        )
        .await
    };
    let config = match net::station_config(ssid, password) {
        Ok(c) => c,
        Err(e) => halt("bad Wi-Fi credentials", e).await,
    };
    let (controller, interface) = match net::init_wifi(peripherals.WIFI, Some(config)) {
        Ok(w) => w,
        Err(e) => halt("Wi-Fi init failed", e).await,
    };
    info!("Station configured for '{ssid}'");
    let seed = u64::from(esp_hal::rng::Rng::new().random()) << 32
        | u64::from(esp_hal::rng::Rng::new().random());
    let stack = net::start(&spawner, controller, interface, seed);

    let address = net::wait_for_address(stack).await;
    info!("DHCP: address {address}");
    message(&mut fb, "getting the time...");
    flush(&mut lcd, &mut fb).await;
    let unix = loop {
        match net::ntp_time(stack, NTP_SERVER).await {
            Ok(unix) => break unix,
            Err(e) => {
                warn!("NTP failed: {e:?}, retrying in 5 s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    };
    CLOCK.lock(|c| c.set(Some(clock::synced(unix))));
    let t = clock().local();
    info!(
        "NTP: {} {} (UTC{:+}:{:02}{})",
        t.weekday_name(),
        t.date,
        t.offset_min / 60,
        t.offset_min.rem_euclid(60),
        if t.summer { ", summer time" } else { "" }
    );
    spawner.spawn(ntp_task(stack).unwrap());

    let url = radio::url();
    info!("Radio stream: {url}");
    spawner.spawn(radio::stream_task(stack, url).unwrap());

    let mut app = App {
        screen: Screen::Clock,
        alarm_on: false,
        alarm: DEFAULT_ALARM,
        fired_minute: None,
        snooze_until: None,
        ring_start: 0,
        last_audio: 0,
        last_frames: 0,
        volume: START_VOLUME,
        edit_touched: Instant::now(),
    };
    info!("{}", app.status());

    let mut finger = false;
    let mut held: Option<Button> = None;
    let mut repeat_at = Instant::now();
    let mut bright_until = Instant::now() + WAKE_BRIGHT;
    let mut brightness = BRIGHT_DAY;
    let mut stats = Stats::now();
    let mut redraw = true;

    loop {
        if redraw {
            draw(&mut fb, &app, &clock().local());
            flush(&mut lcd, &mut fb).await;
            redraw = false;
        }

        // Next second boundary of the clock.
        let next_tick = clock().next_tick(Instant::now());
        // INT pulses are latched by touch_irq_task, so none are lost while
        // this loop draws; poll as well while a finger is down.
        let wait_touch = async {
            if finger {
                let _ = select(TOUCH_IRQ.wait(), Timer::after(TOUCH_POLL)).await;
            } else {
                TOUCH_IRQ.wait().await;
            }
        };

        match select3(wait_touch, pads.next_event(), Timer::at(next_tick)).await {
            Either3::First(()) => {
                let report = match ts.read().await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("touch read failed: {e:?}");
                        continue;
                    }
                };
                bright_until = Instant::now() + WAKE_BRIGHT;
                if !report.touched {
                    finger = false;
                    held = None;
                    continue;
                }
                let p = Point::new(report.x.into(), report.y.into());
                if !finger {
                    finger = true;
                    held = None;
                    match app.screen {
                        Screen::Ringing => stop_alarm(&mut app, "stopped from the screen"),
                        Screen::Clock => {
                            info!("editing the alarm");
                            app.screen = Screen::Edit;
                            app.edit_touched = Instant::now();
                        }
                        Screen::Edit => {
                            app.edit_touched = Instant::now();
                            if let Some(b) = button_at(p) {
                                press(&mut app, b);
                                if b.repeats() {
                                    held = Some(b);
                                    repeat_at = Instant::now() + REPEAT_DELAY;
                                }
                            }
                        }
                    }
                    redraw = true;
                } else if let Some(b) = held
                    && Instant::now() >= repeat_at
                {
                    if button_at(p) == Some(b) {
                        press(&mut app, b);
                        app.edit_touched = Instant::now();
                        repeat_at = Instant::now() + REPEAT_RATE;
                        redraw = true;
                    } else {
                        held = None;
                    }
                }
            }
            Either3::Second(event) => {
                bright_until = Instant::now() + WAKE_BRIGHT;
                let ringing = app.screen == Screen::Ringing;
                match event {
                    TouchEvent::Tap(Pad::Back) if ringing => stop_alarm(&mut app, "stopped"),
                    TouchEvent::Tap(Pad::Front) if ringing => {
                        let until = clock().unix() + SNOOZE_SECS;
                        stop_alarm(&mut app, "snoozed for 5 minutes");
                        app.snooze_until = Some(until);
                    }
                    TouchEvent::Tap(Pad::Back) => {
                        app.alarm_on = !app.alarm_on;
                        app.snooze_until = None;
                        info!("{}", app.status());
                    }
                    TouchEvent::Tap(Pad::Front) => {
                        if radio::is_on() {
                            info!("radio off");
                            radio::stop();
                        } else {
                            info!("radio on");
                            radio::play(MANUAL_RAMP);
                        }
                    }
                    TouchEvent::Long(Pad::Front) => {
                        let now = clock().local();
                        app.alarm = (now.minute_of_day() + 1) % (24 * 60);
                        app.alarm_on = true;
                        app.snooze_until = None;
                        info!("test: {}", app.status());
                    }
                    TouchEvent::Long(Pad::Back) => {
                        app.volume = (app.volume + 1) % VOLUME_DB.len();
                        match codec.set_dac_volume(volume_of(app.volume)).await {
                            Ok(()) => info!("volume {} dB", VOLUME_DB[app.volume]),
                            Err(e) => warn!("volume change failed: {e:?}"),
                        }
                    }
                    _ => continue,
                }
                redraw = true;
            }
            Either3::Third(()) => {
                let now = clock().unix();
                let t = clock().local();
                tick(&mut app, now, &t);
                if app.screen == Screen::Edit && app.edit_touched.elapsed() > EDIT_TIMEOUT {
                    info!("editor closed: {}", app.status());
                    app.screen = Screen::Clock;
                }
                if radio::is_on() && now.is_multiple_of(STATS_PERIOD) {
                    let (kbps, cpu, frames) = stats.since();
                    stats = Stats::now();
                    info!(
                        "{} {} | {kbps} kbit/s, buffer {} KiB, {frames} frames, decode {cpu}% CPU, \
                         gain -{:.1} dB, underruns {}",
                        t.weekday_name(),
                        t.date,
                        radio::buffered() / 1024,
                        radio::stats::ATTENUATION_DB10.load(Ordering::Relaxed) as f32 / 10.0,
                        radio::stats::UNDERRUNS.load(Ordering::Relaxed),
                    );
                }
                redraw = true;
            }
        }

        // Dim at night, unless something is happening.
        let night = !(7 * 60..22 * 60).contains(&clock().local().minute_of_day());
        let want = if night && app.screen == Screen::Clock && Instant::now() > bright_until {
            BRIGHT_NIGHT
        } else {
            BRIGHT_DAY
        };
        if want != brightness {
            backlight.fade(brightness, want, Duration::from_millis(500));
            brightness = want;
        }
    }
}

/// Once a second: starts, supervises and ends the alarm.
fn tick(app: &mut App, now: u64, t: &LocalTime) {
    let minute = now / 60;
    if app.screen != Screen::Ringing {
        let due =
            app.alarm_on && t.minute_of_day() == app.alarm && app.fired_minute != Some(minute);
        let snooze_due = app.snooze_until.is_some_and(|s| now >= s);
        if due || snooze_due {
            info!("ALARM {} {}", t.weekday_name(), t.date);
            app.fired_minute = Some(minute);
            app.snooze_until = None;
            app.screen = Screen::Ringing;
            app.ring_start = now;
            app.last_audio = now;
            app.last_frames = radio::stats::FRAMES.load(Ordering::Relaxed);
            radio::play(ALARM_RAMP);
        }
        return;
    }

    let frames = radio::stats::FRAMES.load(Ordering::Relaxed);
    if frames != app.last_frames {
        app.last_frames = frames;
        app.last_audio = now;
    }
    let silent_for = now - app.last_audio;
    let beep = if silent_for > STREAM_GRACE_SECS {
        // Rise from 20 to 100 % over 30 s from when the beeping starts.
        (20 + (silent_for - STREAM_GRACE_SECS) * 80 / 30).min(100) as u8
    } else {
        0
    };
    if (beep > 0) != (BEEP_GAIN.load(Ordering::Relaxed) > 0) {
        if beep > 0 {
            warn!("no audio from the stream, beeping instead");
        } else {
            info!("stream is playing, beeper off");
        }
    }
    BEEP_GAIN.store(beep, Ordering::Relaxed);
    if now - app.ring_start > RING_LIMIT_SECS {
        stop_alarm(app, "gave up after 30 minutes");
    }
}

fn stop_alarm(app: &mut App, why: &str) {
    info!("alarm {why}");
    radio::stop();
    BEEP_GAIN.store(0, Ordering::Relaxed);
    app.screen = Screen::Clock;
}

fn press(app: &mut App, b: Button) {
    let (h, m) = (app.alarm / 60, app.alarm % 60);
    match b {
        Button::HourUp => app.alarm = (h + 1) % 24 * 60 + m,
        Button::HourDown => app.alarm = (h + 23) % 24 * 60 + m,
        Button::MinuteUp => app.alarm = h * 60 + (m + 1) % 60,
        Button::MinuteDown => app.alarm = h * 60 + (m + 59) % 60,
        Button::Toggle => {
            app.alarm_on = !app.alarm_on;
            info!("{}", app.status());
        }
        Button::Done => {
            app.screen = Screen::Clock;
            info!("{}", app.status());
        }
    }
    app.snooze_until = None;
}

// ------------------------------------------------------------------ drawing

const CENTER: Point = Point::new(180, 180);
const BG: Rgb565 = Rgb565::BLACK;
const DIGIT_ON: Rgb565 = Rgb565::new(10, 50, 31);
const DIGIT_RING: Rgb565 = Rgb565::CSS_ORANGE;
const DIGIT_EDIT: Rgb565 = Rgb565::CSS_GOLD;
// This panel lifts dark colours a lot, so the dark ones are very dark.
const DIGIT_OFF: Rgb565 = Rgb565::new(1, 2, 2);
const TEXT: Rgb565 = Rgb565::new(24, 48, 24);
const DIM: Rgb565 = Rgb565::new(12, 24, 12);
const BUTTON: Rgb565 = Rgb565::new(1, 2, 2);
const BUTTON_EDGE: Rgb565 = Rgb565::new(10, 24, 16);

/// Digit geometry: width, height, segment thickness, top edge.
const DW: i32 = 54;
const DH: i32 = 96;
const DT: i32 = 11;
const DY: i32 = 132;
/// Left edges of the four digits; the colon is at x = 180.
const DX: [i32; 4] = [43, 107, 199, 263];

/// Segments a..g (bit 0..6) of each digit.
const SEGMENTS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

#[allow(
    clippy::large_stack_frames,
    reason = "inlined frame buffer fills; not recursive, called from main only"
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
        let color = if SEGMENTS[usize::from(digit)] & (1 << i) != 0 {
            on
        } else {
            DIGIT_OFF
        };
        // One pixel of gap around each segment.
        let _ = fb.fill_solid(
            &Rectangle::new(
                Point::new(rx + 1, ry + 1),
                Size::new(rw as u32 - 2, rh as u32 - 2),
            ),
            color,
        );
    }
}

fn draw_time(fb: &mut FrameBuffer, minute_of_day: u16, color: Rgb565, colon: bool) {
    let (h, m) = ((minute_of_day / 60) as u8, (minute_of_day % 60) as u8);
    for (x, d) in DX.into_iter().zip([h / 10, h % 10, m / 10, m % 10]) {
        draw_digit(fb, x, d, color);
    }
    let dot = if colon { color } else { DIGIT_OFF };
    for dy in [DH / 3, DH * 2 / 3] {
        let _ = Rectangle::with_center(Point::new(180, DY + dy), Size::new(DT as u32, DT as u32))
            .into_styled(PrimitiveStyle::with_fill(dot))
            .draw(fb);
    }
}

/// Text centred on the screen's vertical axis, `y` is the baseline.
fn text(fb: &mut FrameBuffer, s: &str, y: i32, color: Rgb565, large: bool) {
    text_at(fb, s, Point::new(CENTER.x, y), color, large);
}

/// Text centred on `at.x`, with its baseline at `at.y`.
fn text_at(fb: &mut FrameBuffer, s: &str, at: Point, color: Rgb565, large: bool) {
    let font = if large { &FONT_10X20 } else { &FONT_9X15 };
    let _ =
        Text::with_alignment(s, at, MonoTextStyle::new(font, color), Alignment::Center).draw(fb);
}

/// The 60 second dots around the edge; dots up to `lit` highlighted.
fn draw_ring(fb: &mut FrameBuffer, lit: u8, color: Rgb565) {
    for i in 0..60u8 {
        let angle = (f32::from(i) * 6.0 - 90.0).to_radians();
        let p = Point::new(
            CENTER.x + (170.0 * libm::cosf(angle)) as i32,
            CENTER.y + (170.0 * libm::sinf(angle)) as i32,
        );
        let size = if i % 5 == 0 { 8 } else { 5 };
        let c = if i <= lit { color } else { DIGIT_OFF };
        let _ = Circle::with_center(p, size)
            .into_styled(PrimitiveStyle::with_fill(c))
            .draw(fb);
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn draw(fb: &mut FrameBuffer, app: &App, t: &LocalTime) {
    let _ = fb.clear(BG);
    let d = t.date;
    let colon = d.second.is_multiple_of(2);
    match app.screen {
        Screen::Clock => {
            draw_ring(fb, d.second, DIGIT_ON);
            let date = format!(
                "{} {} {}",
                t.weekday_name(),
                d.day,
                MONTHS[usize::from(d.month - 1)]
            );
            text(fb, &date, 95, TEXT, true);
            draw_time(fb, t.minute_of_day(), DIGIT_ON, colon);
            let alarm_color = if app.alarm_on || app.snooze_until.is_some() {
                DIGIT_EDIT
            } else {
                DIM
            };
            text(fb, &app.status(), 268, alarm_color, true);
            let radio = if radio::is_on() {
                format!("radio on, vol {} dB", VOLUME_DB[app.volume])
            } else {
                format!("vol {} dB", VOLUME_DB[app.volume])
            };
            text(fb, &radio, 298, DIM, false);
        }
        Screen::Edit => {
            text(fb, "ALARM", 50, TEXT, true);
            draw_time(fb, app.alarm, DIGIT_EDIT, true);
            for b in BUTTONS {
                draw_button(fb, b, app.alarm_on);
            }
        }
        Screen::Ringing => {
            draw_ring(fb, 59, if colon { DIGIT_RING } else { DIGIT_OFF });
            text(fb, "WAKE UP", 95, DIGIT_RING, true);
            draw_time(fb, t.minute_of_day(), DIGIT_RING, true);
            text(fb, "touch / back pad: stop", 268, TEXT, false);
            text(fb, "front pad: snooze 5 min", 290, TEXT, false);
        }
    }
}

/// Where each editor button is.
fn button_rect(b: Button) -> Rectangle {
    let key = |cx: i32, cy: i32| Rectangle::with_center(Point::new(cx, cy), Size::new(52, 44));
    match b {
        Button::HourUp => key(102, 100),
        Button::HourDown => key(102, 258),
        Button::MinuteUp => key(258, 100),
        Button::MinuteDown => key(258, 258),
        Button::Toggle => Rectangle::new(Point::new(88, 288), Size::new(88, 36)),
        Button::Done => Rectangle::new(Point::new(184, 288), Size::new(88, 36)),
    }
}

fn button_at(p: Point) -> Option<Button> {
    // A button that was hit as drawn wins. Otherwise the targets are 8 px
    // larger (fingers are not precise); where two enlarged targets overlap
    // (Toggle and Done are 8 px apart), the nearer centre wins.
    BUTTONS
        .into_iter()
        .find(|&b| button_rect(b).contains(p))
        .or_else(|| {
            BUTTONS
                .into_iter()
                .filter(|&b| button_rect(b).offset(8).contains(p))
                .min_by_key(|&b| {
                    let d = p - button_rect(b).center();
                    d.x * d.x + d.y * d.y
                })
        })
}

fn draw_button(fb: &mut FrameBuffer, b: Button, alarm_on: bool) {
    let rect = button_rect(b);
    let (label, fill, edge) = match b {
        Button::HourUp | Button::MinuteUp => ("+", BUTTON, BUTTON_EDGE),
        Button::HourDown | Button::MinuteDown => ("-", BUTTON, BUTTON_EDGE),
        Button::Toggle if alarm_on => ("ON", Rgb565::new(0, 6, 1), Rgb565::new(4, 40, 8)),
        Button::Toggle => ("OFF", Rgb565::new(3, 1, 0), Rgb565::new(24, 10, 4)),
        Button::Done => ("OK", BUTTON, BUTTON_EDGE),
    };
    let _ = RoundedRectangle::with_equal_corners(rect, Size::new(12, 12))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .fill_color(fill)
                .stroke_color(edge)
                .stroke_width(2)
                .build(),
        )
        .draw(fb);
    // FONT_10X20: the baseline sits about 6 px below the middle of the capitals.
    let c = rect.center();
    text_at(fb, label, Point::new(c.x, c.y + 6), Rgb565::WHITE, true);
}

fn message(fb: &mut FrameBuffer, s: &str) {
    let _ = fb.clear(BG);
    text(fb, s, 186, TEXT, true);
}

// -------------------------------------------------------------------- tasks

/// Keeps the I2S ring filled from the radio player, with the fallback beeper
/// mixed in.
#[allow(
    clippy::large_stack_frames,
    reason = "the synthesiser's sine table and a mixing buffer, ~1.5 KiB"
)]
#[embassy_executor::task]
async fn audio_task(mut out: AudioOut<'static>, player: &'static mut Player) {
    let mut synth = Synth::new();
    synth.set_wave(Wave::Square);
    loop {
        let result = out
            .fill(|buf| {
                player.render(buf);
                let gain = BEEP_GAIN.load(Ordering::Relaxed);
                if gain == 0 {
                    synth.stop();
                    return;
                }
                synth.set_gain_pct(gain);
                if !synth.is_playing() {
                    synth.play(BEEP);
                }
                let mut beep = [0i16; 256];
                let beep = &mut beep[..buf.len()];
                synth.render(beep);
                for (s, b) in buf.iter_mut().zip(beep.iter()) {
                    *s = s.saturating_add(*b);
                }
            })
            .await;
        match result {
            Ok(()) => {}
            // Something blocked this task for longer than the ring lasts
            // (Wi-Fi start-up does); the stream has been restarted.
            Err(Error::Dma(e)) => warn!("audio ran dry ({e:?}), restarted"),
            Err(e) => halt("audio output failed", e).await,
        }
    }
}

/// Resyncs the clock every few hours.
#[allow(
    clippy::large_stack_frames,
    reason = "the UDP socket buffers live in the task's future"
)]
/// The touch controller's INT pulses (GPIO10), latched.
static TOUCH_IRQ: IrqLatch = IrqLatch::new();

#[embassy_executor::task]
async fn touch_irq_task(pin: Input<'static>) {
    TOUCH_IRQ.watch(pin).await
}

#[allow(
    clippy::large_stack_frames,
    reason = "the SNTP query's UDP socket buffers and DNS future, ~2 KiB"
)]
#[embassy_executor::task]
async fn ntp_task(stack: Stack<'static>) {
    loop {
        Timer::after(NTP_RESYNC).await;
        match net::ntp_time(stack, NTP_SERVER).await {
            Ok(unix) => {
                let drift = unix as i64 - clock().unix() as i64;
                CLOCK.lock(|c| c.set(Some(clock::synced(unix))));
                info!("NTP resync, clock was off by {drift} s");
            }
            Err(e) => warn!("NTP resync failed: {e:?}"),
        }
    }
}

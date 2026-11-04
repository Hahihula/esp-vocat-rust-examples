//! Demo 02: battery and fuel gauge.
//!
//! Reads the BQ27220 fuel gauge every 5 s and tracks whether the battery is
//! charging, discharging or idle. The console disappears while USB is
//! unplugged, so the demo keeps a history of state changes and prints it when
//! charging starts again. The green LED shows the state, and the touch pads
//! print more details:
//!
//! - front tap: full gauge dump
//! - back tap: state change history

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
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_backtrace as _;
use esp_hal::{Async, i2c::master::I2c, timer::timg::TimerGroup};
use log::{info, warn};
use vocat_rust_examples::{
    battery::{Bq27220, DEVICE_NUMBER, Status},
    board::{self, GreenLed},
    touch::{Pad, TouchEvent, TouchInput, TouchPads},
};

esp_bootloader_esp_idf::esp_app_desc!();

/// How often the gauge is read.
const PERIOD: Duration = Duration::from_secs(5);
/// Currents within this band count as idle (full battery, or gauge noise).
const IDLE_BAND_MA: i16 = 15;
/// Number of state changes kept in the history.
const HISTORY: usize = 32;

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

    fn name(self) -> &'static str {
        match self {
            ChargeState::Charging => "charging",
            ChargeState::Discharging => "discharging",
            ChargeState::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Change {
    at_s: u64,
    state: ChargeState,
    voltage_mv: u16,
    soc_pct: u16,
    remaining_mah: u16,
}

/// Ring buffer of the latest state changes.
struct History {
    entries: [Option<Change>; HISTORY],
    next: usize,
}

impl History {
    #[allow(
        clippy::large_stack_frames,
        reason = "~1 KiB of history, created once in main"
    )]
    const fn new() -> Self {
        Self {
            entries: [None; HISTORY],
            next: 0,
        }
    }

    fn push(&mut self, change: Change) {
        self.entries[self.next] = Some(change);
        self.next = (self.next + 1) % HISTORY;
    }

    /// Oldest first.
    fn iter(&self) -> impl Iterator<Item = &Change> {
        let (newer, older) = self.entries.split_at(self.next);
        older.iter().chain(newer).flatten()
    }

    fn print(&self) {
        info!("state history (uptime, state, voltage, charge):");
        for c in self.iter() {
            info!(
                "  {:>6} s  {:<11} {} mV  {} %  {} mAh",
                c.at_s,
                c.state.name(),
                c.voltage_mv,
                c.soc_pct,
                c.remaining_mah
            );
        }
    }
}

static LED_STATE: Signal<CriticalSectionRawMutex, ChargeState> = Signal::new();

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the gauge and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    board::wait_for_console().await;

    info!("=== Demo 02: battery (BQ27220 fuel gauge) ===");
    info!("front tap = full dump, back tap = state history");
    info!("LED: blinking = charging, steady = idle/full, short flash every 3 s = on battery");

    spawner.spawn(led_task(GreenLed::new(peripherals.GPIO43)).unwrap());

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut gauge = Bq27220::new(i2c);
    match (gauge.device_number().await, gauge.firmware_version().await) {
        (Ok(DEVICE_NUMBER), Ok(fw)) => info!("BQ27220 found, firmware {fw:04X}"),
        (Ok(other), _) => warn!("unexpected device number {other:04X}"),
        (Err(e), _) => warn!("fuel gauge did not answer: {e:?}"),
    }

    let pads = TouchPads::new(
        peripherals.SENS,
        peripherals.RTC_IO,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );
    let mut touch = TouchInput::new(pads).await;

    let mut history = History::new();
    let mut state: Option<ChargeState> = None;
    let mut last: Option<Status> = None;
    // Lowest voltage seen since the battery last started discharging.
    let mut min_voltage_mv = u16::MAX;
    let mut discharge_start: Option<Change> = None;
    let mut ticker = Ticker::every(PERIOD);

    loop {
        match select(ticker.next(), touch.next_event()).await {
            Either::First(()) => {}
            Either::Second(TouchEvent::Tap(Pad::Front)) => {
                if let Some(s) = &last {
                    print_full(s);
                }
                continue;
            }
            Either::Second(TouchEvent::Tap(Pad::Back)) => {
                history.print();
                continue;
            }
            Either::Second(_) => continue,
        }

        let s = match read_status(&mut gauge).await {
            Some(s) => s,
            None => continue,
        };
        last = Some(s);
        print_line(&s);

        let new_state = ChargeState::from_current(s.current_ma);
        min_voltage_mv = min_voltage_mv.min(s.voltage_mv);

        if state != Some(new_state) {
            let change = Change {
                at_s: Instant::now().as_secs(),
                state: new_state,
                voltage_mv: s.voltage_mv,
                soc_pct: s.state_of_charge_pct,
                remaining_mah: s.remaining_mah,
            };
            info!(
                "state: {} -> {}",
                state.map_or("start", ChargeState::name),
                new_state.name()
            );

            history.push(change);
            state = Some(new_state);
            LED_STATE.signal(new_state);

            if new_state == ChargeState::Discharging {
                discharge_start = Some(change);
                min_voltage_mv = s.voltage_mv;
            } else if let Some(start) = discharge_start.take() {
                // Back on USB: what happened on battery was not visible on the console.
                info!(
                    "on battery for {} s: {} -> {} mAh ({} mAh used), {} -> {} %, lowest {} mV",
                    change.at_s - start.at_s,
                    start.remaining_mah,
                    s.remaining_mah,
                    start.remaining_mah.saturating_sub(s.remaining_mah),
                    start.soc_pct,
                    s.state_of_charge_pct,
                    min_voltage_mv,
                );
                history.print();
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

fn print_line(s: &Status) {
    let estimate = match (s.time_to_full_min, s.time_to_empty_min) {
        (Some(m), _) => FmtMinutes(" full in", m),
        (None, Some(m)) => FmtMinutes(" empty in", m),
        (None, None) => FmtMinutes("", 0),
    };
    info!(
        "{} mV  {:+} mA (avg {:+})  {} %  {}/{} mAh  {}.{} °C{}",
        s.voltage_mv,
        s.current_ma,
        s.average_current_ma,
        s.state_of_charge_pct,
        s.remaining_mah,
        s.full_charge_mah,
        s.temperature_dc / 10,
        (s.temperature_dc % 10).abs(),
        estimate,
    );
}

/// Formats `prefix h:mm`, or nothing for an empty prefix.
struct FmtMinutes(&'static str, u16);

impl core::fmt::Display for FmtMinutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() {
            return Ok(());
        }
        write!(f, ",{} {}:{:02} h", self.0, self.1 / 60, self.1 % 60)
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "formatting arguments of many log lines; called from main only"
)]
fn print_full(s: &Status) {
    let f = s.flags;
    info!("--- fuel gauge ---");
    info!("voltage          {} mV", s.voltage_mv);
    info!(
        "current          {:+} mA (average {:+} mA)",
        s.current_ma, s.average_current_ma
    );
    info!("average power    {:+} mW", s.average_power_mw);
    info!("state of charge  {} %", s.state_of_charge_pct);
    info!("state of health  {} %", s.state_of_health_pct);
    info!(
        "capacity         {} mAh remaining, {} mAh full, {} mAh design",
        s.remaining_mah, s.full_charge_mah, s.design_mah
    );
    info!(
        "temperature      {}.{} °C",
        s.temperature_dc / 10,
        (s.temperature_dc % 10).abs()
    );
    info!("time to empty    {}", OptMinutes(s.time_to_empty_min));
    info!("time to full     {}", OptMinutes(s.time_to_full_min));
    info!("cycle count      {}", s.cycle_count);
    info!(
        "flags {:04X}:     {}{}{}{}{}{}{}{}",
        f.0,
        if f.battery_present() {
            "present "
        } else {
            "NOT-PRESENT "
        },
        if f.discharging() { "discharging " } else { "" },
        if f.full_charge() { "full " } else { "" },
        if f.fully_discharged() { "empty " } else { "" },
        if f.system_down() { "system-down " } else { "" },
        if f.charge_inhibit() {
            "charge-inhibit "
        } else {
            ""
        },
        if f.over_temp_charge() {
            "over-temp-charge "
        } else {
            ""
        },
        if f.over_temp_discharge() {
            "over-temp-discharge "
        } else {
            ""
        },
    );
}

struct OptMinutes(Option<u16>);

impl core::fmt::Display for OptMinutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(m) => write!(f, "{}:{:02} h", m / 60, m % 60),
            None => f.write_str("-"),
        }
    }
}

#[embassy_executor::task]
async fn led_task(mut led: GreenLed<'static>) {
    let mut state = ChargeState::Idle;
    loop {
        let steps: &[(bool, u64)] = match state {
            ChargeState::Charging => &[(true, 500), (false, 500)],
            ChargeState::Idle => &[(true, 1000)],
            ChargeState::Discharging => &[(true, 30), (false, 2970)],
        };
        let play = async {
            loop {
                for &(on, ms) in steps {
                    led.set(on);
                    Timer::after(Duration::from_millis(ms)).await;
                }
            }
        };
        state = match select(LED_STATE.wait(), play).await {
            Either::First(s) => s,
        };
    }
}

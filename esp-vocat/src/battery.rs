//! Driver for the BQ27220 battery fuel gauge (I2C address 0x55).
//!
//! The gauge sits on the base board between the 700 mAh cell and the rest of
//! the system (10 mΩ sense resistor). It measures voltage, current and
//! temperature and runs TI's CEDV algorithm for charge, health and the time
//! estimates. All standard commands are 16-bit little-endian registers.
//!
//! The TP4057 charger's status pins only drive the charge LED, so the gauge is
//! the only way to see whether the battery is charging.

use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

use crate::board::i2c_addr;

/// Standard command codes (register addresses).
mod cmd {
    pub const CONTROL: u8 = 0x00;
    pub const TEMPERATURE: u8 = 0x06;
    pub const VOLTAGE: u8 = 0x08;
    pub const BATTERY_STATUS: u8 = 0x0A;
    pub const CURRENT: u8 = 0x0C;
    pub const REMAINING_CAPACITY: u8 = 0x10;
    pub const FULL_CHARGE_CAPACITY: u8 = 0x12;
    pub const AVERAGE_CURRENT: u8 = 0x14;
    pub const TIME_TO_EMPTY: u8 = 0x16;
    pub const TIME_TO_FULL: u8 = 0x18;
    pub const AVERAGE_POWER: u8 = 0x24;
    pub const CYCLE_COUNT: u8 = 0x2A;
    pub const STATE_OF_CHARGE: u8 = 0x2C;
    pub const STATE_OF_HEALTH: u8 = 0x2E;
    pub const DESIGN_CAPACITY: u8 = 0x3C;
    pub const MAC_DATA: u8 = 0x40;
}

/// `Control()` subcommands.
mod sub {
    pub const DEVICE_NUMBER: u16 = 0x0001;
    pub const FW_VERSION: u16 = 0x0002;
}

/// Expected answer to `DEVICE_NUMBER`.
pub const DEVICE_NUMBER: u16 = 0x0220;

/// `BatteryStatus()` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFlags(pub u16);

impl StatusFlags {
    /// Discharging (no charge current).
    pub fn discharging(self) -> bool {
        self.0 & (1 << 0) != 0
    }
    /// System shutdown voltage reached.
    pub fn system_down(self) -> bool {
        self.0 & (1 << 1) != 0
    }
    /// Battery detected.
    pub fn battery_present(self) -> bool {
        self.0 & (1 << 3) != 0
    }
    /// Charging inhibited (temperature out of range).
    pub fn charge_inhibit(self) -> bool {
        self.0 & (1 << 8) != 0
    }
    /// Full charge detected.
    pub fn full_charge(self) -> bool {
        self.0 & (1 << 9) != 0
    }
    /// Over-temperature in discharge.
    pub fn over_temp_discharge(self) -> bool {
        self.0 & (1 << 10) != 0
    }
    /// Over-temperature in charge.
    pub fn over_temp_charge(self) -> bool {
        self.0 & (1 << 11) != 0
    }
    /// Fully discharged.
    pub fn fully_discharged(self) -> bool {
        self.0 & (1 << 15) != 0
    }
}

/// A full set of gauge readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub voltage_mv: u16,
    /// Positive while charging, negative while discharging.
    pub current_ma: i16,
    /// Current averaged by the gauge over about a minute.
    pub average_current_ma: i16,
    pub average_power_mw: i16,
    pub state_of_charge_pct: u16,
    pub state_of_health_pct: u16,
    pub remaining_mah: u16,
    pub full_charge_mah: u16,
    pub design_mah: u16,
    /// Temperature in 0.1 °C.
    pub temperature_dc: i16,
    /// Minutes until empty at the average current; `None` while not discharging.
    pub time_to_empty_min: Option<u16>,
    /// Minutes until full at the average current; `None` while not charging.
    pub time_to_full_min: Option<u16>,
    pub cycle_count: u16,
    pub flags: StatusFlags,
}

/// BQ27220 driver over any async I2C bus.
pub struct Bq27220<I> {
    i2c: I,
}

impl<I: I2c> Bq27220<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    async fn read_word(&mut self, command: u8) -> Result<u16, I::Error> {
        let mut buf = [0u8; 2];
        self.i2c
            .write_read(i2c_addr::BQ27220, &[command], &mut buf)
            .await?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Sends a `Control()` subcommand and reads its 16-bit answer from `MACData()`.
    async fn control(&mut self, subcommand: u16) -> Result<u16, I::Error> {
        let [lo, hi] = subcommand.to_le_bytes();
        self.i2c
            .write(i2c_addr::BQ27220, &[cmd::CONTROL, lo, hi])
            .await?;
        Timer::after(Duration::from_millis(2)).await;
        self.read_word(cmd::MAC_DATA).await
    }

    /// Reads the device number; [`DEVICE_NUMBER`] for a BQ27220.
    pub async fn device_number(&mut self) -> Result<u16, I::Error> {
        self.control(sub::DEVICE_NUMBER).await
    }

    /// Reads the firmware version.
    pub async fn firmware_version(&mut self) -> Result<u16, I::Error> {
        self.control(sub::FW_VERSION).await
    }

    pub async fn voltage_mv(&mut self) -> Result<u16, I::Error> {
        self.read_word(cmd::VOLTAGE).await
    }

    pub async fn current_ma(&mut self) -> Result<i16, I::Error> {
        Ok(self.read_word(cmd::CURRENT).await? as i16)
    }

    pub async fn state_of_charge_pct(&mut self) -> Result<u16, I::Error> {
        self.read_word(cmd::STATE_OF_CHARGE).await
    }

    /// Reads all values. Takes about 15 register reads.
    pub async fn status(&mut self) -> Result<Status, I::Error> {
        let minutes = |v: u16| (v != 0xFFFF).then_some(v);
        Ok(Status {
            voltage_mv: self.read_word(cmd::VOLTAGE).await?,
            current_ma: self.read_word(cmd::CURRENT).await? as i16,
            average_current_ma: self.read_word(cmd::AVERAGE_CURRENT).await? as i16,
            average_power_mw: self.read_word(cmd::AVERAGE_POWER).await? as i16,
            state_of_charge_pct: self.read_word(cmd::STATE_OF_CHARGE).await?,
            state_of_health_pct: self.read_word(cmd::STATE_OF_HEALTH).await?,
            remaining_mah: self.read_word(cmd::REMAINING_CAPACITY).await?,
            full_charge_mah: self.read_word(cmd::FULL_CHARGE_CAPACITY).await?,
            design_mah: self.read_word(cmd::DESIGN_CAPACITY).await?,
            // The gauge reports 0.1 K.
            temperature_dc: (i32::from(self.read_word(cmd::TEMPERATURE).await?) - 2731) as i16,
            time_to_empty_min: minutes(self.read_word(cmd::TIME_TO_EMPTY).await?),
            time_to_full_min: minutes(self.read_word(cmd::TIME_TO_FULL).await?),
            cycle_count: self.read_word(cmd::CYCLE_COUNT).await?,
            flags: StatusFlags(self.read_word(cmd::BATTERY_STATUS).await?),
        })
    }
}

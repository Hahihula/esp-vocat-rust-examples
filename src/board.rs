//! Board-level constants and common initialisation for ESP-VoCat v1.0.
//!
//! | Function                  | GPIO                         |
//! |---------------------------|------------------------------|
//! | BOOT button               | 0 (active low)               |
//! | Shared I2C SCL/SDA        | 1 / 2                        |
//! | LCD reset (also touch)    | 3                            |
//! | Speaker PA enable         | 4 (active high)              |
//! | UART1 RX/TX (magnetic)    | 5 / 6                        |
//! | Touch pad                 | 7 (TOUCH7)                   |
//! | LCD tearing effect        | 8                            |
//! | Peripheral power (LCD, SD)| 9 (active low)               |
//! | Touch screen INT          | 10                           |
//! | LCD QSPI D0/D1/D2/D3      | 46 / 13 / 11 / 12            |
//! | LCD QSPI CLK / CS / DC    | 18 / 14 / 45                 |
//! | microSD CLK / CMD / D0    | 16 / 38 / 17 (1-bit SD mode) |
//! | USB D- / D+               | 19 / 20                      |
//! | IMU INT1                  | 21                           |
//! | I2S DI (from ES7210)      | 15                           |
//! | I2S WS / BCK / DO / MCLK  | 39 / 40 / 41 / 42            |
//! | Green LED                 | 43 (U0TXD, active low)       |
//! | LCD backlight             | 44 (U0RXD, active high)      |

use esp_hal::{
    Async,
    clock::CpuClock,
    gpio::{Level, Output, OutputConfig, OutputPin},
    i2c::master::{Config as I2cConfig, ConfigError, I2c, Instance as I2cInstance},
    peripherals::{GPIO1, GPIO2, GPIO9, Peripherals},
    time::Rate,
};

/// 7-bit addresses of the devices on the shared I2C bus (GPIO1/GPIO2).
pub mod i2c_addr {
    /// ES8311 mono codec (speaker DAC), behind a level shifter.
    pub const ES8311: u8 = 0x18;
    /// CST816S capacitive touch controller on the LCD flex.
    /// Only answers while awake; it sleeps a few seconds after reset.
    pub const CST816S: u8 = 0x15;
    /// ES7210 4-channel microphone ADC, behind a level shifter.
    pub const ES7210: u8 = 0x40;
    /// BQ27220 battery fuel gauge on the base board.
    pub const BQ27220: u8 = 0x55;
    /// BMI270 6-axis IMU (SDO tied low).
    pub const BMI270: u8 = 0x68;
}

/// Initialises logging, the HAL with maximum CPU clock and a small internal heap.
///
/// The caller still has to start `esp_rtos` with a `TIMG0` timer.
pub fn init() -> Peripherals {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    peripherals
}

/// Creates the async I2C master for the shared bus (codecs, IMU, fuel gauge, touch).
///
/// The bus has 2.2 kΩ pull-ups on board.
pub fn i2c<'d>(
    i2c: impl I2cInstance + 'd,
    scl: GPIO1<'d>,
    sda: GPIO2<'d>,
) -> Result<I2c<'d, Async>, ConfigError> {
    let config = I2cConfig::default().with_frequency(Rate::from_khz(400));
    Ok(I2c::new(i2c, config)?
        .with_scl(scl)
        .with_sda(sda)
        .into_async())
}

/// Switches on `VCC_3V3`, which powers the LCD (with its touch controller) and
/// the microSD slot. GPIO9 drives a P-MOSFET gate that is pulled up, so the
/// rail is off until this is called. Keep the returned pin alive.
pub fn peripheral_power_on<'d>(pin: GPIO9<'d>) -> Output<'d> {
    Output::new(pin, Level::Low, OutputConfig::default())
}

/// The green LED on the MicBoard (GPIO43, active low).
pub struct GreenLed<'d>(Output<'d>);

impl<'d> GreenLed<'d> {
    /// Creates the LED driver, initially off.
    pub fn new(pin: impl OutputPin + 'd) -> Self {
        Self(Output::new(pin, Level::High, OutputConfig::default()))
    }

    /// Turns the LED on or off.
    pub fn set(&mut self, on: bool) {
        self.0.set_level(if on { Level::Low } else { Level::High });
    }

    /// Inverts the LED state.
    pub fn toggle(&mut self) {
        self.0.toggle();
    }
}

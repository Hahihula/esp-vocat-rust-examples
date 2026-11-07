//! Board-level constants and common initialisation for ESP-VoCat v1.2.
//!
//! | Function                  | GPIO                         |
//! |---------------------------|------------------------------|
//! | BOOT button               | 0 (active low)               |
//! | Shared I2C SCL/SDA        | 1 / 2                        |
//! | I2S DI (from ES7210)      | 3                            |
//! | UART1 RX/TX (magnetic)    | 4 / 5                        |
//! | Touch pads 1 / 2          | 6 / 7 (TOUCH6 / TOUCH7)      |
//! | LCD tearing effect        | 8                            |
//! | Peripheral power (LCD, SD)| 9 (active low)               |
//! | Touch screen INT          | 10                           |
//! | LCD QSPI D0/D1/D2/D3      | 46 / 13 / 11 / 12            |
//! | LCD QSPI CLK / CS / DC    | 18 / 14 / 45                 |
//! | Speaker PA enable         | 15 (active high)             |
//! | microSD CLK / CMD / D0    | 16 / 38 / 17 (1-bit SD mode) |
//! | USB D- / D+               | 19 / 20                      |
//! | IMU INT1                  | 21                           |
//! | I2S WS / BCK / DO / MCLK  | 39 / 40 / 41 / 42            |
//! | Green LED                 | 43 (U0TXD, active low)       |
//! | LCD backlight             | 44 (U0RXD, active high)      |
//! | LCD/touch reset           | 47 (high = reset, via FET)   |
//! | Codec power (ES8311/7210) | 48 (active high)             |

use esp_hal::{
    Async,
    clock::CpuClock,
    gpio::{Level, Output, OutputConfig, OutputPin},
    i2c::master::{Config as I2cConfig, ConfigError, I2c, Instance as I2cInstance},
    peripherals::{GPIO1, GPIO2, GPIO9, GPIO47, GPIO48, PSRAM, Peripherals},
    psram::{Psram, PsramConfig, PsramMode, SpiRamFreq},
    time::Rate,
};

/// 7-bit addresses of the devices on the shared I2C bus (GPIO1/GPIO2).
pub mod i2c_addr {
    /// ES8311 mono codec (speaker DAC), behind a level shifter.
    /// Needs codec power ([`codec_power_on`](super::codec_power_on)).
    pub const ES8311: u8 = 0x18;
    /// CST816T capacitive touch controller on the LCD flex.
    /// Only answers while awake; it sleeps a few seconds after reset.
    pub const CST816: u8 = 0x15;
    /// ES7210 4-channel microphone ADC, behind a level shifter.
    /// Needs codec power ([`codec_power_on`](super::codec_power_on)).
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

/// Initialises the 16 MB octal PSRAM at 80 MHz (the esp-hal default is 40 MHz).
pub fn psram(psram: PSRAM<'static>) -> Psram {
    let config = PsramConfig {
        mode: PsramMode::OctalSpi,
        ram_frequency: SpiRamFreq::Freq80m,
        ..Default::default()
    };
    Psram::new(psram, config)
}

/// Initialises PSRAM with [`psram`] and adds all of it to the heap.
/// Allocations that don't fit into internal RAM then come from PSRAM.
/// Call at most once. Returns the PSRAM size in bytes.
pub fn psram_heap(psram: PSRAM<'static>) -> usize {
    let psram = self::psram(psram);
    let (_, size) = psram.raw_parts();
    esp_alloc::psram_allocator!(&psram);
    size
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

/// Waits two seconds so a serial monitor started right after reset catches the
/// first log lines. The USB-Serial-JTAG console re-enumerates on every chip
/// reset, and anything printed before the host reconnects is lost.
pub async fn wait_for_console() {
    embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
}

/// Switches on `VCC_3V3`, which powers the LCD (with its touch controller) and
/// the microSD slot. GPIO9 drives a P-MOSFET gate that is pulled up, so the
/// rail is off until this is called. Keep the returned pin alive.
pub fn peripheral_power_on<'d>(pin: GPIO9<'d>) -> Output<'d> {
    Output::new(pin, Level::Low, OutputConfig::default())
}

/// Switches on `CODEC_3V3` (GPIO48 drives the enable of the SY8088 buck
/// converter), which powers the ES8311 and ES7210 codecs. Keep the returned
/// pin alive. The codecs need a few milliseconds before they answer on I2C.
pub fn codec_power_on<'d>(pin: GPIO48<'d>) -> Output<'d> {
    Output::new(pin, Level::High, OutputConfig::default())
}

/// Shared reset of the LCD and its touch controller.
///
/// GPIO47 drives an N-MOSFET that pulls `LCD_RST` low, so a high level holds
/// the LCD in reset. The gate has a pull-down, so reset is released by default.
pub struct LcdReset<'d>(Output<'d>);

impl<'d> LcdReset<'d> {
    /// Creates the reset driver with reset released.
    pub fn new(pin: GPIO47<'d>) -> Self {
        Self(Output::new(pin, Level::Low, OutputConfig::default()))
    }

    /// Asserts or releases the reset.
    pub fn set_reset(&mut self, asserted: bool) {
        self.0
            .set_level(if asserted { Level::High } else { Level::Low });
    }
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

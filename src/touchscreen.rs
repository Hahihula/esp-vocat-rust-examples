//! CST816T capacitive touch controller of the round LCD (I2C 0x15, INT GPIO10).
//!
//! The controller shares its reset line with the LCD (GPIO47), so it is reset
//! by [`Lcd::new`](crate::display::Lcd::new). It goes to sleep a few seconds
//! after reset unless auto-sleep is disabled; asleep it doesn't answer on I2C
//! until the screen is touched. [`Cst816::init`] disables auto-sleep, so call
//! it soon after the reset.
//!
//! Register map (from the CST816 family datasheets):
//!
//! | Register | Contents |
//! |----------|----------|
//! | 0x01 | gesture ID |
//! | 0x02 | number of fingers (0 or 1) |
//! | 0x03/0x04 | event (bits 7:6 of 0x03) and X (12 bits) |
//! | 0x05/0x06 | Y (12 bits) |
//! | 0xA7 | chip ID (B5 = CST816T) |
//! | 0xA9 | firmware version |
//! | 0xEC | motion mask (bit 0: double click) |
//! | 0xFA | interrupt control |
//! | 0xFE | auto-sleep disable |

use embedded_hal_async::i2c::I2c;

use crate::board::i2c_addr;

mod reg {
    pub const GESTURE: u8 = 0x01;
    pub const CHIP_ID: u8 = 0xA7;
    pub const FW_VERSION: u8 = 0xA9;
    pub const MOTION_MASK: u8 = 0xEC;
    pub const IRQ_CTL: u8 = 0xFA;
    pub const DIS_AUTO_SLEEP: u8 = 0xFE;
}

/// `IRQ_CTL`: pulse INT on touch and on every change of the touch position.
const IRQ_EN_TOUCH_CHANGE: u8 = 0x60;
/// `MOTION_MASK`: also report double clicks.
const MOTION_DOUBLE_CLICK: u8 = 0x01;

/// A gesture recognised by the controller itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    None,
    SwipeUp,
    SwipeDown,
    SwipeLeft,
    SwipeRight,
    Click,
    DoubleClick,
    LongPress,
    Other(u8),
}

impl From<u8> for Gesture {
    fn from(id: u8) -> Self {
        match id {
            0x00 => Gesture::None,
            0x01 => Gesture::SwipeUp,
            0x02 => Gesture::SwipeDown,
            0x03 => Gesture::SwipeLeft,
            0x04 => Gesture::SwipeRight,
            0x05 => Gesture::Click,
            0x0B => Gesture::DoubleClick,
            0x0C => Gesture::LongPress,
            other => Gesture::Other(other),
        }
    }
}

/// One touch report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub gesture: Gesture,
    /// Whether a finger is on the screen.
    pub touched: bool,
    /// Event code from bits 7:6 of register 0x03 (0 down, 1 up, 2 contact).
    pub event: u8,
    pub x: u16,
    pub y: u16,
}

/// CST816T driver over any async I2C bus.
pub struct Cst816<I> {
    i2c: I,
}

impl<I: I2c> Cst816<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    async fn read_u8(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut b = [0u8];
        self.i2c
            .write_read(i2c_addr::CST816, &[reg], &mut b)
            .await?;
        Ok(b[0])
    }

    async fn write_u8(&mut self, reg: u8, value: u8) -> Result<(), I::Error> {
        self.i2c.write(i2c_addr::CST816, &[reg, value]).await
    }

    /// Disables auto-sleep and enables interrupts on touch and movement.
    /// Returns the chip ID and firmware version.
    pub async fn init(&mut self) -> Result<(u8, u8), I::Error> {
        let id = self.read_u8(reg::CHIP_ID).await?;
        let fw = self.read_u8(reg::FW_VERSION).await?;
        self.write_u8(reg::DIS_AUTO_SLEEP, 0x01).await?;
        self.write_u8(reg::IRQ_CTL, IRQ_EN_TOUCH_CHANGE).await?;
        self.write_u8(reg::MOTION_MASK, MOTION_DOUBLE_CLICK).await?;
        Ok((id, fw))
    }

    /// Reads the current touch state and gesture.
    pub async fn read(&mut self) -> Result<Report, I::Error> {
        let mut b = [0u8; 6];
        self.i2c
            .write_read(i2c_addr::CST816, &[reg::GESTURE], &mut b)
            .await?;
        Ok(Report {
            gesture: Gesture::from(b[0]),
            touched: b[1] > 0,
            event: b[2] >> 6,
            x: u16::from(b[2] & 0x0F) << 8 | u16::from(b[3]),
            y: u16::from(b[4] & 0x0F) << 8 | u16::from(b[5]),
        })
    }
}

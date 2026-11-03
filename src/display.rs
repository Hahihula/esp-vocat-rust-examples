//! 1.85" 360×360 round LCD with an ST77916 controller on QSPI.
//!
//! Wiring: CLK GPIO18, D0..D3 GPIO46/13/11/12, CS GPIO14, DC GPIO45, reset
//! GPIO47 (through a transistor, see [`LcdReset`]), backlight GPIO44 (PWM).
//! The LCD is powered from `VCC_3V3` ([`board::peripheral_power_on`]).
//!
//! Every transfer is framed like Espressif's `esp_lcd` QSPI driver does it,
//! with CS held low throughout:
//!
//! 1. a 32-bit header on one data line with DC low: `0x02 0x00 cmd 0x00` for a
//!    command, `0x32 0x00 0x2C 0x00` (RAMWR) for pixel data,
//! 2. the command parameters on one line, or the pixels on all four lines,
//!    with DC high. Pixels are RGB565, high byte first.
//!
//! The whole frame lives in a [`FrameBuffer`] in PSRAM. [`Lcd::flush`] sends
//! the rows that changed since the last flush, copied in chunks of up to 32 KB
//! into internal RAM and sent from there with DMA.
//!
//! [`board::peripheral_power_on`]: crate::board::peripheral_power_on

use alloc::{boxed::Box, vec};

use embassy_time::{Duration, Timer};
use embedded_graphics::{
    Pixel,
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions, Size},
    pixelcolor::{IntoStorage, Rgb565},
    primitives::{PointsIter, Rectangle},
};
use esp_hal::{
    Async,
    dma_rx_buffer, dma_tx_buffer,
    gpio::{Level, Output, OutputConfig},
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    peripherals::{
        DMA_CH0, GPIO11, GPIO12, GPIO13, GPIO14, GPIO18, GPIO44, GPIO45, GPIO46, LEDC, SPI2,
    },
    spi::{
        Mode,
        master::{Address, Command, Config, DataMode, Spi, SpiDma},
    },
    time::Rate,
};
use static_cell::{ConstStaticCell, StaticCell};

use crate::{board::LcdReset, st77916_init::INIT_SEQUENCE};

/// Panel width in pixels.
pub const WIDTH: u16 = 360;
/// Panel height in pixels.
pub const HEIGHT: u16 = 360;
/// Bytes per frame (RGB565).
pub const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 2;
const ROW_BYTES: usize = WIDTH as usize * 2;

/// Bytes per DMA transfer. The SPI peripheral handles at most 32 KiB.
const CHUNK: usize = 32_000;
/// Size of the DMA copy buffer esp-hal uses for data outside internal RAM
/// (command parameters from flash). Pixels go through [`Lcd::bounce`].
const COPY_BUF: usize = 256;

/// SPI clock. The LCD pins go through the GPIO matrix, not IO_MUX: at 80 MHz
/// the picture is garbage, so this is the next step down from the 80 MHz
/// source (integer dividers only).
pub const SPI_CLOCK: Rate = Rate::from_mhz(40);

mod cmd {
    pub const CASET: u8 = 0x2A;
    pub const RASET: u8 = 0x2B;
    pub const RAMWR: u8 = 0x2C;
    pub const MADCTL: u8 = 0x36;
    pub const COLMOD: u8 = 0x3A;
    pub const INVON: u8 = 0x21;
    pub const DISPON: u8 = 0x29;
    pub const DISPOFF: u8 = 0x28;
}

const OPCODE_WRITE_CMD: u8 = 0x02;
const OPCODE_WRITE_COLOR: u8 = 0x32;

/// Errors from the LCD driver.
#[derive(Debug)]
pub enum Error {
    Spi(esp_hal::spi::Error),
    Config(esp_hal::spi::master::ConfigError),
}

impl From<esp_hal::spi::Error> for Error {
    fn from(e: esp_hal::spi::Error) -> Self {
        Error::Spi(e)
    }
}

/// The pins of the LCD bus.
pub struct LcdPins {
    pub clk: GPIO18<'static>,
    pub d0: GPIO46<'static>,
    pub d1: GPIO13<'static>,
    pub d2: GPIO11<'static>,
    pub d3: GPIO12<'static>,
    pub cs: GPIO14<'static>,
    pub dc: GPIO45<'static>,
}

/// The ST77916 panel.
pub struct Lcd {
    spi: SpiDma<'static, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    /// Pixel chunks are copied here (internal RAM) before each DMA transfer.
    ///
    /// DMA straight out of the PSRAM frame buffer showed stale data: rows
    /// that are redrawn every frame appeared with content from an earlier
    /// program. The CPU copy reads through the cache, so it always sees the
    /// current pixels.
    bounce: &'static mut [u8; CHUNK],
}

impl Lcd {
    /// Resets and initialises the panel and turns it on (showing whatever is
    /// in its memory, so send a frame before switching on the backlight).
    pub async fn new(
        spi2: SPI2<'static>,
        dma: DMA_CH0<'static>,
        pins: LcdPins,
        reset: &mut LcdReset<'_>,
    ) -> Result<Self, Error> {
        // The RX buffer is unused (half-duplex writes only) but required.
        let rx = dma_rx_buffer!(32).expect("valid DMA RX buffer");
        let tx = dma_tx_buffer!(COPY_BUF).expect("valid DMA TX buffer");
        // Initialised statically, so the 32 KB never pass through the stack.
        static BOUNCE: ConstStaticCell<[u8; CHUNK]> = ConstStaticCell::new([0; CHUNK]);
        let bounce = BOUNCE.take();

        let config = Config::default()
            .with_frequency(SPI_CLOCK)
            .with_mode(Mode::_0);
        let spi = Spi::new(spi2, config)
            .map_err(Error::Config)?
            .with_sck(pins.clk)
            .with_sio0(pins.d0)
            .with_sio1(pins.d1)
            .with_sio2(pins.d2)
            .with_sio3(pins.d3)
            .with_dma(dma)
            .with_buffers(rx, tx)
            .into_async();

        let mut lcd = Self {
            spi,
            cs: Output::new(pins.cs, Level::High, OutputConfig::default()),
            dc: Output::new(pins.dc, Level::Low, OutputConfig::default()),
            bounce,
        };

        reset.set_reset(true);
        Timer::after(Duration::from_millis(10)).await;
        reset.set_reset(false);
        Timer::after(Duration::from_millis(120)).await;

        // RGB order, 16 bits per pixel, then the vendor sequence (which ends
        // with INVON and SLPOUT + 120 ms), as esp_lcd_st77916 does.
        lcd.command(cmd::MADCTL, &[0x00]).await?;
        lcd.command(cmd::COLMOD, &[0x55]).await?;
        for &(c, params, delay_ms) in INIT_SEQUENCE {
            lcd.command(c, params).await?;
            if delay_ms > 0 {
                Timer::after(Duration::from_millis(delay_ms.into())).await;
            }
        }
        // The BSP inverts colours once more after the init sequence.
        lcd.command(cmd::INVON, &[]).await?;
        lcd.command(cmd::DISPON, &[]).await?;
        Ok(lcd)
    }

    /// Switches the panel output on or off.
    pub async fn set_on(&mut self, on: bool) -> Result<(), Error> {
        self.command(if on { cmd::DISPON } else { cmd::DISPOFF }, &[])
            .await
    }

    /// Sends a command with its parameters.
    pub async fn command(&mut self, command: u8, params: &[u8]) -> Result<(), Error> {
        let _cs = CsGuard::select(&mut self.cs);
        header(&mut self.spi, &mut self.dc, OPCODE_WRITE_CMD, command).await?;
        if !params.is_empty() {
            self.spi
                .half_duplex_write_async(DataMode::Single, Command::None, Address::None, 0, params)
                .await?;
        }
        Ok(())
    }

    /// Writes `pixels` (RGB565, high byte first) to the window starting at
    /// (x0, y0) with the given width; the height follows from the length.
    ///
    /// Safe to cancel: if the future is dropped mid-transfer, CS is released,
    /// which ends the write on the panel side, and the next call starts clean.
    pub async fn write_window(
        &mut self,
        x0: u16,
        y0: u16,
        width: u16,
        pixels: &[u8],
    ) -> Result<(), Error> {
        let height = (pixels.len() / (usize::from(width) * 2)) as u16;
        if height == 0 {
            return Ok(());
        }
        let [xs_h, xs_l] = x0.to_be_bytes();
        let [xe_h, xe_l] = (x0 + width - 1).to_be_bytes();
        let [ys_h, ys_l] = y0.to_be_bytes();
        let [ye_h, ye_l] = (y0 + height - 1).to_be_bytes();
        self.command(cmd::CASET, &[xs_h, xs_l, xe_h, xe_l]).await?;
        self.command(cmd::RASET, &[ys_h, ys_l, ye_h, ye_l]).await?;

        let _cs = CsGuard::select(&mut self.cs);
        header(&mut self.spi, &mut self.dc, OPCODE_WRITE_COLOR, cmd::RAMWR).await?;
        for chunk in pixels.chunks(CHUNK) {
            let buf = &mut self.bounce[..chunk.len()];
            buf.copy_from_slice(chunk);
            self.spi
                .half_duplex_write_async(DataMode::Quad, Command::None, Address::None, 0, buf)
                .await?;
        }
        Ok(())
    }

    /// Sends the rows of `fb` that changed since the last flush. Returns the
    /// number of rows sent.
    pub async fn flush(&mut self, fb: &mut FrameBuffer) -> Result<u16, Error> {
        let Some((first, last)) = fb.take_dirty() else {
            return Ok(0);
        };
        let start = usize::from(first) * ROW_BYTES;
        let end = (usize::from(last) + 1) * ROW_BYTES;
        self.write_window(0, first, WIDTH, &fb.buf[start..end]).await?;
        Ok(last - first + 1)
    }
}

/// Holds CS low while alive and releases it on drop, also when a transfer
/// future is cancelled.
struct CsGuard<'a, 'd>(&'a mut Output<'d>);

impl<'a, 'd> CsGuard<'a, 'd> {
    fn select(cs: &'a mut Output<'d>) -> Self {
        cs.set_low();
        Self(cs)
    }
}

impl Drop for CsGuard<'_, '_> {
    fn drop(&mut self) {
        self.0.set_high();
    }
}

/// Sends the 32-bit transfer header on one line with DC low, then raises DC
/// for the parameter or pixel phase.
async fn header(
    spi: &mut SpiDma<'static, Async>,
    dc: &mut Output<'static>,
    opcode: u8,
    command: u8,
) -> Result<(), Error> {
    dc.set_low();
    spi.half_duplex_write_async(
        DataMode::Single,
        Command::None,
        Address::None,
        0,
        &[opcode, 0x00, command, 0x00],
    )
    .await?;
    dc.set_high();
    Ok(())
}

/// A full-screen RGB565 frame buffer (in PSRAM, since it is 253 KiB) with
/// dirty-row tracking. Implements embedded-graphics' [`DrawTarget`].
pub struct FrameBuffer {
    buf: Box<[u8]>,
    dirty: Option<(u16, u16)>,
}

impl FrameBuffer {
    /// Allocates a black frame buffer. The heap must include PSRAM.
    pub fn new() -> Self {
        Self {
            buf: vec![0u8; FRAME_BYTES].into_boxed_slice(),
            dirty: Some((0, HEIGHT - 1)),
        }
    }

    /// Marks the whole frame as changed.
    pub fn mark_all_dirty(&mut self) {
        self.dirty = Some((0, HEIGHT - 1));
    }

    fn mark_dirty(&mut self, first: u16, last: u16) {
        self.dirty = Some(match self.dirty {
            Some((a, b)) => (a.min(first), b.max(last)),
            None => (first, last),
        });
    }

    fn take_dirty(&mut self) -> Option<(u16, u16)> {
        self.dirty.take()
    }

    /// Raw frame bytes (RGB565, high byte first).
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OriginDimensions for FrameBuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH.into(), HEIGHT.into())
    }
}

impl DrawTarget for FrameBuffer {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let (mut first, mut last) = (HEIGHT, 0);
        for Pixel(p, color) in pixels {
            if p.x < 0 || p.y < 0 || p.x >= WIDTH.into() || p.y >= HEIGHT.into() {
                continue;
            }
            let (x, y) = (p.x as usize, p.y as u16);
            let i = (usize::from(y) * usize::from(WIDTH) + x) * 2;
            self.buf[i..i + 2].copy_from_slice(&color.into_storage().to_be_bytes());
            first = first.min(y);
            last = last.max(y);
        }
        if first <= last {
            self.mark_dirty(first, last);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        let Some(bottom_right) = area.bottom_right() else {
            return Ok(());
        };
        let bytes = color.into_storage().to_be_bytes();
        let (x0, x1) = (area.top_left.x as usize, bottom_right.x as usize);
        let (y0, y1) = (area.top_left.y as u16, bottom_right.y as u16);
        for y in y0..=y1 {
            let row = usize::from(y) * ROW_BYTES;
            for px in self.buf[row + x0 * 2..row + (x1 + 1) * 2].chunks_exact_mut(2) {
                px.copy_from_slice(&bytes);
            }
        }
        self.mark_dirty(y0, y1);
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        self.draw_iter(area.points().zip(colors).map(|(p, c)| Pixel(p, c)))
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        let bytes = color.into_storage().to_be_bytes();
        for px in self.buf.chunks_exact_mut(2) {
            px.copy_from_slice(&bytes);
        }
        self.mark_all_dirty();
        Ok(())
    }
}

/// PWM backlight on GPIO44 (LEDC low-speed channel 0, 20 kHz, 10-bit).
pub struct Backlight {
    channel: channel::Channel<'static, LowSpeed>,
}

impl Backlight {
    /// Configures the PWM with the backlight off.
    pub fn new(ledc: LEDC<'static>, pin: GPIO44<'static>) -> Self {
        static LEDC_DRIVER: StaticCell<Ledc<'static>> = StaticCell::new();
        static TIMER: StaticCell<timer::Timer<'static, LowSpeed>> = StaticCell::new();

        let ledc = LEDC_DRIVER.init(Ledc::new(ledc));
        ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
        let timer = TIMER.init(ledc.timer::<LowSpeed>(timer::Number::Timer0));
        timer
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty10Bit,
                clock_source: timer::LSClockSource::APBClk,
                frequency: Rate::from_khz(20),
            })
            .expect("valid LEDC timer config");

        let mut channel = ledc.channel(channel::Number::Channel0, pin);
        channel
            .configure(channel::config::Config {
                timer,
                duty_pct: 0,
                drive_mode: esp_hal::gpio::DriveMode::PushPull,
            })
            .expect("valid LEDC channel config");
        Self { channel }
    }

    /// Sets the brightness in percent (0..=100).
    pub fn set(&mut self, percent: u8) {
        // Can only fail for values above 100.
        let _ = self.channel.set_duty(percent.min(100));
    }

    /// Fades to `percent` over `duration` in hardware.
    pub fn fade(&mut self, from: u8, to: u8, duration: Duration) {
        let _ = self.channel.start_duty_fade(
            from.min(100),
            to.min(100),
            duration.as_millis().min(u64::from(u16::MAX)) as u16,
        );
    }
}

//! Driver for the ES7210 4-channel microphone ADC (I2C 0x40).
//!
//! Ported from Espressif's `esp_codec_dev` ES7210 driver
//! (`components/esp_codec_dev/device/es7210/es7210.c` in esp-adf,
//! Apache-2.0, © 2023 Espressif Systems). The codec runs as I2S slave: the
//! ESP32-S3 provides MCLK (256 × fs), BCLK and WS, so no clock coefficients
//! are needed.
//!
//! On the ESP-VoCat, MIC1 and MIC2 are the two microphones on the MicBoard
//! and MIC3 is wired to the speaker output for echo cancellation. Only SDOUT1
//! is connected (GPIO3). In plain I2S mode it carries MIC1 (left) and MIC2
//! (right); the other two inputs would need TDM mode.

use embedded_hal_async::i2c::I2c;

use crate::board::i2c_addr;

mod reg {
    pub const RESET: u8 = 0x00;
    pub const CLOCK_OFF: u8 = 0x01;
    pub const MAINCLK: u8 = 0x02;
    pub const POWER_DOWN: u8 = 0x06;
    pub const OSR: u8 = 0x07;
    pub const MODE_CONFIG: u8 = 0x08;
    pub const TIME_CONTROL0: u8 = 0x09;
    pub const TIME_CONTROL1: u8 = 0x0A;
    pub const SDP_INTERFACE1: u8 = 0x11;
    pub const SDP_INTERFACE2: u8 = 0x12;
    pub const ADC34_MUTE: u8 = 0x14;
    pub const ADC12_MUTE: u8 = 0x15;
    pub const ADC34_HPF2: u8 = 0x20;
    pub const ADC34_HPF1: u8 = 0x21;
    pub const ADC12_HPF1: u8 = 0x22;
    pub const ADC12_HPF2: u8 = 0x23;
    pub const CHIP_ID1: u8 = 0x3D;
    pub const CHIP_ID0: u8 = 0x3E;
    pub const ANALOG: u8 = 0x40;
    pub const MIC12_BIAS: u8 = 0x41;
    pub const MIC34_BIAS: u8 = 0x42;
    pub const MIC1_GAIN: u8 = 0x43;
    pub const MIC2_GAIN: u8 = 0x44;
    pub const MIC3_GAIN: u8 = 0x45;
    pub const MIC4_GAIN: u8 = 0x46;
    pub const MIC1_POWER: u8 = 0x47;
    pub const MIC12_POWER: u8 = 0x4B;
    pub const MIC34_POWER: u8 = 0x4C;
}

/// Chip ID registers 0x3D/0x3E.
pub const CHIP_ID: [u8; 2] = [0x72, 0x10];

/// Microphone preamplifier gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gain(u8);

impl Gain {
    /// The highest gain, 37.5 dB.
    pub const MAX: Gain = Gain(14);

    /// The nearest gain at or below `db`: 0 to 33 dB in 3 dB steps, then
    /// 34.5, 36 and 37.5 dB.
    pub fn from_db(db: f32) -> Self {
        let code = match db {
            d if d >= 37.5 => 14,
            d if d >= 36.0 => 13,
            d if d >= 34.5 => 12,
            d if d >= 0.0 => ((d / 3.0) as u8).min(11),
            _ => 0,
        };
        Gain(code)
    }

    /// The gain in dB.
    pub fn db(self) -> f32 {
        match self.0 {
            0..=11 => f32::from(self.0) * 3.0,
            12 => 34.5,
            13 => 36.0,
            _ => 37.5,
        }
    }
}

/// ES7210 driver over any async I2C bus.
pub struct Es7210<I> {
    i2c: I,
}

impl<I: I2c> Es7210<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    async fn write(&mut self, reg: u8, value: u8) -> Result<(), I::Error> {
        self.i2c.write(i2c_addr::ES7210, &[reg, value]).await
    }

    async fn read(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut b = [0u8];
        self.i2c
            .write_read(i2c_addr::ES7210, &[reg], &mut b)
            .await?;
        Ok(b[0])
    }

    async fn update(&mut self, reg: u8, mask: u8, value: u8) -> Result<(), I::Error> {
        let old = self.read(reg).await?;
        self.write(reg, (old & !mask) | (value & mask)).await
    }

    /// Reads the chip ID; [`CHIP_ID`] for an ES7210.
    pub async fn chip_id(&mut self) -> Result<[u8; 2], I::Error> {
        Ok([self.read(reg::CHIP_ID1).await?, self.read(reg::CHIP_ID0).await?])
    }

    /// Configures the codec as I2S slave with 16-bit samples and MIC1/MIC2
    /// enabled at `gain`, and starts it. MCLK, BCLK and WS must be running.
    pub async fn init(&mut self, gain: Gain) -> Result<(), I::Error> {
        // es7210_open()
        self.write(reg::RESET, 0xFF).await?;
        self.write(reg::RESET, 0x41).await?;
        self.write(reg::CLOCK_OFF, 0x3F).await?;
        self.write(reg::TIME_CONTROL0, 0x30).await?;
        self.write(reg::TIME_CONTROL1, 0x30).await?;
        self.write(reg::ADC12_HPF2, 0x2A).await?;
        self.write(reg::ADC12_HPF1, 0x0A).await?;
        self.write(reg::ADC34_HPF2, 0x0A).await?;
        self.write(reg::ADC34_HPF1, 0x2A).await?;
        // Slave mode.
        self.update(reg::MODE_CONFIG, 0x01, 0x00).await?;
        // Analog on, VDDA 3.3 V, mic bias 2.87 V.
        self.write(reg::ANALOG, 0x43).await?;
        self.write(reg::MIC12_BIAS, 0x70).await?;
        self.write(reg::MIC34_BIAS, 0x70).await?;
        self.write(reg::OSR, 0x20).await?;
        self.write(reg::MAINCLK, 0xC1).await?;
        self.select_mics(gain).await?;
        let clock_off = self.read(reg::CLOCK_OFF).await?;

        // es7210_set_fs(): 16-bit, standard I2S.
        let iface = self.read(reg::SDP_INTERFACE1).await?;
        self.write(reg::SDP_INTERFACE1, (iface & 0x1C) | 0x60).await?;

        // es7210_start()
        self.write(reg::CLOCK_OFF, clock_off).await?;
        self.write(reg::POWER_DOWN, 0x00).await?;
        self.write(reg::ANALOG, 0x43).await?;
        for i in 0..4 {
            self.write(reg::MIC1_POWER + i, 0x08).await?;
        }
        self.select_mics(gain).await?;
        self.write(reg::ANALOG, 0x43).await?;
        self.write(reg::RESET, 0x71).await?;
        self.write(reg::RESET, 0x41).await?;

        // Unmute all ADCs.
        self.update(reg::ADC34_MUTE, 0x03, 0x00).await?;
        self.update(reg::ADC12_MUTE, 0x03, 0x00).await
    }

    /// es7210_mic_select() for MIC1 and MIC2.
    async fn select_mics(&mut self, gain: Gain) -> Result<(), I::Error> {
        for r in [reg::MIC1_GAIN, reg::MIC2_GAIN, reg::MIC3_GAIN, reg::MIC4_GAIN] {
            self.update(r, 0x10, 0x00).await?;
        }
        self.write(reg::MIC12_POWER, 0xFF).await?;
        self.write(reg::MIC34_POWER, 0xFF).await?;
        // MIC1 and MIC2 clocks and power on, gain enabled and set.
        self.update(reg::CLOCK_OFF, 0x0B, 0x00).await?;
        self.write(reg::MIC12_POWER, 0x00).await?;
        for r in [reg::MIC1_GAIN, reg::MIC2_GAIN] {
            self.update(r, 0x10, 0x10).await?;
            self.update(r, 0x0F, gain.0).await?;
        }
        // Not TDM.
        self.write(reg::SDP_INTERFACE2, 0x00).await
    }

    /// Changes the MIC1/MIC2 gain.
    pub async fn set_gain(&mut self, gain: Gain) -> Result<(), I::Error> {
        self.update(reg::MIC1_GAIN, 0x0F, gain.0).await?;
        self.update(reg::MIC2_GAIN, 0x0F, gain.0).await
    }
}

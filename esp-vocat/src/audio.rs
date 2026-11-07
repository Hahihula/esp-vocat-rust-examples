//! Audio output: I2S to the ES8311 codec and the NS4150B speaker amplifier.
//!
//! Wiring: MCLK GPIO42, BCLK GPIO40, WS GPIO39, DOUT GPIO41 (to the ES8311),
//! amplifier enable GPIO15 (active high), codec power GPIO48.
//!
//! The ESP32-S3 is I2S master and sends 16-bit stereo frames (Philips). The
//! ES8311 driver from `oxstack-drivers` runs the codec as slave with its clock
//! derived from BCLK, so it needs no MCLK; MCLK is still generated for the
//! ES7210 microphone ADC on the same bus. The codec has one DAC, which plays the
//! left channel, and the speaker is mono.
//!
//! [`AudioOut`] streams through a DMA ring buffer. Something has to keep
//! filling it (silence when there's nothing to play), otherwise the DMA runs
//! dry and stops; [`AudioOut::fill`] is meant to be called in a loop from one
//! task.
//!
//! [`AudioIn`] is the receive side: stereo frames from the ES7210
//! microphone ADC on DIN (GPIO3).
//!
//! [`Synth`] is a small sine/square synthesiser with an envelope, enough for
//! beeps and simple melodies.

use embassy_time::Duration;
use esp_hal::{
    Async,
    dma::{DmaRxStreamBuf, DmaTxStreamBuf},
    dma_rx_stream_buffer, dma_tx_stream_buffer,
    i2s::master::{
        Channels, DataFormat, Error as I2sError, I2s, I2sRxDmaTransfer, I2sTxDmaTransfer, TdmConfig,
    },
    peripherals::{DMA_CH1, GPIO3, GPIO39, GPIO40, GPIO41, GPIO42, I2S0},
    time::Rate,
};
use libm::sinf;

/// Output sample rate.
pub const SAMPLE_RATE: u32 = 44_100;
/// Bytes per stereo frame (two 16-bit samples).
const FRAME_BYTES: usize = 4;
/// Size of the DMA ring buffer: about 46 ms of audio.
const RING_BYTES: usize = 8192;

/// The I2S pins.
pub struct I2sPins {
    pub mclk: GPIO42<'static>,
    pub bclk: GPIO40<'static>,
    pub ws: GPIO39<'static>,
    pub dout: GPIO41<'static>,
}

/// Errors from the audio output.
#[derive(Debug)]
pub enum Error {
    Config(esp_hal::i2s::master::ConfigError),
    I2s(I2sError),
    Dma(esp_hal::dma::DmaError),
    /// Restarting after a DMA error failed; the output is gone.
    Stopped,
}

/// A running I2S output stream.
pub struct AudioOut {
    /// `None` only if restarting after an error failed.
    transfer: Option<I2sTxDmaTransfer<'static, Async, DmaTxStreamBuf>>,
}

impl AudioOut {
    /// Starts the I2S clocks and a stream of silence.
    pub fn new(i2s: I2S0<'static>, dma: DMA_CH1<'static>, pins: I2sPins) -> Result<Self, Error> {
        let config = TdmConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO);
        let i2s = I2s::new(i2s, dma, config)
            .map_err(Error::Config)?
            .with_mclk(pins.mclk)
            .into_async();
        let tx = i2s
            .i2s_tx
            .with_bclk(pins.bclk)
            .with_ws(pins.ws)
            .with_dout(pins.dout)
            .build();

        let mut buf = dma_tx_stream_buffer!(RING_BYTES, 1024);
        // The stream must be pre-filled before it starts; fill all of it, so
        // there's slack before the first refill.
        while buf.push(&[0u8; 512]) > 0 {}
        let transfer = tx.write(buf).map_err(|(e, _, _)| Error::I2s(e))?;
        Ok(Self {
            transfer: Some(transfer),
        })
    }

    /// Waits until there is room in the ring buffer, then lets `render` fill
    /// all of it, in pieces of up to 256 mono samples. Each sample goes to
    /// both channels.
    ///
    /// If this isn't called again before the ring runs dry (about 46 ms), the
    /// DMA stops with a descriptor error. The stream is then restarted (the
    /// error is still returned, so the caller can log it); the ring replays
    /// its old contents once.
    pub async fn fill(&mut self, mut render: impl FnMut(&mut [i16])) -> Result<(), Error> {
        let transfer = self.transfer.as_mut().ok_or(Error::Stopped)?;
        if let Err(e) = transfer.wait_for_available_async().await {
            self.restart()?;
            return Err(Error::Dma(e));
        }
        let mut mono = [0i16; 256];
        while transfer.available_bytes() >= FRAME_BYTES {
            transfer.push_with(|buf| {
                let frames = (buf.len() / FRAME_BYTES).min(mono.len());
                render(&mut mono[..frames]);
                for (frame, s) in buf.chunks_exact_mut(FRAME_BYTES).zip(&mono[..frames]) {
                    let [lo, hi] = s.to_le_bytes();
                    frame.copy_from_slice(&[lo, hi, lo, hi]);
                }
                frames * FRAME_BYTES
            });
        }
        Ok(())
    }

    /// Stops the dead transfer and starts a new one on the same ring.
    fn restart(&mut self) -> Result<(), Error> {
        let transfer = self.transfer.take().ok_or(Error::Stopped)?;
        let (tx, buf) = transfer.stop();
        let transfer = tx.write(buf).map_err(|(e, _, _)| Error::I2s(e))?;
        self.transfer = Some(transfer);
        Ok(())
    }
}

/// The I2S pins for recording.
pub struct I2sInPins {
    pub mclk: GPIO42<'static>,
    pub bclk: GPIO40<'static>,
    pub ws: GPIO39<'static>,
    /// SDOUT1 of the ES7210.
    pub din: GPIO3<'static>,
}

/// Size of the DMA ring buffer for recording.
const IN_RING_BYTES: usize = 8192;

/// A running I2S input stream: 16-bit stereo frames from the ES7210 (left =
/// MIC1, right = MIC2).
pub struct AudioIn {
    transfer: I2sRxDmaTransfer<'static, Async, DmaRxStreamBuf>,
    /// Bytes of a frame that was split between two DMA descriptors.
    partial: [u8; FRAME_BYTES],
    partial_len: usize,
}

impl AudioIn {
    /// Starts the I2S clocks (MCLK = 256 × fs) and the receive stream. The
    /// ES7210 can be initialised once this is running.
    pub fn new(
        i2s: I2S0<'static>,
        dma: DMA_CH1<'static>,
        pins: I2sInPins,
        sample_rate: u32,
    ) -> Result<Self, Error> {
        let config = TdmConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(sample_rate))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO);
        let i2s = I2s::new(i2s, dma, config)
            .map_err(Error::Config)?
            .with_mclk(pins.mclk)
            .into_async();
        let rx = i2s
            .i2s_rx
            .with_bclk(pins.bclk)
            .with_ws(pins.ws)
            .with_din(pins.din)
            .build();
        let buf = dma_rx_stream_buffer!(IN_RING_BYTES, 1024);
        let transfer = rx.read(buf).map_err(|(e, _, _)| Error::I2s(e))?;
        Ok(Self {
            transfer,
            partial: [0; FRAME_BYTES],
            partial_len: 0,
        })
    }

    /// Waits for recorded data and passes each frame (left, right) to
    /// `frame`. Has to be called often enough that the ring (about 128 ms at
    /// 16 kHz) never fills up.
    pub async fn read(&mut self, mut frame: impl FnMut(i16, i16)) -> Result<usize, Error> {
        self.transfer
            .wait_for_available_async()
            .await
            .map_err(Error::Dma)?;
        let mut frames = 0;
        let emit = |f: &[u8], frame: &mut dyn FnMut(i16, i16)| {
            frame(
                i16::from_le_bytes([f[0], f[1]]),
                i16::from_le_bytes([f[2], f[3]]),
            );
        };
        loop {
            let data = self.transfer.peek();
            if data.is_empty() {
                break;
            }
            let len = data.len();
            let mut rest = data;
            // A descriptor doesn't always end on a frame boundary: complete
            // the frame carried over from the previous piece first.
            if self.partial_len > 0 {
                let take = (FRAME_BYTES - self.partial_len).min(rest.len());
                self.partial[self.partial_len..self.partial_len + take]
                    .copy_from_slice(&rest[..take]);
                self.partial_len += take;
                rest = &rest[take..];
                if self.partial_len == FRAME_BYTES {
                    emit(&self.partial, &mut frame);
                    frames += 1;
                    self.partial_len = 0;
                }
            }
            let whole = rest.len() / FRAME_BYTES * FRAME_BYTES;
            for f in rest[..whole].chunks_exact(FRAME_BYTES) {
                emit(f, &mut frame);
            }
            frames += whole / FRAME_BYTES;
            let tail = &rest[whole..];
            self.partial[self.partial_len..self.partial_len + tail.len()].copy_from_slice(tail);
            self.partial_len += tail.len();
            // Always consume everything, or the ring stops draining.
            self.transfer.consume(len);
        }
        Ok(frames)
    }
}

/// Oscillator waveform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wave {
    Sine,
    Square,
    Triangle,
}

/// A note: frequency in Hz (0 = rest) and duration.
#[derive(Debug, Clone, Copy)]
pub struct Note {
    pub hz: f32,
    pub duration: Duration,
}

impl Note {
    pub const fn new(hz: f32, ms: u64) -> Self {
        Self {
            hz,
            duration: Duration::from_millis(ms),
        }
    }
}

/// Frequencies of the octave starting at middle C (C4..B4).
pub mod pitch {
    pub const C4: f32 = 261.63;
    pub const D4: f32 = 293.66;
    pub const E4: f32 = 329.63;
    pub const F4: f32 = 349.23;
    pub const G4: f32 = 392.00;
    pub const A4: f32 = 440.00;
    pub const B4: f32 = 493.88;
    pub const C5: f32 = 523.25;
    pub const D5: f32 = 587.33;
    pub const E5: f32 = 659.25;
    pub const G5: f32 = 783.99;
    pub const REST: f32 = 0.0;
}

const LUT_BITS: u32 = 8;
const LUT_LEN: usize = 1 << LUT_BITS;
/// Envelope attack and release, in samples.
const ATTACK: u32 = SAMPLE_RATE / 200;
const RELEASE: u32 = SAMPLE_RATE / 40;

/// What the synthesiser is doing.
enum Voice {
    Silent,
    /// A note sequence; `index` is the current note, `left` its remaining samples.
    Sequence {
        notes: &'static [Note],
        index: usize,
        left: u32,
        len: u32,
    },
    /// A sweep from `from` to `to` Hz (exponential).
    Sweep {
        from: f32,
        to: f32,
        pos: u32,
        len: u32,
    },
}

/// A single-voice synthesiser.
pub struct Synth {
    lut: [i16; LUT_LEN],
    wave: Wave,
    /// Phase accumulator; the top [`LUT_BITS`] bits index the table.
    phase: u32,
    step: u32,
    voice: Voice,
    /// Software gain, 0..=256.
    gain: i32,
}

impl Default for Synth {
    fn default() -> Self {
        Self::new()
    }
}

impl Synth {
    pub fn new() -> Self {
        let mut lut = [0i16; LUT_LEN];
        for (i, v) in lut.iter_mut().enumerate() {
            let x = i as f32 / LUT_LEN as f32 * core::f32::consts::TAU;
            *v = (sinf(x) * 32000.0) as i16;
        }
        Self {
            lut,
            wave: Wave::Sine,
            phase: 0,
            step: 0,
            voice: Voice::Silent,
            gain: 256,
        }
    }

    /// Selects the waveform for the following notes.
    pub fn set_wave(&mut self, wave: Wave) {
        self.wave = wave;
    }

    /// Sets the software gain in percent.
    pub fn set_gain_pct(&mut self, pct: u8) {
        self.gain = i32::from(pct.min(100)) * 256 / 100;
    }

    /// Plays a note sequence, replacing whatever was playing.
    pub fn play(&mut self, notes: &'static [Note]) {
        self.voice = Voice::Sequence {
            notes,
            index: 0,
            left: 0,
            len: 0,
        };
        self.start_note();
    }

    /// Sweeps from `from` to `to` Hz over `duration`.
    pub fn sweep(&mut self, from: f32, to: f32, duration: Duration) {
        self.voice = Voice::Sweep {
            from,
            to,
            pos: 0,
            len: samples(duration),
        };
    }

    /// Stops at once.
    pub fn stop(&mut self) {
        self.voice = Voice::Silent;
    }

    /// Whether anything is playing.
    pub fn is_playing(&self) -> bool {
        !matches!(self.voice, Voice::Silent)
    }

    fn set_freq(&mut self, hz: f32) {
        self.step = (hz / SAMPLE_RATE as f32 * 4_294_967_296.0) as u32;
    }

    fn start_note(&mut self) {
        if let Voice::Sequence {
            notes,
            index,
            left,
            len,
        } = &mut self.voice
        {
            match notes.get(*index) {
                Some(n) => {
                    *len = samples(n.duration);
                    *left = *len;
                    let hz = n.hz;
                    self.set_freq(hz);
                }
                None => self.voice = Voice::Silent,
            }
        }
    }

    fn oscillator(&mut self) -> i32 {
        self.phase = self.phase.wrapping_add(self.step);
        let i = (self.phase >> (32 - LUT_BITS)) as usize;
        match self.wave {
            Wave::Sine => i32::from(self.lut[i]),
            Wave::Square => {
                if self.phase < 0x8000_0000 {
                    16000
                } else {
                    -16000
                }
            }
            Wave::Triangle => {
                let p = (self.phase >> 16) as i32; // 0..65536
                let t = if p < 32768 { p } else { 65535 - p };
                (t - 16384) * 2
            }
        }
    }

    /// Renders the next samples.
    pub fn render(&mut self, out: &mut [i16]) {
        for s in out {
            let (amp, rest) = match &mut self.voice {
                Voice::Silent => (0, true),
                Voice::Sequence {
                    notes,
                    index,
                    left,
                    len,
                } => {
                    let rest = notes[*index].hz == 0.0;
                    let elapsed = *len - *left;
                    let env = envelope(elapsed, *left);
                    *left -= 1;
                    if *left == 0 {
                        *index += 1;
                        self.start_note();
                    }
                    (env, rest)
                }
                Voice::Sweep { from, to, pos, len } => {
                    let t = *pos as f32 / *len as f32;
                    // Exponential sweep: equal time per octave.
                    let hz = *from * libm::powf(*to / *from, t);
                    let env = envelope(*pos, *len - *pos);
                    *pos += 1;
                    if *pos >= *len {
                        self.voice = Voice::Silent;
                    }
                    self.set_freq(hz);
                    (env, false)
                }
            };
            let v = if rest || amp == 0 {
                0
            } else {
                self.oscillator() * amp / 256 * self.gain / 256
            };
            *s = v as i16;
        }
    }
}

/// Linear attack/release envelope, 0..=256.
fn envelope(elapsed: u32, left: u32) -> i32 {
    let a = if elapsed < ATTACK {
        elapsed * 256 / ATTACK
    } else {
        256
    };
    let r = if left < RELEASE {
        left * 256 / RELEASE
    } else {
        256
    };
    a.min(r) as i32
}

fn samples(d: Duration) -> u32 {
    (d.as_micros() * u64::from(SAMPLE_RATE) / 1_000_000).max(1) as u32
}

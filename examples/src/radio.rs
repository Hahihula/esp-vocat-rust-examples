//! Internet radio: an HTTP (Icecast/SHOUTcast) MP3 stream decoded to the
//! speaker, with a volume ramp for waking up gently.
//!
//! Two pieces run in different tasks and meet in [`STREAM`], a byte pipe:
//!
//! - [`stream_task`] downloads the stream (plain HTTP/1.0, following up to
//!   three redirects) while the radio is on, and reconnects when it drops.
//! - [`Player`] runs inside the audio task's `fill` callback. It takes MP3
//!   data from the pipe, decodes it with `nanomp3-core`, mixes it to mono,
//!   resamples it to the output rate if needed and applies the gain.
//!
//! [`play`] and [`stop`] control both from anywhere.

use alloc::string::{String, ToString};
use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::url::{self, Url};
use embassy_futures::select::{Either, select};
use embassy_net::{IpAddress, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, pipe::Pipe, signal::Signal};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_io_async::Write as _;
use log::{info, warn};
use nanomp3_core::{Channels, DecodeError, Decoder, MAX_SAMPLES_PER_FRAME};
use static_cell::ConstStaticCell;

use esp_vocat::audio::SAMPLE_RATE;

/// Default stream: SomaFM Groove Salad, 128 kbit/s MP3, 44.1 kHz.
pub const DEFAULT_URL: &str = "http://ice1.somafm.com/groovesalad-128-mp3";

/// The stream URL: `RADIO_URL` at build time, or [`DEFAULT_URL`].
pub fn url() -> &'static str {
    option_env!("RADIO_URL")
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_URL)
}

/// Downloaded MP3 data waiting to be decoded: 2 s at 128 kbit/s.
const STREAM_BYTES: usize = 32 * 1024;
/// Playback starts (and restarts after an underrun) once this much is buffered.
const PREBUFFER_BYTES: usize = 16 * 1024;
/// MP3 input window handed to the decoder.
const WINDOW_BYTES: usize = 8 * 1024;
/// The decoder only runs with at least this much input, so it never sees a
/// frame cut off at the end of the window (a 320 kbit/s frame is 1.4 KiB,
/// and resyncing looks a few frames ahead).
const MIN_INPUT_BYTES: usize = 4 * 1024;

pub static STREAM: Pipe<CriticalSectionRawMutex, STREAM_BYTES> = Pipe::new();

static ON: AtomicBool = AtomicBool::new(false);
/// Wakes the stream task when [`ON`] changes.
static STREAM_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Commands for the [`Player`].
static PLAYER_CMD: Signal<CriticalSectionRawMutex, Command> = Signal::new();

#[derive(Clone, Copy)]
enum Command {
    Play { ramp: Duration },
    Stop,
}

/// Starts the radio; the volume rises from [`RAMP_FROM_DB`] to full over
/// `ramp` (zero = full at once).
pub fn play(ramp: Duration) {
    ON.store(true, Ordering::Relaxed);
    STREAM_WAKE.signal(());
    PLAYER_CMD.signal(Command::Play { ramp });
}

/// Stops the radio and closes the connection.
pub fn stop() {
    ON.store(false, Ordering::Relaxed);
    STREAM_WAKE.signal(());
    PLAYER_CMD.signal(Command::Stop);
}

/// Whether the radio is on (it may still be connecting or buffering).
pub fn is_on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Counters for the log.
pub mod stats {
    use core::sync::atomic::AtomicU32;
    /// Times the decoder ran out of input while playing.
    pub static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
    /// MP3 frames decoded.
    pub static FRAMES: AtomicU32 = AtomicU32::new(0);
    /// Microseconds spent decoding.
    pub static DECODE_US: AtomicU32 = AtomicU32::new(0);
    /// Bytes downloaded.
    pub static BYTES: AtomicU32 = AtomicU32::new(0);
    /// Current gain in tenths of a dB below full (0 = full volume).
    pub static ATTENUATION_DB10: AtomicU32 = AtomicU32::new(0);
}

/// Bytes waiting in the stream buffer.
pub fn buffered() -> usize {
    STREAM.len()
}

// ---------------------------------------------------------------- download

#[derive(Debug)]
pub enum StreamError {
    BadUrl,
    Dns,
    Connect,
    Io,
    BadResponse,
    Status(u16),
    TooManyRedirects,
    Closed,
}

const RX_BYTES: usize = 8 * 1024;
const TX_BYTES: usize = 1024;
const HEADER_BYTES: usize = 2048;

struct Buffers {
    rx: [u8; RX_BYTES],
    tx: [u8; TX_BYTES],
    header: [u8; HEADER_BYTES],
    chunk: [u8; 1460],
}

static BUFFERS: ConstStaticCell<Buffers> = ConstStaticCell::new(Buffers {
    rx: [0; RX_BYTES],
    tx: [0; TX_BYTES],
    header: [0; HEADER_BYTES],
    chunk: [0; 1460],
});

/// Downloads `url` into [`STREAM`] whenever the radio is on.
#[embassy_executor::task]
pub async fn stream_task(stack: Stack<'static>, url: &'static str) -> ! {
    let buffers = BUFFERS.take();
    loop {
        while !is_on() {
            STREAM_WAKE.wait().await;
        }
        let off = async {
            loop {
                STREAM_WAKE.wait().await;
                if !is_on() {
                    break;
                }
            }
        };
        match select(stream(stack, url, buffers), off).await {
            Either::First(Err(e)) => {
                warn!("radio: stream failed: {e:?}, retrying in 3 s");
                Timer::after(Duration::from_secs(3)).await;
            }
            Either::First(Ok(())) => {}
            Either::Second(()) => info!("radio: disconnected"),
        }
    }
}

async fn stream(stack: Stack<'_>, url: &str, buffers: &mut Buffers) -> Result<(), StreamError> {
    let mut current = url.to_string();
    for _ in 0..4 {
        match request(stack, &current, buffers).await? {
            Some(location) => {
                info!("radio: redirected to {location}");
                current = location;
            }
            None => return Ok(()),
        }
    }
    Err(StreamError::TooManyRedirects)
}

/// One GET request. Streams the body into [`STREAM`] until the connection
/// ends; returns the target of a redirect instead if there is one.
async fn request(
    stack: Stack<'_>,
    url: &str,
    buffers: &mut Buffers,
) -> Result<Option<String>, StreamError> {
    let Url { host, port, path } = url::parse(url).ok_or(StreamError::BadUrl)?;
    let addrs = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| StreamError::Dns)?;
    let addr: IpAddress = *addrs.first().ok_or(StreamError::Dns)?;

    let Buffers {
        rx,
        tx,
        header,
        chunk,
    } = buffers;
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    info!("radio: connecting to {host} ({addr}:{port})");
    with_timeout(Duration::from_secs(10), socket.connect((addr, port)))
        .await
        .map_err(|_| StreamError::Connect)?
        .map_err(|_| StreamError::Connect)?;

    // HTTP/1.0, so the body is never chunked. No ICY metadata: it would be
    // interleaved with the audio.
    let mut req = String::new();
    let _ = write!(
        req,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: vocat-radio/0.1\r\n\
         Accept: */*\r\nIcy-MetaData: 0\r\nConnection: close\r\n\r\n"
    );
    socket
        .write_all(req.as_bytes())
        .await
        .map_err(|_| StreamError::Io)?;

    // Read until the end of the headers.
    let mut len = 0;
    let end = loop {
        if len == header.len() {
            return Err(StreamError::BadResponse);
        }
        let n = socket
            .read(&mut header[len..])
            .await
            .map_err(|_| StreamError::Io)?;
        if n == 0 {
            return Err(StreamError::Closed);
        }
        len += n;
        if let Some(i) = header[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let text = core::str::from_utf8(&header[..end]).map_err(|_| StreamError::BadResponse)?;
    let mut lines = text.split("\r\n");
    // "HTTP/1.1 200 OK" or SHOUTcast's "ICY 200 OK".
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or(StreamError::BadResponse)?;
    let mut location = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "location" => location = Some(value.to_string()),
            "content-type" | "icy-name" | "icy-br" | "icy-genre" => {
                info!("radio: {name}: {value}");
            }
            _ => {}
        }
    }
    match status {
        200 => {}
        301 | 302 | 303 | 307 | 308 => {
            return location.ok_or(StreamError::BadResponse).map(Some);
        }
        s => return Err(StreamError::Status(s)),
    }

    // Start from an empty buffer, so the decoder doesn't mix two streams.
    STREAM.clear();
    let body = &header[end..len];
    STREAM.write_all(body).await;
    stats::BYTES.fetch_add(body.len() as u32, Ordering::Relaxed);
    loop {
        let n = socket.read(chunk).await.map_err(|_| StreamError::Io)?;
        if n == 0 {
            return Err(StreamError::Closed);
        }
        stats::BYTES.fetch_add(n as u32, Ordering::Relaxed);
        STREAM.write_all(&chunk[..n]).await;
    }
}

// ---------------------------------------------------------------- playback

/// Gain at the start of a ramp.
pub const RAMP_FROM_DB: f32 = -40.0;

#[derive(PartialEq, Eq)]
enum State {
    Idle,
    /// Waiting for [`PREBUFFER_BYTES`].
    Buffering,
    Playing,
}

/// Decodes [`STREAM`] into mono samples at [`SAMPLE_RATE`].
pub struct Player {
    decoder: Decoder,
    state: State,
    window: [u8; WINDOW_BYTES],
    win_start: usize,
    win_end: usize,
    /// The last decoded frame, interleaved.
    pcm: [i16; MAX_SAMPLES_PER_FRAME],
    pcm_len: usize,
    pcm_pos: usize,
    channels: usize,
    /// Linear-interpolation resampler: position between `prev` and `next`
    /// (16.16) and the step per output sample.
    phase: u32,
    step: u32,
    prev: i32,
    next: i32,
    /// Sample rate and bitrate of the last frame, to log changes.
    rate: u32,
    bitrate: u32,
    /// Volume ramp, in output samples.
    ramp_pos: u32,
    ramp_len: u32,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub const fn new() -> Self {
        Self {
            decoder: Decoder::new(),
            state: State::Idle,
            window: [0; WINDOW_BYTES],
            win_start: 0,
            win_end: 0,
            pcm: [0; MAX_SAMPLES_PER_FRAME],
            pcm_len: 0,
            pcm_pos: 0,
            channels: 2,
            phase: 0,
            step: 1 << 16,
            prev: 0,
            next: 0,
            rate: 0,
            bitrate: 0,
            ramp_pos: 0,
            ramp_len: 0,
        }
    }

    /// Fills `out` with the next samples (silence while idle or buffering).
    /// Called from the audio task.
    pub fn render(&mut self, out: &mut [i16]) {
        if let Some(cmd) = PLAYER_CMD.try_take() {
            self.command(cmd);
        }
        if self.state == State::Buffering && STREAM.len() >= PREBUFFER_BYTES {
            info!("radio: buffered, playing");
            self.state = State::Playing;
        }
        if self.state != State::Playing {
            out.fill(0);
            return;
        }

        let gain = self.gain();
        for (i, s) in out.iter_mut().enumerate() {
            while self.phase >= 1 << 16 {
                self.phase -= 1 << 16;
                self.prev = self.next;
                match self.next_sample() {
                    Some(v) => self.next = v,
                    None => {
                        stats::UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                        warn!("radio: buffer ran dry, rebuffering");
                        self.state = State::Buffering;
                        out[i..].fill(0);
                        return;
                    }
                }
            }
            let frac = i64::from(self.phase >> 1); // 0..32768
            // In i64: the difference times frac comes within 0.005% of i32::MAX.
            let v = self.prev + ((i64::from(self.next - self.prev) * frac) >> 15) as i32;
            *s = ((v * gain) >> 15).clamp(-32768, 32767) as i16;
            self.phase += self.step;
        }
        self.ramp_pos = self.ramp_pos.saturating_add(out.len() as u32);
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Play { ramp } => {
                self.decoder = Decoder::new();
                self.win_start = 0;
                self.win_end = 0;
                self.pcm_len = 0;
                self.pcm_pos = 0;
                self.phase = 0;
                self.prev = 0;
                self.next = 0;
                self.rate = 0;
                self.ramp_pos = 0;
                self.ramp_len = (ramp.as_millis() * u64::from(SAMPLE_RATE) / 1000) as u32;
                self.state = State::Buffering;
            }
            Command::Stop => {
                self.state = State::Idle;
                STREAM.clear();
            }
        }
    }

    /// Current gain in Q15, following the ramp (exponential in amplitude,
    /// i.e. linear in dB, which sounds even).
    fn gain(&self) -> i32 {
        let db = if self.ramp_pos >= self.ramp_len {
            0.0
        } else {
            RAMP_FROM_DB * (1.0 - self.ramp_pos as f32 / self.ramp_len as f32)
        };
        stats::ATTENUATION_DB10.store((-db * 10.0) as u32, Ordering::Relaxed);
        (libm::powf(10.0, db / 20.0) * 32768.0).min(32767.0) as i32
    }

    /// The next mono sample at the stream's own rate.
    fn next_sample(&mut self) -> Option<i32> {
        if self.pcm_pos >= self.pcm_len && !self.decode_frame() {
            return None;
        }
        let v = if self.channels == 2 {
            (i32::from(self.pcm[self.pcm_pos]) + i32::from(self.pcm[self.pcm_pos + 1])) / 2
        } else {
            i32::from(self.pcm[self.pcm_pos])
        };
        self.pcm_pos += self.channels;
        Some(v)
    }

    /// Tops up the input window from the pipe.
    fn refill(&mut self) {
        if self.win_start > 0 {
            self.window.copy_within(self.win_start..self.win_end, 0);
            self.win_end -= self.win_start;
            self.win_start = 0;
        }
        while self.win_end < WINDOW_BYTES {
            match STREAM.try_read(&mut self.window[self.win_end..]) {
                Ok(n) => self.win_end += n,
                Err(_) => break,
            }
        }
    }

    /// Decodes the next frame into `pcm`; false if there isn't enough input.
    fn decode_frame(&mut self) -> bool {
        loop {
            if self.win_end - self.win_start < MIN_INPUT_BYTES {
                self.refill();
                if self.win_end - self.win_start < MIN_INPUT_BYTES {
                    return false;
                }
            }
            let started = Instant::now();
            let (used, result) = self
                .decoder
                .decode(&self.window[self.win_start..self.win_end], &mut self.pcm);
            stats::DECODE_US.fetch_add(started.elapsed().as_micros() as u32, Ordering::Relaxed);
            self.win_start += used;
            match result {
                Ok(frame) if frame.samples_produced > 0 => {
                    stats::FRAMES.fetch_add(1, Ordering::Relaxed);
                    self.channels = match frame.channels {
                        Channels::Mono => 1,
                        Channels::Stereo => 2,
                    };
                    self.pcm_len = frame.samples_produced * self.channels;
                    self.pcm_pos = 0;
                    if frame.sample_rate != self.rate || frame.bitrate != self.bitrate {
                        if frame.sample_rate != self.rate {
                            self.step = ((u64::from(frame.sample_rate) << 16)
                                / u64::from(SAMPLE_RATE))
                                as u32;
                        }
                        self.rate = frame.sample_rate;
                        self.bitrate = frame.bitrate;
                        info!(
                            "radio: MPEG layer {}, {} kbit/s, {} Hz, {:?}",
                            frame.layer, frame.bitrate, frame.sample_rate, frame.channels
                        );
                    }
                    return true;
                }
                Err(DecodeError::NoFrame) if used == 0 => return false,
                Ok(_) | Err(DecodeError::ReservoirUnavailable(_)) => {}
                Err(e) => warn!("radio: {e}"),
            }
            // Never spin on the same input: this runs inside the audio fill
            // callback, and a stall there starves the I2S DMA. Skip a byte
            // and let the decoder resync.
            if used == 0 {
                self.win_start += 1;
            }
        }
    }
}

/// A [`Player`] in static memory (it's about 35 KiB).
pub fn player() -> &'static mut Player {
    static PLAYER: ConstStaticCell<Player> = ConstStaticCell::new(Player::new());
    PLAYER.take()
}

/// Counter snapshot, for [`Stats::since`].
#[derive(Clone, Copy)]
pub struct Stats {
    at: Instant,
    frames: u32,
    decode_us: u32,
    bytes: u32,
}

impl Stats {
    pub fn now() -> Self {
        Self {
            at: Instant::now(),
            frames: stats::FRAMES.load(Ordering::Relaxed),
            decode_us: stats::DECODE_US.load(Ordering::Relaxed),
            bytes: stats::BYTES.load(Ordering::Relaxed),
        }
    }

    /// Download rate in kbit/s and CPU share of the decoder in percent since `self`.
    pub fn since(&self) -> (u32, u32, u32) {
        let now = Self::now();
        let us = now.at.duration_since(self.at).as_micros().max(1) as u32;
        let kbps = (u64::from(now.bytes.wrapping_sub(self.bytes)) * 8_000 / u64::from(us)) as u32;
        let cpu =
            (u64::from(now.decode_us.wrapping_sub(self.decode_us)) * 100 / u64::from(us)) as u32;
        (kbps, cpu, now.frames.wrapping_sub(self.frames))
    }
}

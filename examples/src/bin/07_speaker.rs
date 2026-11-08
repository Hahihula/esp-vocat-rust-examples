//! Demo 07: speaker.
//!
//! Plays generated sound through I2S, the ES8311 codec and the NS4150B
//! amplifier. An audio task keeps the I2S DMA ring filled from a small
//! synthesiser; the main task sends it commands.
//!
//! - front pad tap: play the next item (scale, melody, sweep, waveforms)
//! - front pad long touch: stop
//! - back pad tap: next volume step

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Delay, Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_vocat::{
    audio::{AudioOut, Note, SAMPLE_RATE, Synth, Wave, pitch::*},
    board::{self, i2c_addr},
    touch::{Pad, TouchEvent, TouchInput},
    touch_pads,
};
use log::{info, warn};
use oxstack_drivers::es8311::{Es8311, Mode};

use esp_vocat_examples::demo::{START_VOLUME, VOLUME_DB, halt, volume_of};

use esp_vocat::i2s_out_pins;

esp_bootloader_esp_idf::esp_app_desc!();

const CHIME: &[Note] = &[Note::new(C5, 120), Note::new(E5, 120), Note::new(G5, 300)];

const SCALE: &[Note] = &[
    Note::new(C4, 250),
    Note::new(D4, 250),
    Note::new(E4, 250),
    Note::new(F4, 250),
    Note::new(G4, 250),
    Note::new(A4, 250),
    Note::new(B4, 250),
    Note::new(C5, 500),
];

/// Beethoven, Ode to Joy (first phrase).
const ODE_TO_JOY: &[Note] = &[
    Note::new(E4, 300),
    Note::new(E4, 300),
    Note::new(F4, 300),
    Note::new(G4, 300),
    Note::new(G4, 300),
    Note::new(F4, 300),
    Note::new(E4, 300),
    Note::new(D4, 300),
    Note::new(C4, 300),
    Note::new(C4, 300),
    Note::new(D4, 300),
    Note::new(E4, 300),
    Note::new(E4, 450),
    Note::new(D4, 150),
    Note::new(D4, 600),
    Note::new(REST, 300),
    Note::new(E4, 300),
    Note::new(E4, 300),
    Note::new(F4, 300),
    Note::new(G4, 300),
    Note::new(G4, 300),
    Note::new(F4, 300),
    Note::new(E4, 300),
    Note::new(D4, 300),
    Note::new(C4, 300),
    Note::new(C4, 300),
    Note::new(D4, 300),
    Note::new(E4, 300),
    Note::new(D4, 450),
    Note::new(C4, 150),
    Note::new(C4, 600),
];

const A440: &[Note] = &[Note::new(A4, 1000)];

enum Command {
    Play(&'static [Note], Wave),
    Sweep(f32, f32, Duration),
    Stop,
}

static COMMANDS: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

#[derive(Clone, Copy, Debug)]
enum Item {
    Scale,
    Melody,
    Sweep,
    Waveforms,
}

const ITEMS: [Item; 4] = [Item::Scale, Item::Melody, Item::Sweep, Item::Waveforms];

#[allow(
    clippy::large_stack_frames,
    reason = "main holds the codec and touch futures for the whole program"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_vocat_examples::start!(peripherals, "Demo 07: speaker (I2S, ES8311, NS4150B)");

    info!("front tap = next item, front long touch = stop, back tap = volume");

    let _codec_power = board::codec_power_on(peripherals.GPIO48);
    Timer::after(Duration::from_millis(20)).await;

    // I2S first: the codec derives its clock from BCLK.
    let pins = i2s_out_pins!(peripherals);
    let out = match AudioOut::new(peripherals.I2S0, peripherals.DMA_CH1, pins) {
        Ok(out) => out,
        Err(e) => halt("I2S init failed", e).await,
    };
    info!("I2S running: {SAMPLE_RATE} Hz, 16-bit stereo");
    spawner.spawn(audio_task(out).unwrap());

    let i2c = board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2)
        .expect("I2C config is valid");
    let mut codec = Es8311::new(i2c, i2c_addr::ES8311);
    let mut volume = START_VOLUME;
    let setup = async {
        codec.init(&mut Delay).await?;
        codec.set_dac_volume(volume_of(volume)).await?;
        codec.set_mode(Mode::Playback).await?;
        codec.set_dac_mute(false).await
    };
    if let Err(e) = setup.await {
        halt("ES8311 init failed", e).await;
    }
    info!("ES8311 in playback mode, volume {} dB", VOLUME_DB[volume]);

    // Amplifier on last, so the codec's power-up doesn't pop.
    let _pa = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());
    COMMANDS.send(Command::Play(CHIME, Wave::Sine)).await;

    let pads = touch_pads!(peripherals);
    let mut touch = TouchInput::new(pads).await;
    let mut next = 0;

    loop {
        match touch.next_event().await {
            TouchEvent::Tap(Pad::Front) => {
                let item = ITEMS[next];
                next = (next + 1) % ITEMS.len();
                info!("playing {item:?}");
                play(item).await;
            }
            TouchEvent::Long(Pad::Front) => {
                info!("stop");
                COMMANDS.send(Command::Stop).await;
            }
            TouchEvent::Tap(Pad::Back) => {
                volume = (volume + 1) % VOLUME_DB.len();
                match codec.set_dac_volume(volume_of(volume)).await {
                    Ok(()) => info!("volume {} dB", VOLUME_DB[volume]),
                    Err(e) => warn!("volume change failed: {e:?}"),
                }
                COMMANDS.send(Command::Play(CHIME, Wave::Sine)).await;
            }
            _ => {}
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "channel send futures; called from main only"
)]
async fn play(item: Item) {
    match item {
        Item::Scale => COMMANDS.send(Command::Play(SCALE, Wave::Sine)).await,
        Item::Melody => {
            COMMANDS
                .send(Command::Play(ODE_TO_JOY, Wave::Triangle))
                .await
        }
        Item::Sweep => {
            info!("  100 Hz -> 8 kHz in 4 s");
            COMMANDS
                .send(Command::Sweep(100.0, 8000.0, Duration::from_secs(4)))
                .await;
        }
        Item::Waveforms => {
            for wave in [Wave::Sine, Wave::Triangle, Wave::Square] {
                info!("  A4 (440 Hz) {wave:?}");
                COMMANDS.send(Command::Play(A440, wave)).await;
                Timer::after(Duration::from_millis(1200)).await;
            }
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "the synthesiser's sine table and the render buffer, ~1 KiB"
)]
#[embassy_executor::task]
async fn audio_task(mut out: AudioOut<'static>) {
    let mut synth = Synth::new();
    loop {
        while let Ok(cmd) = COMMANDS.try_receive() {
            match cmd {
                Command::Play(notes, wave) => {
                    synth.set_wave(wave);
                    synth.play(notes);
                }
                Command::Sweep(from, to, d) => {
                    synth.set_wave(Wave::Sine);
                    synth.sweep(from, to, d);
                }
                Command::Stop => synth.stop(),
            }
        }
        if let Err(e) = out.fill(|buf| synth.render(buf)).await {
            warn!("audio stream error: {e:?}");
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}

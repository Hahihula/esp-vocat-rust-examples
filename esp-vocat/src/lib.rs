//! Board support for the ESP-VoCat v1.2 (ESP32-S3): pins and power
//! switches, the round LCD and its touch controller, the touch pads, the
//! microphones and speaker, the fuel gauge, and motion detection.
//!
//! Pin assignment follows the v1.2 schematic:
//! <https://dl.espressif.com/AE/esp-dev-kits/ESP-VoCat_SCH_V1_2.pdf>
//!
//! The hardware-independent modules ([`gesture`], [`motion`], [`gain`])
//! touch no peripheral and read no clock (time is passed in), so they also
//! build for a PC, where they are unit-tested:
//! `cargo +stable test -p esp-vocat --target <host triple>`. The drivers
//! build only for the ESP32-S3.

#![no_std]

#[cfg(target_arch = "xtensa")]
extern crate alloc;

// Hardware-independent, tested on the host.
pub mod gain;
pub mod gesture;
pub mod motion;

// Drivers.
#[cfg(target_arch = "xtensa")]
pub mod audio;
#[cfg(target_arch = "xtensa")]
pub use bq27220_async as battery;
#[cfg(target_arch = "xtensa")]
pub mod board;
#[cfg(target_arch = "xtensa")]
pub mod display;
#[cfg(target_arch = "xtensa")]
pub mod es7210;
#[cfg(target_arch = "xtensa")]
mod st77916_init;
#[cfg(target_arch = "xtensa")]
pub mod touch;
#[cfg(target_arch = "xtensa")]
pub mod touchscreen;

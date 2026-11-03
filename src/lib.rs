//! Shared board support for the ESP-VoCat v1.2 demos.
//!
//! Pin assignment follows the v1.2 schematic:
//! <https://dl.espressif.com/AE/esp-dev-kits/ESP-VoCat_SCH_V1_2.pdf>

#![no_std]

extern crate alloc;

pub mod audio;
pub mod battery;
pub mod board;
pub mod display;
pub mod es7210;
pub mod motion;
mod st77916_init;
pub mod touch;
pub mod touchscreen;

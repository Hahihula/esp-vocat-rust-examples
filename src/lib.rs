//! Shared board support for the ESP-VoCat v1.2 demos.
//!
//! Pin assignment follows the v1.2 schematic:
//! <https://dl.espressif.com/AE/esp-dev-kits/ESP-VoCat_SCH_V1_2.pdf>

#![no_std]

pub mod battery;
pub mod board;
pub mod motion;
pub mod touch;

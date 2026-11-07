//! App-level helpers for the demos: Wi-Fi and networking (`net`), the
//! internet radio player (`radio`) and the local clock (`clock`). Kept out of
//! the `esp-vocat` library so a firmware using the board doesn't pull in the
//! network stack or the MP3 decoder.

#![no_std]

extern crate alloc;

pub mod clock;
pub mod net;
pub mod radio;

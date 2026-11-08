//! App-level helpers for the demos: start-up and LCD plumbing (`demo`),
//! Wi-Fi and networking (`net`), the internet radio player (`radio`) and the
//! local clock (`clock`). Kept out of the `esp-vocat` library so a firmware
//! using the board doesn't pull in the network stack or the MP3 decoder.
//!
//! `time` and `url` are hardware-independent and also build for a PC, where
//! they are unit-tested: `cargo +stable test -p esp-vocat-examples --lib
//! --target <host triple>`.

#![no_std]

#[cfg(target_arch = "xtensa")]
extern crate alloc;

// Hardware-independent, tested on the host.
pub mod time;
pub mod url;

// Board and network code.
#[cfg(target_arch = "xtensa")]
pub mod clock;
#[cfg(target_arch = "xtensa")]
pub mod demo;
#[cfg(target_arch = "xtensa")]
pub mod net;
#[cfg(target_arch = "xtensa")]
pub mod radio;

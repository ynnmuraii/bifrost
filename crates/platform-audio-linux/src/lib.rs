//! Local Unix Pulse protocol backend. No native Pulse/PipeWire/ALSA libraries.
//!
//! Portal PTT trusts live-session elapsed-millisecond compositor timestamps, not raw
//! key-up. A pre-CreateSession anchor retains conservative lag; mapped events older
//! than 250 ms, zero/constant/regressed/future clocks close native PTT. Delayed first
//! events can therefore make otherwise valid sessions unavailable; GUI hold-to-talk
//! remains independent. No unit guessing or receipt-time freshness calibration occurs.
//! Current KDE kglobalacceld emits zero timestamps and cannot prove this freshness:
//! <https://invent.kde.org/plasma/kglobalacceld/-/raw/master/src/component.cpp>.
//! Incompatible clocks, including seconds-style implementations, are unsupported;
//! incompatible units are not universally distinguishable from delayed valid signals.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
mod backend;
mod mpris;
mod mute;
mod ptt;
mod raw;
mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use backend::{Limitation, LinuxBackend};
pub use mute::LinuxMuteGuard;
pub use stream::LinuxStream;

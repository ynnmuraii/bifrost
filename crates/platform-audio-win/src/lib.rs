//! Optional Windows adapter implementing [`audio_backend::Backend`].
//! Construction/linking opens no devices, sessions or hotkeys; Linux builds an empty library.
//!
//! # Developer contract
//!
//! Capture/render callbacks execute on owning MTA workers with best-effort MMCSS "Pro Audio".
//! They must not allocate, block, lock, log or initialize lazy resources. Hand PCM off through
//! caller-preallocated SPSC rings. Stream snapshots preserve clock validity and device
//! invalidation; rebuild explicitly without replacing a selected endpoint. Debounce device
//! notifications for 100 ms before re-enumeration.
//!
//! The shared [`audio_backend::MuteGuard`] restores only owned changes and fails closed on
//! external changes or lost notifications. Callers using lower-level endpoint mute controls
//! must implement that ownership policy themselves; never clear a pre-existing mute.
//!
//! Media commands are bounded/asynchronous and checked against session token/revision.
//! Timeout or cancellation does not prove an already-dispatched OS command had no effect;
//! never retry automatically. Metadata is UTF-8 bounded and Debug-redacted; never log it.
//! Native session methods retain current-session identity and exact playback status; the
//! shared playback vocabulary preserves unmapped native states as Unknown.
//!
//! Supply all reserved KVM chords when configuring PTT. Consumers must check the absolute
//! 250 ms press lease on every use; renewal events cannot reopen an expired gate.
//!
//! # Hardware diagnostic
//!
//! Tests only read native devices/media sessions. The separate command
//! `cargo run -p platform-audio-win --example hw_probe` **plays an audible three-second
//! 1 kHz tone** and checks EXCLUDE capture against a positive INCLUDE control. Run only
//! with permission on the target desktop; it does not change endpoint selection, volume
//! or mute, and does not persist PCM.
#![cfg(windows)]

mod backend;
mod backend_devices;
mod backend_media;
pub use backend::{WindowsBackend, WindowsStream};
mod common;
pub mod devices;
pub mod hotkey;
pub mod media;
pub mod stream;

/// Startup/control errors. Real-time failures are compact [`stream::StreamStatus`] values.
#[derive(Debug)]
pub enum Error {
    Windows(windows::core::Error),
    Wasapi(wasapi::WasapiError),
    Unsupported(&'static str),
    InvalidArgument(&'static str),
    WorkerStopped,
}
impl From<windows::core::Error> for Error {
    fn from(value: windows::core::Error) -> Self {
        Self::Windows(value)
    }
}
impl From<wasapi::WasapiError> for Error {
    fn from(value: wasapi::WasapiError) -> Self {
        Self::Wasapi(value)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

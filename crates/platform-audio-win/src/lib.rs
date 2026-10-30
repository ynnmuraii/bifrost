//! Optional Windows audio adapter. Merely linking this crate opens no devices.
//! Native handles stay on their owning threads; PCM crosses preallocated SPSC rings.
//! Linux intentionally builds an empty library. See each module for lifecycle contracts.
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

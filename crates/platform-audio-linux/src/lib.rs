//! Local Unix Pulse protocol backend. No native Pulse/PipeWire/ALSA libraries.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
mod backend;
mod raw;
mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use backend::{Limitation, LinuxBackend, UnsupportedMute};
pub use stream::LinuxStream;

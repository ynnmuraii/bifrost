//! Pure audio contracts: no I/O, threads, clocks or external dependencies.

#![forbid(unsafe_code)]

pub mod drift;
pub mod focus;
pub mod jitter;
pub mod keys;
pub mod media;
pub mod microphone;
pub mod packet;
pub mod replay;
pub mod session;
pub mod status;

pub use packet::{validate_datagram, Header, PacketError, PacketKind};
pub use replay::{ReplayError, ReplayWindow};
pub use status::AudioStatus;

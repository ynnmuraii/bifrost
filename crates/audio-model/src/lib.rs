//! Pure audio contracts: no I/O, threads, clocks or external dependencies.

#![forbid(unsafe_code)]

pub mod drift;
pub mod jitter;
pub mod packet;
pub mod replay;
pub mod status;

pub use packet::{validate_datagram, Header, PacketError, PacketKind};
pub use replay::{ReplayError, ReplayWindow};
pub use status::AudioStatus;

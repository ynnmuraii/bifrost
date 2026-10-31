//! Opt-in media workers and pure pipeline primitives, independent of transport roots/core/IPC.
//! Prepare allocates codec/resampler storage; packet and frame processing reuse it.
#![forbid(unsafe_code)]
pub mod codec;
pub mod control;
pub mod crypto;
pub mod keys;
pub mod mixer;
mod native_rate;
pub mod pipeline;
pub mod runtime;
pub mod socket;

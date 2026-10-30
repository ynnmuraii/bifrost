//! Media-worker primitives. No sockets, threads, transport roots or device callbacks.
//! Prepare allocates codec/resampler storage; packet and frame processing reuse it.
#![forbid(unsafe_code)]
pub mod codec;
pub mod crypto;
pub mod keys;
pub mod pipeline;

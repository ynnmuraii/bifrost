//! Private audio-child supervision; independent of the input agent and GUI audio IPC.
// The coordinator is integrated separately; keep the allowance at this module boundary.
#![allow(dead_code, unused_imports)]

mod supervisor;

pub use supervisor::{
    AudioHandle, AudioPrivacyOperation, AudioRestartReason, AudioSupervisor, AudioSupervisorConfig,
    AudioSupervisorError, AudioSupervisorEvent,
};

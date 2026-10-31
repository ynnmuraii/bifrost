//! Private audio-child supervision; independent of the input agent and GUI audio IPC.

mod supervisor;

pub use supervisor::{
    AudioHandle, AudioSupervisor, AudioSupervisorConfig, AudioSupervisorError, AudioSupervisorEvent,
};

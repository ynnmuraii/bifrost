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
//! Registration starts unarmed. Both edges retain physical HID/scan/E0 identity independently
//! of the active layout or VK aliases (including main/keypad Enter). Rearming needs a
//! non-injected release of the configured non-modifier key, followed by a fresh non-injected
//! press. Modifier release, zero/unavailable asynchronous key state, reset and lease expiry
//! only revoke; they are never release proof. Original queued press timestamps are retained
//! rather than rejuvenated when drained. Observer failure/removal stays closed; reconfigure to
//! reinstall it. Registration-only checks do not qualify physical edges or desktop transitions.
//!
//! # Hardware diagnostic
//!
//! Tests only read native devices/media sessions. The separate command
//! `cargo run -p platform-audio-win --example hw_probe` **plays an audible three-second
//! 1 kHz tone** and checks EXCLUDE capture against a positive INCLUDE control. Run only
//! with permission on the target desktop; it does not change endpoint selection, volume
//! or mute, and does not persist PCM.
//!
//! `hw_probe --timing` initializes but does not start capture/render clients. It reports actual
//! capacities, engine periods and supported latency queries without playing a tone or changing
//! endpoint settings; process-loopback queries may be unsupported. `--period min|5ms|10ms`
//! selects a diagnostic candidate. `--stress-period 5ms --seconds 60` is explicitly audible.
//! Post-warmup source exhaustion, zero padding and the two-period starvation heuristic are
//! separate observations; the heuristic is not a driver xrun counter.
//!
//! Render attempts IAudioClient3 shared/event mode at a supported fundamental multiple with a
//! 5 ms floor and at most two queued periods, capped by actual buffer capacity. Unsupported
//! formats/interfaces/periods or a period locked by another app fall back to legacy shared
//! conversion with typed diagnostics. Legacy timing queries report the current period or an
//! explicitly labelled default-period estimate, never a fabricated negotiated period.
//!
//! The binary's `windows_e2e --audible` example plays a six-second 1 kHz tone, opens native
//! workers/local UDP and records loopback in memory. Its test-only 500 ms ciphertext delay
//! isolates the Listener tail; delay subtraction is an estimate, not synchronized physical
//! end-to-end latency. `--seconds 60` uses bounded buffers and reports steady-state underruns.
//! Worker drift ratio/fill/corrections differ from the helper's delivered-sample/QPC estimate.
//! Muted or loopback-suppressed endpoints may fail; never change their settings to obtain green.
//!
//! `windows_media_e2e --owned-test` creates/reaps its own SMTC player and generated tone,
//! relaying only to a local Dry discard Listener. Metadata must be absent before consent;
//! commands target only the helper's current exact token/revision. Ambiguity fails closed.
//! Both examples require explicit desktop permission and neither mutates a microphone or
//! synthesizes input. Ordinary microphone lease tests use Dry targets. Registration-only
//! tests reserve/release their documented chords, without proving real physical-key acceptance.
#![cfg(windows)]

mod backend;
mod backend_devices;
mod backend_media;
pub use backend::{WindowsBackend, WindowsStream};
mod common;
pub mod devices;
pub mod hotkey;
pub mod media;
pub mod render_config;
mod source_silence;
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

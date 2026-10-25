//! Windows platform adapter spike for Bifrost: local input capture
//! (KVM-010) and synthetic input injection (KVM-011).
//!
//! This crate is the E1 spike for the Windows side of the native adapter. It is
//! deliberately the only place in the workspace where Win32 FFI lives; the pure
//! pieces (USB HID tables, pressed-set ledger, injection planning) have no FFI at
//! all and are unit-testable on any host.
//!
//! # What is here
//!
//! * [`event`] — the Windows-specific part of the event vocabulary: the
//!   emergency combination in virtual-key space. The normalised vocabulary
//!   itself (`HidKey` / `InputEvent` / `PointerAbsolute` …), which maps 1:1 onto
//!   the `HidKey` / `PointerAbs` / `ButtonEv` / `ScrollEv` / `LocalEvent`
//!   messages of `outputs/ipc.proto`, lives in the shared `input-model` crate.
//!   Keys are physical USB HID usages (ADR-006): no text ever exists in this
//!   crate, so EN/RU/AltGr layouts are irrelevant to capture and injection and
//!   no keylogging payload can be produced by accident.
//! * [`hid`] — scancode set 1 (make codes) ⇄ USB HID keyboard/keypad usage IDs,
//!   full 104/105-key ISO/ANSI set including the `E0`-prefixed extended keys.
//! * [`ledger`] — the pressed-set ledger that backs "release everything we
//!   pressed" cleanup (AC-02/AC-03).
//! * [`inject_plan`] — pure planning and partial-failure accounting for a
//!   `SendInput` batch.
//! * [`capture`] (Windows only) — `WH_KEYBOARD_LL` / `WH_MOUSE_LL` capture on a
//!   dedicated thread with a message loop, plus Raw Input for mouse deltas.
//! * [`inject`] (Windows only) — `SendInput` injection, buttons, wheel, relative
//!   and virtual-desktop absolute positioning.
//!
//! # Design decisions taken by this spike
//!
//! ## Hooks vs Raw Input
//!
//! Both are used, because they answer different questions:
//!
//! * `WH_KEYBOARD_LL` / `WH_MOUSE_LL` are the only user-mode way to *suppress*
//!   input (`CallNextHookEx` is simply not called and the callback returns 1).
//!   They also carry the `LLKHF_INJECTED` / `LLMHF_INJECTED` flags and our
//!   `dwExtraInfo` marker, which is how we recognise our own injections.
//! * Raw Input (`RIDEV_INPUTSINK` on a message-only window) is where *mouse
//!   deltas* come from. A low-level mouse hook reports only screen coordinates;
//!   once we swallow motion those coordinates stop changing, so the hook cannot
//!   produce deltas at all. Raw Input additionally identifies the source device.
//!
//! The open question named by `outputs/03-architecture.md` is whether Raw Input
//! keeps delivering motion *while* the low-level hook swallows it. That is not
//! decided here: `examples/spike_capture` prints both counters side by side so
//! the answer is measured (see the manual steps in the spike output). The code is
//! written so that both answers work: if raw deltas stop arriving in Remote mode
//! the fallback is to compute deltas from hook coordinates accumulated *before*
//! suppression, which is a local change inside [`capture`].
//!
//! ## Cursor hold in Remote
//!
//! Windows has no `XGrabPointer` equivalent. The chosen mechanism is
//! **suppress motion in the low-level hook + re-assert a hold point from a timer**:
//!
//! * The mouse hook returns 1 for `WM_MOUSEMOVE` in Remote mode, so the physical
//!   mouse no longer moves the local cursor.
//! * A 50 ms `WM_TIMER` re-warps the cursor to the hold point captured when the
//!   mode switched, which catches *drift* from paths the hook does not own
//!   (other injectors, Raw Input clients, tablet drivers) and keeps the cursor
//!   from escaping to a screen edge.
//!
//! The warp is done from the timer and not from inside the hook callback on
//! purpose: `SetCursorPos` inside the hook generates a movement message that the
//! hook itself would see, which is exactly the feedback loop we are trying to
//! avoid. A side effect is that the warp is best-effort (up to one timer tick
//! late) rather than synchronous; for a software KVM that is fine, because the
//! cursor is not user-visible during Remote anyway.
//!
//! ## Emergency combo
//!
//! `Ctrl+Alt+Shift+F12` (per `[input].emergency_hotkey`, ≥3 modifiers + key) is
//! detected *inside* the keyboard hook from virtual-key state that the hook
//! maintains itself. On detection the shared mode is switched to `Observe`
//! directly — the flag is a plain atomic in this process, so no `lws-core`, no
//! IPC and no network is on that path — and a [`capture::CaptureEvent::Emergency`]
//! is queued for the agent. The triggering key down, repeats, and key up are swallowed,
//! including across a live binding replacement. AC-03's ≤100 ms budget is met by
//! construction: the mode flip is one store.
//!
//! ## Injected-event loop prevention
//!
//! Keyboard and mouse-button/wheel injections are tagged with
//! [`INJECT_TAG`] in `dwExtraInfo` and additionally carry the OS's injected flag.
//! The hooks drop them, so a local replay of a cancelled transition is never
//! re-captured and never re-sent. Raw Input carries no per-event marker (there is
//! no injected bit in `RAWMOUSE`), so motion is covered by [`synth`]: the
//! injector declares how many synthetic motion records it is about to produce and
//! the Raw Input handler consumes one budget token per motion event. This is an
//! explicit, documented approximation — the honest limitation of the platform.
//!
//! Recognising injections is also what makes receiver takeover mean "a human
//! touched this machine", so the drop happens before any mode logic. The
//! `CaptureConfig::capture_injected` escape hatch is development-only; even when
//! enabled, the `INJECT_TAG` used by this process is always dropped.
//!
//! ## DPI awareness
//!
//! `build.rs` embeds `dpiAwareness = PerMonitorV2` for MSVC-linked targets, which
//! is what `lws-agent` and `lws-gui` are. Linkers that cannot take MSVC manifest
//! switches (the GNU toolchain this crate was verified with) get the same effect
//! from [`dpi::ensure_per_monitor_v2`], called by both examples before any
//! coordinate is read. Measured output on Windows 10.0.26200: a process started
//! without a manifest reports `unaware`, the call succeeds, and the resulting
//! context compares equal to `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` via
//! `AreDpiAwarenessContextsEqual` (comparing the handle values directly does not
//! work — they are opaque, not the negative pseudo-values). Coordinates reported
//! by `GetCursorPos`/`GetSystemMetrics` are then real pixels, including the
//! negative origin of a monitor placed above the primary one.
//!
//! ## No sockets
//!
//! The agent invariant is that this process has no network sockets. This crate
//! links no socket API and has no dependency that could open one. (Note that
//! `ws2_32` appears in the import table of every Rust `std` Windows binary,
//! because `std`'s own `net` module links it; that is a property of the standard
//! library, not of this crate. A CI symbol check has to account for it, e.g. by
//! checking this crate's own object files.)
//!
//! # Non-Windows targets
//!
//! `capture`, `inject` and `dpi` are `#[cfg(windows)]`. On any other target only
//! the platform-neutral modules are compiled, so `cargo check --target
//! x86_64-unknown-linux-gnu` succeeds and the Linux workspace pull does not
//! inherit a single Win32 symbol.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod event;
pub mod game_guard;
pub mod hid;
pub mod inject_plan;
pub mod ledger;
pub mod synth;

#[cfg(windows)]
pub mod capture;
// Clipboard text and its secret markers: `AddClipboardFormatListener`, `CF_UNICODETEXT`
// and the three documented formats (KVM-041/KVM-043).
pub mod clipboard;
#[cfg(windows)]
pub mod displays;
#[cfg(windows)]
pub mod dpi;
#[cfg(windows)]
pub mod inject;
#[cfg(windows)]
pub mod strip;
#[cfg(windows)]
pub mod veil;

/// `dwExtraInfo` marker written into every `INPUT` record this process injects.
///
/// Chosen small enough for a 32-bit `usize` so the crate also builds for 32-bit
/// Windows, and unlikely to collide with the few well-known reserved values
/// (`0`, and `1`, which Windows itself uses to mark the fake shift that
/// `PrintScreen`/`Pause` synthesise).
pub const INJECT_TAG: usize = 0x4C57_5301; // "LWS\x01"

//! Synthetic input injection through `SendInput` (KVM-011).
//!
//! What the spike has to prove, and how it is done here:
//!
//! * **Keys** go out as scancodes (`KEYEVENTF_SCANCODE`, plus
//!   `KEYEVENTF_EXTENDEDKEY` for `E0` keys). Scancodes, not virtual keys, because
//!   ADR-006 puts physical key identity on the wire and because a scancode is
//!   interpreted with the receiver's own layout.
//! * **Absolute pointer** uses `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
//!   with coordinates normalised over the whole virtual desktop, so positions on
//!   monitors with negative origin work. This only behaves when the process is
//!   `PerMonitorV2`-aware (see [`crate::dpi`]).
//! * **Relative pointer** and **wheel** use the plain `MOUSEEVENTF_MOVE` /
//!   `WHEEL` / `HWHEEL` flags; wheel delta is already in 1/120 units, the same
//!   unit the protocol uses.
//! * **Partial failure** is read from the `SendInput` return value: the call is
//!   never retried, and [`InjectStatus`] reports exactly which events were not
//!   injected. UIPI/UAC blockage is surfaced as an explicit
//!   [`InjectErrorKind::AccessDenied`] rather than being swallowed.
//! * **Cleanup** ([`Injector::release_all`]) drains the pressed-set ledger into
//!   up-events, injects them once and re-arms the ledger with whatever could not
//!   be released, so nothing is silently forgotten (AC-02/AC-03).

use std::fmt;
use std::mem::size_of;

use windows::Win32::Foundation::{GetLastError, SetLastError, ERROR_ACCESS_DENIED, WIN32_ERROR};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::inject_plan::{self, Plan, Record, VirtualDesktop};
use crate::ledger::PressedLedger;
use crate::{synth, INJECT_TAG};
use input_model::{InputEvent, MouseButton};

/// What went wrong with an injection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectErrorKind {
    /// `SendInput` was refused by UIPI: the foreground window belongs to a
    /// higher-integrity process (an elevated/UAC window, for example).
    AccessDenied,
    /// Any other failure.
    Other,
}

/// A failure detail, always free of key, pointer or clipboard payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectError {
    /// Classification.
    pub kind: InjectErrorKind,
    /// Raw `GetLastError()` value, `0` if the API reported nothing.
    pub win32: u32,
    /// Human-readable explanation, safe to log.
    pub detail: String,
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            InjectErrorKind::AccessDenied => "ACCESS_DENIED",
            InjectErrorKind::Other => "OTHER",
        };
        write!(f, "{kind}({}) {}", self.win32, self.detail)
    }
}

/// Outcome of one `SendInput` call.
#[derive(Clone, Debug, PartialEq)]
pub struct InjectStatus {
    /// How many events were offered.
    pub requested: usize,
    /// How many events were actually injected (fully covered by accepted records).
    pub applied: usize,
    /// Events that were not injected, in input order.
    pub not_injected: Vec<InputEvent>,
    /// Records the OS accepted, out of [`InjectStatus::total_records`].
    pub accepted_records: usize,
    /// Records the batch expanded to.
    pub total_records: usize,
    /// Set when the batch was not fully accepted.
    pub error: Option<InjectError>,
}

impl InjectStatus {
    /// Whether everything the caller asked for was injected.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.not_injected.is_empty()
    }

    /// Whether nothing at all was injected.
    #[must_use]
    pub fn is_total_failure(&self) -> bool {
        self.applied == 0 && self.requested > 0
    }
}

impl fmt::Display for InjectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "injected {}/{} events ({} of {} INPUT records accepted), not injected: {}",
            self.applied,
            self.requested,
            self.accepted_records,
            self.total_records,
            self.not_injected.len()
        )?;
        if let Some(error) = &self.error {
            write!(f, "; error: {error}")?;
        }
        Ok(())
    }
}

/// Stateful injector: owns the pressed-set ledger.
#[derive(Clone, Debug, Default)]
pub struct Injector {
    ledger: PressedLedger,
}

impl Injector {
    /// A new injector with an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The pressed-set ledger, for inspection by the agent.
    #[must_use]
    pub fn ledger(&self) -> &PressedLedger {
        &self.ledger
    }

    /// The virtual desktop rectangle, in physical pixels.
    ///
    /// Requires PerMonitorV2 awareness for the values to be physical rather than
    /// virtualised at 150/200% scaling.
    #[must_use]
    pub fn virtual_desktop() -> VirtualDesktop {
        // SAFETY: `GetSystemMetrics` is a query with no preconditions.
        unsafe {
            VirtualDesktop {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }

    /// Lay out a batch exactly as [`Injector::inject`] would, without injecting.
    /// The dry run and the plan-only diagnostics use this.
    #[must_use]
    pub fn plan_for(events: &[InputEvent]) -> Plan {
        inject_plan::plan(events, Self::virtual_desktop())
    }

    /// Inject a batch with a single `SendInput` call.
    ///
    /// The call is made once, whatever the outcome: a partial failure is reported
    /// through [`InjectStatus`] and is never retried here (KVM-011).
    pub fn inject(&mut self, events: &[InputEvent]) -> InjectStatus {
        let plan = Self::plan_for(events);
        self.inject_planned(&plan, events)
    }

    /// Release everything the ledger says is held.
    ///
    /// The batch is built from the ledger, injected once, and any release that was
    /// not injected is put back into the ledger so a later cleanup still knows
    /// about it. The ledger is the mechanism that keeps AC-02/AC-03 honest.
    pub fn release_all(&mut self) -> InjectStatus {
        let batch = self.ledger.take_all();
        if batch.is_empty() {
            return InjectStatus {
                requested: 0,
                applied: 0,
                not_injected: Vec::new(),
                accepted_records: 0,
                total_records: 0,
                error: None,
            };
        }

        let events = batch.to_events();
        let status = self.inject(&events);

        for event in &status.not_injected {
            match *event {
                InputEvent::Key { key, state } if state.is_up() => self.ledger.mark_held(key),
                InputEvent::Button { button, state } if state.is_up() => {
                    self.ledger.mark_held_button(button);
                }
                _ => {}
            }
        }

        status
    }

    fn inject_planned(&mut self, plan: &Plan, events: &[InputEvent]) -> InjectStatus {
        let inputs: Vec<INPUT> = plan.records.iter().copied().map(record_to_input).collect();
        let total_records = inputs.len();

        if total_records == 0 {
            return InjectStatus {
                requested: events.len(),
                applied: 0,
                not_injected: events.to_vec(),
                accepted_records: 0,
                total_records: 0,
                error: None,
            };
        }

        let declared_motions = plan.motion_count();
        synth::expect_motions(declared_motions);

        // SAFETY: `SendInput` takes a slice of fully initialised `INPUT` records
        // and their byte size; every record is tagged with `INJECT_TAG` so our own
        // hooks can recognise it. `SetLastError(0)` makes the error below
        // meaningful even when the API does not touch it.
        let accepted = unsafe {
            SetLastError(WIN32_ERROR(0));
            SendInput(&inputs, size_of::<INPUT>() as i32) as usize
        };

        // Do not leave motion tokens behind for events that were never injected.
        synth::expect_motions(motion_budget_adjustment(plan, accepted, declared_motions));

        let error = if accepted < total_records {
            // SAFETY: read-only query of the calling thread's last error.
            let raw = unsafe { GetLastError() };
            Some(classify(raw))
        } else {
            None
        };

        let (applied, missing) = inject_plan::account(plan, accepted);

        let mut not_injected = Vec::with_capacity(missing.len());
        let mut is_missing = vec![false; events.len()];
        for &index in &missing {
            is_missing[index] = true;
            not_injected.push(events[index]);
        }

        // Only state that really went out is recorded as pressed.
        for (index, event) in events.iter().enumerate() {
            if !is_missing[index] {
                self.ledger.note(event);
            }
        }

        InjectStatus {
            requested: events.len(),
            applied,
            not_injected,
            accepted_records: accepted,
            total_records,
            error,
        }
    }
}

fn motion_budget_adjustment(plan: &Plan, accepted: usize, declared_motions: i32) -> i32 {
    let emitted_motions = plan
        .records
        .iter()
        .take(accepted)
        .filter(|record| matches!(record, Record::MoveRel { .. } | Record::MoveAbsNorm { .. }))
        .count();
    i32::try_from(emitted_motions).unwrap_or(i32::MAX) - declared_motions
}

fn classify(raw: WIN32_ERROR) -> InjectError {
    if raw == ERROR_ACCESS_DENIED {
        InjectError {
            kind: InjectErrorKind::AccessDenied,
            win32: raw.0,
            detail: "SendInput refused by UIPI: the foreground window belongs to a \
                     higher-integrity process (elevated/UAC). Injection into it is not \
                     possible without matching integrity."
                .to_owned(),
        }
    } else {
        InjectError {
            kind: InjectErrorKind::Other,
            win32: raw.0,
            detail: "SendInput did not accept the whole batch. For a UIPI block Windows is \
                     allowed to report no error code at all, so a zero here still means the \
                     batch may have been refused rather than malformed."
                .to_owned(),
        }
    }
}

fn button_flags(button: MouseButton, up: bool) -> MOUSE_EVENT_FLAGS {
    match (button, up) {
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEUP,
        (MouseButton::Back | MouseButton::Forward, false) => MOUSEEVENTF_XDOWN,
        (MouseButton::Back | MouseButton::Forward, true) => MOUSEEVENTF_XUP,
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn record_to_input(record: Record) -> INPUT {
    match record {
        Record::Key { scan, extended, up } => {
            let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if up {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        // Scancode path: no virtual key is involved, which is what
                        // makes the injected key follow the receiver's layout.
                        wVk: VIRTUAL_KEY(0),
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECT_TAG,
                    },
                },
            }
        }
        Record::Button { button, up } => mouse_input(
            0,
            0,
            button_flags(button, up),
            button.side_index().unwrap_or(0),
        ),
        Record::MoveRel { dx, dy } => mouse_input(dx, dy, MOUSEEVENTF_MOVE, 0),
        Record::MoveAbsNorm { x, y } => mouse_input(
            i32::from(x),
            i32::from(y),
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
        ),
        Record::Wheel { vertical, delta120 } => mouse_input(
            0,
            0,
            if vertical {
                MOUSEEVENTF_WHEEL
            } else {
                MOUSEEVENTF_HWHEEL
            },
            delta120 as u32,
        ),
    }
}

/// One-line description of a record, for `--dry-run` output and diagnostics.
///
/// Deliberately reports scancode/usage identifiers only: there is no text, no
/// virtual key and no character in a `Record` that could leak.
#[must_use]
pub fn describe_record(record: Record) -> String {
    match record {
        Record::Key { scan, extended, up } => format!(
            "KEY scan=0x{scan:02X}{} {}",
            if extended { " ext" } else { "" },
            if up { "up  " } else { "down" }
        ),
        Record::Button { button, up } => {
            format!("BTN {button:?} {}", if up { "up" } else { "down" })
        }
        Record::MoveRel { dx, dy } => format!("MOV rel dx={dx} dy={dy}"),
        Record::MoveAbsNorm { x, y } => format!("MOV abs nx={x} ny={y} (of 65535)"),
        Record::Wheel { vertical, delta120 } => format!(
            "WHEEL {} delta120={delta120}",
            if vertical { "vertical" } else { "horizontal" }
        ),
    }
}

/// One-line description of a normalised event, for diagnostics.
///
/// Same rule as [`describe_record`]: identifiers, never text.
#[must_use]
pub fn describe_event(event: &InputEvent) -> String {
    match *event {
        InputEvent::Key { key, state } => format!(
            "KEY page=0x{:04X} usage=0x{:04X} {}",
            key.page,
            key.usage,
            if state.is_down() { "down" } else { "up" }
        ),
        InputEvent::Button { button, state } => {
            format!(
                "BTN {button:?} {}",
                if state.is_down() { "down" } else { "up" }
            )
        }
        InputEvent::PointerRelative { dx, dy } => format!("MOV rel dx={dx} dy={dy}"),
        InputEvent::PointerAbsolute(position) => {
            format!("MOV abs x={} y={}", position.x, position.y)
        }
        InputEvent::Scroll { h120, v120 } => format!("WHEEL h120={h120} v120={v120}"),
    }
}

/// The OS autorepeat settings, as the two indices `SystemParametersInfo` speaks.
///
/// `(delay_index, speed_index)`: the delay is 0..=3 (250 ms steps) and the speed 0..=31
/// (2.5..30 repeats per second). The receiver's software repeat engine is the only thing on
/// Windows that repeats a held injected key at all — a `SendInput` key-down without an up
/// produces no further down of its own (measured; see `lws-agent`'s `repeat` module) — so the
/// rate has to come from here to look like the keyboard the user configured (KVM-036, AC-14).
///
/// A failure returns the Windows defaults (1, 31): 500 ms and the fastest rate.
#[cfg(windows)]
#[must_use]
pub fn keyboard_timing() -> (u32, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    fn query(
        action: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION,
    ) -> Option<u32> {
        let mut value: u32 = 0;
        // SAFETY: `pvParam` points at a live `u32`; both actions are documented as
        // `uiParam = 0`, an integer output and no update flag.
        let ok = unsafe {
            SystemParametersInfoW(
                action,
                0,
                Some(std::ptr::addr_of_mut!(value).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        ok.ok().map(|()| value)
    }

    let delay = query(SPI_GETKEYBOARDDELAY);
    let speed = query(SPI_GETKEYBOARDSPEED);
    match (delay, speed) {
        (Some(delay), Some(speed)) if (0..=3).contains(&delay) && (0..=31).contains(&speed) => {
            (delay, speed)
        }
        _ => (1, 31),
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, motion_budget_adjustment, InjectErrorKind, Plan, Record};
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, WIN32_ERROR};

    #[test]
    fn rejected_motion_budget_is_retracted_for_every_accepted_count() {
        let plan = Plan {
            records: vec![
                Record::Key {
                    scan: 0x1e,
                    extended: false,
                    up: false,
                },
                Record::MoveRel { dx: 1, dy: 2 },
                Record::Wheel {
                    vertical: true,
                    delta120: 120,
                },
                Record::MoveAbsNorm { x: 123, y: 456 },
                Record::Button {
                    button: input_model::MouseButton::Left,
                    up: false,
                },
                Record::MoveRel { dx: -1, dy: -2 },
            ],
            ..Plan::default()
        };
        let declared = plan.motion_count();
        let expected_emitted = [0, 0, 1, 1, 2, 2, 3];
        for (accepted, emitted) in expected_emitted.into_iter().enumerate() {
            let correction = motion_budget_adjustment(&plan, accepted, declared);
            assert_eq!(declared + correction, emitted, "accepted {accepted} records");
            // Capture may have consumed accepted motions before SendInput returns.
            assert_eq!(declared - emitted + correction, 0);
        }
        let empty = Plan::default();
        assert_eq!(motion_budget_adjustment(&empty, 0, 0), 0);
    }

    #[test]
    fn access_denied_is_classified_and_raw_error_codes_are_preserved() {
        let denied = classify(ERROR_ACCESS_DENIED);
        assert_eq!(denied.kind, InjectErrorKind::AccessDenied);
        assert_eq!(denied.win32, 5);
        // Windows may leave GetLastError at zero even when injection failed.
        for raw in [0, 1, 6, 87, u32::MAX] {
            let error = classify(WIN32_ERROR(raw));
            assert_eq!(error.kind, InjectErrorKind::Other);
            assert_eq!(error.win32, raw);
        }
    }
}

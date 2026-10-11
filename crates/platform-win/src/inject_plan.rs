//! Pure planning and accounting for an injection batch.
//!
//! Everything here is arithmetic and data: no Win32 call, no allocation beyond
//! the `Vec`s the plan itself is made of. That is on purpose — KVM-011's
//! definition of done ("a partial failure must not repeat the whole batch but
//! must report which events were not injected") is a property of this module and
//! can be checked without a desktop session.
//!
//! Shape of the pipeline:
//!
//! 1. [`plan`] turns normalised events into a flat list of [`Record`]s. One event
//!    can produce zero records (a key with no scancode), one, or two (a diagonal
//!    wheel event becomes a vertical and a horizontal record).
//! 2. The platform layer turns the records into `INPUT` and calls `SendInput`
//!    once with the whole list — one syscall, one accept count.
//! 3. [`account`] maps the accepted *record* count back to *events*, which is the
//!    only accounting that is meaningful to the caller: `applied` events plus the
//!    list of events that were not injected, in input order.

use input_model::{InputEvent, MouseButton};

/// The Windows virtual desktop, in physical pixels. Negative origin is normal
/// when a monitor sits left of or above the primary one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualDesktop {
    /// Left edge of the virtual desktop.
    pub left: i32,
    /// Top edge of the virtual desktop.
    pub top: i32,
    /// Total width in pixels.
    pub width: i32,
    /// Total height in pixels.
    pub height: i32,
}

impl VirtualDesktop {
    /// Map a virtual-desktop pixel position onto `SendInput`'s `MOUSEEVENTF_ABSOLUTE`
    /// range (`0..=65535`, inclusive at both ends).
    ///
    /// The divisor is `extent - 1` so that both edges land exactly on the ends of
    /// the range; with `extent` the rightmost pixel would never be reachable.
    /// Degenerate extents (a headless session) map to the centre of the range
    /// rather than dividing by zero.
    #[must_use]
    pub fn normalize(&self, x: i32, y: i32) -> (u16, u16) {
        let norm = |value: i32, origin: i32, extent: i32| -> u16 {
            if extent <= 1 {
                return 0x7FFF;
            }
            let relative = i64::from(value) - i64::from(origin);
            let scaled = relative * 65535 / (i64::from(extent) - 1);
            scaled.clamp(0, 65535) as u16
        };
        (
            norm(x, self.left, self.width),
            norm(y, self.top, self.height),
        )
    }
}

/// One `INPUT` record, described portably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// `KEYEVENTF_SCANCODE` key event.
    Key {
        /// Make code, low byte.
        scan: u16,
        /// Whether `KEYEVENTF_EXTENDEDKEY` is required (`E0`-prefixed key).
        extended: bool,
        /// Key-up rather than key-down.
        up: bool,
    },
    /// Mouse button event.
    Button {
        /// Which button.
        button: MouseButton,
        /// Release rather than press.
        up: bool,
    },
    /// `MOUSEEVENTF_MOVE` with relative deltas.
    MoveRel {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
    },
    /// `MOUSEEVENTF_MOVE | ABSOLUTE | VIRTUALDESK`.
    MoveAbsNorm {
        /// Normalised horizontal position, `0..=65535`.
        x: u16,
        /// Normalised vertical position, `0..=65535`.
        y: u16,
    },
    /// Wheel event; exactly one axis per record.
    Wheel {
        /// `true` for the vertical wheel (`MOUSEEVENTF_WHEEL`), `false` for the
        /// horizontal one (`MOUSEEVENTF_HWHEEL`).
        vertical: bool,
        /// Delta in 1/120 units.
        delta120: i32,
    },
}

/// A batch laid out for one `SendInput` call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Records in call order.
    pub records: Vec<Record>,
    /// Source event index of every record, same length as `records`.
    pub owner: Vec<usize>,
    /// Records produced per source event index, same length as the input slice.
    /// Zero means the event was not representable (for example a key with no
    /// scancode) and can never be injected.
    pub per_event: Vec<u8>,
}

impl Plan {
    /// Append one record owned by source event `index`.
    fn push(&mut self, index: usize, record: Record) {
        self.records.push(record);
        self.owner.push(index);
    }

    /// Total number of records.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// Number of source events that produced no record.
    #[must_use]
    pub fn unmapped_count(&self) -> usize {
        self.per_event.iter().filter(|&&n| n == 0).count()
    }

    /// Motion records in the batch. The caller feeds this to
    /// [`crate::synth::expect_motions`] before injecting.
    #[must_use]
    pub fn motion_count(&self) -> i32 {
        let n = self
            .records
            .iter()
            .filter(|record| matches!(record, Record::MoveRel { .. } | Record::MoveAbsNorm { .. }))
            .count();
        i32::try_from(n).unwrap_or(i32::MAX)
    }
}

/// Lay out a batch. Never fails; unrepresentable events end up with zero records.
#[must_use]
pub fn plan(events: &[InputEvent], vd: VirtualDesktop) -> Plan {
    let mut plan = Plan {
        records: Vec::with_capacity(events.len()),
        owner: Vec::with_capacity(events.len()),
        per_event: Vec::with_capacity(events.len()),
    };

    for (index, event) in events.iter().enumerate() {
        let before = plan.records.len();
        match *event {
            InputEvent::Key { key, state } => {
                if let Some(scancode) = crate::hid::hid_key_to_scancode(key) {
                    let (scan, extended) = crate::hid::scancode_parts(scancode);
                    plan.push(
                        index,
                        Record::Key {
                            scan,
                            extended,
                            up: state.is_up(),
                        },
                    );
                }
            }
            InputEvent::Button { button, state } => {
                plan.push(
                    index,
                    Record::Button {
                        button,
                        up: state.is_up(),
                    },
                );
            }
            InputEvent::PointerRelative { dx, dy } => {
                plan.push(index, Record::MoveRel { dx, dy });
            }
            InputEvent::PointerAbsolute(position) => {
                let (nx, ny) = vd.normalize(position.x.round() as i32, position.y.round() as i32);
                plan.push(index, Record::MoveAbsNorm { x: nx, y: ny });
            }
            InputEvent::Scroll { h120, v120 } => {
                if v120 != 0 {
                    plan.push(
                        index,
                        Record::Wheel {
                            vertical: true,
                            delta120: v120,
                        },
                    );
                }
                if h120 != 0 {
                    plan.push(
                        index,
                        Record::Wheel {
                            vertical: false,
                            delta120: h120,
                        },
                    );
                }
            }
        }
        let produced = (plan.records.len() - before) as u8;
        plan.per_event.push(produced);
    }

    plan
}

/// Map the number of records the OS accepted back onto source events.
///
/// Returns `(applied_events, missing_event_indices)`. `applied_events` counts
/// events whose records were *all* accepted, so an event is only reported as
/// injected when it really was; everything else — including events that have no
/// record at all — is listed in `missing_event_indices`, in input order. The
/// invariant `applied_events + missing.len() == events.len()` holds.
#[must_use]
pub fn account(plan: &Plan, accepted_records: usize) -> (usize, Vec<usize>) {
    let accepted = accepted_records.min(plan.records.len());
    let mut applied = 0usize;
    let mut consumed = 0usize;
    let mut missing = Vec::new();

    for (index, &count) in plan.per_event.iter().enumerate() {
        let count = usize::from(count);
        if count == 0 {
            missing.push(index);
        } else if consumed + count <= accepted {
            applied += 1;
            consumed += count;
        } else {
            // Records are laid out in event order, so once one event does not fit
            // none of the later ones can either.
            missing.push(index);
        }
    }

    (applied, missing)
}

//! Pressed-set ledger.
//!
//! The ledger is the core safety mechanism for AC-02/AC-03/AC-06: it records
//! exactly which keys and buttons *this session* has pressed down, so cleanup can
//! release them and nothing else. The receiver keeps one for its synthetic
//! presses; the source keeps one for the physical pressed set (a normal
//! transition is only allowed with an empty physical pressed set).
//!
//! It is a plain set with no OS dependency: capture and injection both feed it,
//! cleanup drains it.

use std::collections::BTreeSet;

use input_model::{HidKey, InputEvent, KeyState, MouseButton};

/// Everything a session is holding down.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PressedLedger {
    keys: BTreeSet<HidKey>,
    buttons: BTreeSet<MouseButton>,
}

impl PressedLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event to the ledger.
    ///
    /// Only `Key` and `Button` events change anything; pointer and wheel events
    /// are ignored. Re-pressing something already pressed is idempotent (Windows
    /// repeats `WM_KEYDOWN` with autorepeat), and releasing something that was
    /// never pressed is ignored — the ledger only ever describes *our* presses.
    pub fn note(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { key, state } => {
                if state.is_down() {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
            }
            InputEvent::Button { button, state } => {
                if state.is_down() {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            InputEvent::PointerRelative { .. }
            | InputEvent::PointerAbsolute(_)
            | InputEvent::Scroll { .. } => {}
        }
    }

    /// Record a key as held without a key-down event (used to restore state that
    /// a partial release could not undo).
    pub fn mark_held(&mut self, key: HidKey) {
        self.keys.insert(key);
    }

    /// Record a button as held without a button-down event.
    pub fn mark_held_button(&mut self, button: MouseButton) {
        self.buttons.insert(button);
    }

    /// Whether any key or button is currently held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Number of held keys. Intended for logs and counters.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Number of held buttons. Intended for logs and counters.
    #[must_use]
    pub fn button_count(&self) -> usize {
        self.buttons.len()
    }

    /// Whether the key is held.
    #[must_use]
    pub fn holds_key(&self, key: HidKey) -> bool {
        self.keys.contains(&key)
    }

    /// Whether the button is held.
    #[must_use]
    pub fn holds_button(&self, button: MouseButton) -> bool {
        self.buttons.contains(&button)
    }

    /// Take everything out of the ledger into a release batch, leaving it empty.
    pub fn take_all(&mut self) -> ReleaseBatch {
        ReleaseBatch {
            keys: std::mem::take(&mut self.keys).into_iter().collect(),
            buttons: std::mem::take(&mut self.buttons).into_iter().collect(),
        }
    }
}

/// The keys and buttons a cleanup has to release, in a deterministic order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseBatch {
    /// Keys to release, sorted by usage page/usage.
    pub keys: Vec<HidKey>,
    /// Buttons to release, sorted by button id.
    pub buttons: Vec<MouseButton>,
}

impl ReleaseBatch {
    /// Nothing to release.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Total number of transitions in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len() + self.buttons.len()
    }

    /// The equivalent up-events, keys first, then buttons.
    #[must_use]
    pub fn to_events(&self) -> Vec<InputEvent> {
        let mut events = Vec::with_capacity(self.len());
        events.extend(self.keys.iter().map(|&key| InputEvent::Key {
            key,
            state: KeyState::Up,
        }));
        events.extend(self.buttons.iter().map(|&button| InputEvent::Button {
            button,
            state: KeyState::Up,
        }));
        events
    }
}

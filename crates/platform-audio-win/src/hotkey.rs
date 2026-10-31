//! Opt-in global push-to-talk observation, without hooks or input synthesis.
//!
//! A private message-only window receives RegisterHotKey presses. While pressed,
//! the worker polls physical key/modifier high bits every 10 ms. Consumers MUST
//! check the absolute expiry on every use, not extend a lease on receipt: queued
//! events, worker stalls, shutdown and lost releases must all close the gate.

use crate::Error;
use audio_backend::PttReleaseReason;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::HWND,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const POLL: Duration = Duration::from_millis(10);
const RENEW: Duration = Duration::from_millis(100);
/// Maximum permitted age of a physical press observation.
pub const PRESS_LEASE: Duration = Duration::from_millis(250);
const HOTKEY_ID: i32 = 1;
const MODIFIER_MASK: u32 = 0x0f;

/// Windows VK plus the MOD_ALT/CONTROL/SHIFT/WIN bit mask (no side-specific keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub virtual_key: u16,
    pub modifiers: u32,
}
impl HotkeyBinding {
    pub fn validate(self, kvm_bindings: &[Self]) -> Result<(), Error> {
        if !(1..=254).contains(&self.virtual_key) || self.modifiers & !MODIFIER_MASK != 0 {
            return Err(Error::InvalidArgument("invalid hotkey VK or modifier mask"));
        }
        if matches!(self.virtual_key, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5) {
            return Err(Error::InvalidArgument("hotkey requires a non-modifier key"));
        }
        if self.conflicts(kvm_bindings) {
            return Err(Error::InvalidArgument(
                "hotkey conflicts with configured KVM binding",
            ));
        }
        Ok(())
    }

    /// Extra modifiers also conflict: KVM observers may allow modifier supersets.
    pub fn conflicts(self, kvm_bindings: &[Self]) -> bool {
        kvm_bindings.iter().any(|other| {
            self.virtual_key == other.virtual_key
                && ((self.modifiers & other.modifiers) == self.modifiers
                    || (self.modifiers & other.modifiers) == other.modifiers)
        })
    }

    fn physically_down(self) -> bool {
        let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) < 0 };
        if !down(self.virtual_key) {
            return false;
        }
        for (mask, vk) in [(1, VK_MENU.0), (2, VK_CONTROL.0), (4, VK_SHIFT.0)] {
            if self.modifiers & mask != 0 && !down(vk) {
                return false;
            }
        }
        self.modifiers & 8 == 0 || down(VK_LWIN.0) || down(VK_RWIN.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HotkeyEvent {
    /// Initial press or renewal; this absolute deadline never moves on receipt.
    PressLease {
        observed_at: Instant,
        expires_at: Instant,
        initial: bool,
    },
    Released {
        reason: PttReleaseReason,
    },
}
impl HotkeyEvent {
    pub fn is_live(self, now: Instant) -> bool {
        matches!(self, Self::PressLease { observed_at, expires_at, .. }
            if now >= observed_at && now < expires_at && expires_at <= observed_at + PRESS_LEASE)
    }
}

/// Thread-owned registration. Drop unregisters, destroys the window and joins.
pub struct Hotkey {
    events: Receiver<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Hotkey {
    /// All configured KVM bindings, including emergency release, must be supplied.
    /// Windows registration failures (including other applications) are returned.
    pub fn register(binding: HotkeyBinding, kvm_bindings: &[HotkeyBinding]) -> Result<Self, Error> {
        binding.validate(kvm_bindings)?;
        let (tx, events) = mpsc::sync_channel(16);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let window = match MessageWindow::open(binding) {
                Ok(window) => window,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            // A replacement registration cannot inherit permission from an already-held chord.
            let mut press = PressState {
                blocked: true,
                ..PressState::default()
            };
            let mut message = MSG::default();
            while !stopped.load(Ordering::Acquire) {
                // Bound each drain so a message flood cannot starve release polling.
                for _ in 0..64 {
                    if !unsafe { PeekMessageW(&mut message, Some(window.0), 0, 0, PM_REMOVE) }
                        .as_bool()
                    {
                        break;
                    }
                    if message.message == WM_HOTKEY && message.wParam.0 == HOTKEY_ID as usize {
                        press.activate(Instant::now());
                    } else {
                        unsafe {
                            DispatchMessageW(&message);
                        }
                    }
                }
                if press.pressed || press.blocked {
                    press.poll(Instant::now(), binding.physically_down(), &tx);
                }
                thread::sleep(POLL);
            }
            let _ = tx.try_send(HotkeyEvent::Released {
                reason: PttReleaseReason::UnavailableOrReset,
            });
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(Error::WorkerStopped)
            }
        }
    }
    pub fn events(&self) -> &Receiver<HotkeyEvent> {
        &self.events
    }
    pub fn shutdown(mut self) {
        self.close();
    }
    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Hotkey {
    fn drop(&mut self) {
        self.close();
    }
}

struct MessageWindow(HWND);
impl MessageWindow {
    fn open(binding: HotkeyBinding) -> Result<Self, Error> {
        // The built-in STATIC class avoids global class registration/callback state.
        let window = Self(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )?
        });
        unsafe {
            RegisterHotKey(
                Some(window.0),
                HOTKEY_ID,
                HOT_KEY_MODIFIERS(binding.modifiers) | MOD_NOREPEAT,
                u32::from(binding.virtual_key),
            )?;
        }
        Ok(window)
    }
}
impl Drop for MessageWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.0), HOTKEY_ID);
            let _ = DestroyWindow(self.0);
        }
    }
}

#[derive(Default)]
struct PressState {
    pressed: bool,
    blocked: bool,
    next_renewal: Option<Instant>,
    last_observation: Option<Instant>,
    announced: bool,
}
impl PressState {
    fn activate(&mut self, now: Instant) {
        if !self.pressed && !self.blocked {
            self.pressed = true;
            self.next_renewal = Some(now);
            self.last_observation = Some(now);
        }
    }
    fn poll(&mut self, now: Instant, down: bool, tx: &SyncSender<HotkeyEvent>) {
        if !down {
            if self.pressed || self.blocked {
                let _ = tx.try_send(HotkeyEvent::Released {
                    reason: PttReleaseReason::PhysicalKeyUp,
                });
            }
            *self = Self::default();
            return;
        }
        // A stalled worker must not reopen the gate without a new physical press.
        if self
            .last_observation
            .is_some_and(|last| now.duration_since(last) >= PRESS_LEASE)
        {
            self.pressed = false;
            self.blocked = true;
            let _ = tx.try_send(HotkeyEvent::Released {
                reason: PttReleaseReason::LeaseExpired,
            });
        }
        if !self.pressed || self.blocked {
            return;
        }
        self.last_observation = Some(now);
        if self.next_renewal.is_some_and(|next| now >= next) {
            if tx
                .try_send(HotkeyEvent::PressLease {
                    observed_at: now,
                    expires_at: now + PRESS_LEASE,
                    initial: !self.announced,
                })
                .is_err()
            {
                // Queue congestion is loss of lease, not permission to keep talking.
                self.pressed = false;
                self.blocked = true;
                let _ = tx.try_send(HotkeyEvent::Released {
                    reason: PttReleaseReason::UnavailableOrReset,
                });
            } else {
                self.announced = true;
                self.next_renewal = Some(now + RENEW);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_kvm_conflicts_and_invalid_bindings() {
        let kvm = HotkeyBinding {
            virtual_key: 0x70,
            modifiers: 3,
        };
        assert!(kvm.validate(&[kvm]).is_err());
        assert!(HotkeyBinding {
            modifiers: 7,
            ..kvm
        }
        .validate(&[kvm])
        .is_err());
        assert!(HotkeyBinding {
            virtual_key: 0x71,
            ..kvm
        }
        .validate(&[kvm])
        .is_ok());
        assert!(HotkeyBinding {
            virtual_key: 0,
            ..kvm
        }
        .validate(&[])
        .is_err());
        assert!(HotkeyBinding {
            modifiers: 0x4000,
            ..kvm
        }
        .validate(&[])
        .is_err());
        assert!(HotkeyBinding {
            virtual_key: VK_SHIFT.0,
            ..kvm
        }
        .validate(&[])
        .is_err());
    }
    #[test]
    fn registration_waits_for_observed_key_up_before_admitting_a_press() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut state = PressState {
            blocked: true,
            ..PressState::default()
        };
        state.activate(now);
        state.poll(now, true, &tx);
        state.activate(now + POLL);
        state.poll(now + POLL, true, &tx);
        assert!(state.blocked && !state.pressed);
        assert!(rx.try_recv().is_err());

        state.poll(now + POLL * 2, false, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp,
            }
        ));
        assert!(!state.blocked && !state.pressed);

        let observed_at = now + POLL * 3;
        state.activate(observed_at);
        state.poll(observed_at, true, &tx);
        let lease = rx.try_recv().unwrap();
        assert!(lease.is_live(observed_at));
        assert!(matches!(
            lease,
            HotkeyEvent::PressLease { initial: true, .. }
        ));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn physical_key_up_is_distinct_from_lease_expiry() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut state = PressState::default();
        state.activate(now);
        state.poll(now, true, &tx);
        assert!(rx.try_recv().unwrap().is_live(now));
        state.poll(now + POLL, false, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp,
            }
        ));
        assert!(!state.pressed && !state.blocked);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn leases_expire_at_observation_not_receipt() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut state = PressState::default();
        state.activate(now);
        state.poll(now, true, &tx);
        let lease = rx.try_recv().unwrap();
        assert!(lease.is_live(now));
        assert!(!lease.is_live(now + PRESS_LEASE));
        state.poll(now + PRESS_LEASE, true, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::LeaseExpired,
            }
        ));
        state.activate(now + PRESS_LEASE);
        assert!(!state.pressed);
        state.poll(now + PRESS_LEASE, false, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp,
            }
        ));
        state.activate(now + PRESS_LEASE);
        assert!(state.pressed);
    }
    #[test]
    fn overflow_blocks_until_release() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut state = PressState::default();
        state.activate(now);
        state.poll(now, true, &tx);
        state.poll(now + RENEW, true, &tx);
        assert!(state.blocked && !state.pressed);
        assert!(!rx.try_recv().unwrap().is_live(now + PRESS_LEASE));
        assert!(rx.try_recv().is_err());
        state.poll(now + RENEW, false, &tx);
        assert!(!state.blocked);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp,
            }
        ));
    }
}

//! Opt-in global push-to-talk observation, without input synthesis or suppression.
//!
//! RegisterHotKey reserves the chord; a narrowly filtered, non-suppressing
//! keyboard observer confirms exact physical scan/E0 identity on both edges.
//! Polling can revoke a lease, but unavailable/zero key state never proves release.
//! Registration starts unarmed: a physical press/release may be needed to arm it.
//! Consumers MUST check absolute expiry; receipt never refreshes an initial edge.

use crate::Error;
use audio_backend::PttReleaseReason;
use std::{
    cell::Cell,
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
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::SystemInformation::{GetSystemTimeAdjustment, GetTickCount},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const POLL: Duration = Duration::from_millis(10);
const RENEW: Duration = Duration::from_millis(100);
/// Maximum permitted age of a physical press observation.
pub const PRESS_LEASE: Duration = Duration::from_millis(250);
const HOTKEY_ID: i32 = 1;
const MODIFIER_MASK: u32 = 0x0f;

/// Physical set-1 make code (including E0) plus MOD_ALT/CONTROL/SHIFT/WIN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub scan_code: u16,
    pub modifiers: u32,
}
impl HotkeyBinding {
    pub fn from_hid(usage: u16, modifiers: u32) -> Result<Self, Error> {
        let scan_code = platform_win::hid::hid_to_scancode(usage).ok_or(Error::Unsupported(
            "physical hotkey scan identity unavailable",
        ))?;
        let binding = Self {
            scan_code,
            modifiers,
        };
        binding.validate(&[])?;
        Ok(binding)
    }

    pub fn validate(self, kvm_bindings: &[Self]) -> Result<(), Error> {
        let usage = platform_win::hid::scancode_to_hid(self.scan_code).ok_or(
            Error::Unsupported("physical hotkey scan identity unavailable"),
        )?;
        if self.modifiers & !MODIFIER_MASK != 0 {
            return Err(Error::InvalidArgument("invalid hotkey modifier mask"));
        }
        if matches!(usage, 0xe0..=0xe7) {
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
            self.scan_code == other.scan_code
                && ((self.modifiers & other.modifiers) == self.modifiers
                    || (self.modifiers & other.modifiers) == other.modifiers)
        })
    }

    fn modifiers_down(self) -> bool {
        let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) < 0 };
        for (mask, vk) in [(1, VK_MENU.0), (2, VK_CONTROL.0), (4, VK_SHIFT.0)] {
            if self.modifiers & mask != 0 && !down(vk) {
                return false;
            }
        }
        self.modifiers & 8 == 0 || down(VK_LWIN.0) || down(VK_RWIN.0)
    }

    fn registration_vk(self) -> Result<u32, Error> {
        // VK is only a reservation, never evidence of the configured physical edge.
        let foreground = unsafe { GetForegroundWindow() };
        let thread = unsafe { GetWindowThreadProcessId(foreground, None) };
        let layout = unsafe { GetKeyboardLayout(thread) };
        let vk = unsafe {
            MapVirtualKeyExW(u32::from(self.scan_code), MAPVK_VSC_TO_VK_EX, Some(layout))
        };
        if (1..=254).contains(&vk) {
            Ok(vk)
        } else {
            Err(Error::Unsupported(
                "physical hotkey has no native registration mapping",
            ))
        }
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
            let _physical_hook = match PhysicalHook::open(binding) {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let tick_uncertainty = match tick_uncertainty() {
                Ok(value) => value,
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
                    let available =
                        unsafe { PeekMessageW(&mut message, Some(window.0), 0, 0, PM_REMOVE) }
                            .as_bool();
                    if let Some(observation) = PhysicalHook::take_edges() {
                        if observation.release_pending {
                            press.observe_release(&tx);
                        }
                        if let Some(message_ticks) = observation.pressed_at {
                            let now = Instant::now();
                            if let Some(observed) = message_observed_at(
                                now,
                                unsafe { GetTickCount() },
                                message_ticks,
                                tick_uncertainty,
                            ) {
                                press.activate(observed);
                            }
                        }
                    }
                    press.flush_release(&tx);
                    if !available {
                        break;
                    }
                    // WM_HOTKEY has only a VK, so it can never authorize a physical press.
                    if message.message != WM_HOTKEY {
                        unsafe {
                            DispatchMessageW(&message);
                        }
                    }
                }
                if press.pressed || press.blocked {
                    press.poll(Instant::now(), PhysicalHook::physically_down(), &tx);
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
        let virtual_key = binding.registration_vk()?;
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
                virtual_key,
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

/// MSG.time/GetTickCount wrap together. Round an entire timer tick backwards:
/// neither queue delay nor coarse tick quantization may freshen a pre-fence edge.
fn message_observed_at(
    now: Instant,
    ticks: u32,
    message_ticks: u32,
    uncertainty: Duration,
) -> Option<Instant> {
    let age = Duration::from_millis(u64::from(ticks.wrapping_sub(message_ticks)))
        .checked_add(uncertainty)?;
    if age >= PRESS_LEASE {
        return None;
    }
    now.checked_sub(age)
}

fn tick_uncertainty() -> Result<Duration, Error> {
    let (mut adjustment, mut increment) = (0, 0);
    let mut disabled = windows::core::BOOL::default();
    unsafe { GetSystemTimeAdjustment(&mut adjustment, &mut increment, &mut disabled) }?;
    if increment == 0 {
        return Err(Error::Unsupported(
            "native hotkey timer resolution unavailable",
        ));
    }
    // GetTickCount is normally quantized to 10–16 ms. Include millisecond
    // rounding as well as the system's boot-time timer increment.
    Ok(
        Duration::from_nanos(u64::from(increment) * 100).max(Duration::from_millis(16))
            + Duration::from_millis(1),
    )
}

#[derive(Clone, Copy)]
struct PhysicalObservation {
    binding: HotkeyBinding,
    down: bool,
    virtual_key: u32,
    pressed_at: Option<u32>,
    release_pending: bool,
}

impl PhysicalObservation {
    fn new(binding: HotkeyBinding) -> Self {
        Self {
            binding,
            down: false,
            virtual_key: 0,
            pressed_at: None,
            release_pending: false,
        }
    }

    fn observe(
        &mut self,
        message: u32,
        scan_code: u32,
        virtual_key: u32,
        flags: KBDLLHOOKSTRUCT_FLAGS,
        time: u32,
        modifiers_down: bool,
    ) {
        if flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0
            || scan_code > 0xff
            || platform_win::hid::scancode_from_hook(scan_code, flags.0 & LLKHF_EXTENDED.0 != 0)
                != self.binding.scan_code
        {
            return;
        }
        match message {
            WM_KEYUP | WM_SYSKEYUP => {
                self.down = false;
                self.release_pending = true;
                self.pressed_at = None;
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if !self.down && modifiers_down {
                    self.pressed_at = Some(time);
                }
                self.down = true;
                self.virtual_key = virtual_key;
            }
            _ => {}
        }
    }
}

thread_local! {
    static PHYSICAL_OBSERVATION: Cell<Option<PhysicalObservation>> = const { Cell::new(None) };
}

struct PhysicalHook(HHOOK);
impl PhysicalHook {
    fn open(binding: HotkeyBinding) -> Result<Self, Error> {
        let module = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }?;
        PHYSICAL_OBSERVATION.set(Some(PhysicalObservation::new(binding)));
        match unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(physical_hook), Some(module.into()), 0)
        } {
            Ok(hook) => Ok(Self(hook)),
            Err(_) => {
                PHYSICAL_OBSERVATION.set(None);
                Err(Error::Unsupported(
                    "native physical hotkey observer unavailable",
                ))
            }
        }
    }

    fn take_edges() -> Option<PhysicalObservation> {
        let mut observation = PHYSICAL_OBSERVATION.get()?;
        let pending = observation;
        observation.pressed_at = None;
        observation.release_pending = false;
        PHYSICAL_OBSERVATION.set(Some(observation));
        Some(pending)
    }

    fn physically_down() -> bool {
        PHYSICAL_OBSERVATION.get().is_some_and(|observation| {
            // Async VK state can only revoke an already observed exact physical down.
            observation.down
                && (1..=254).contains(&observation.virtual_key)
                && unsafe { GetAsyncKeyState(observation.virtual_key as i32) < 0 }
                && observation.binding.modifiers_down()
        })
    }
}

impl Drop for PhysicalHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
        PHYSICAL_OBSERVATION.set(None);
    }
}

unsafe extern "system" fn physical_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // Windows owns the record until this callback returns; no VK alias admits an edge.
        let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if let Some(mut observation) = PHYSICAL_OBSERVATION.get() {
            observation.observe(
                wparam.0 as u32,
                key.scanCode,
                key.vkCode,
                key.flags,
                key.time,
                observation.binding.modifiers_down(),
            );
            PHYSICAL_OBSERVATION.set(Some(observation));
        }
    }
    // Never suppress or modify input; no allocation, logging, locks or I/O above.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[derive(Default)]
struct PressState {
    pressed: bool,
    blocked: bool,
    next_renewal: Option<Instant>,
    last_observation: Option<Instant>,
    announced: bool,
    initial_observation: Option<Instant>,
    release_pending: bool,
}
impl PressState {
    fn activate(&mut self, now: Instant) {
        if !self.pressed && !self.blocked {
            self.pressed = true;
            self.next_renewal = Some(now);
            self.last_observation = Some(now);
            self.initial_observation = Some(now);
        }
    }
    fn observe_release(&mut self, tx: &SyncSender<HotkeyEvent>) {
        *self = Self {
            blocked: true,
            release_pending: true,
            ..Self::default()
        };
        self.flush_release(tx);
    }
    fn flush_release(&mut self, tx: &SyncSender<HotkeyEvent>) {
        if self.release_pending
            && tx
                .try_send(HotkeyEvent::Released {
                    reason: PttReleaseReason::PhysicalKeyUp,
                })
                .is_ok()
        {
            *self = Self::default();
        }
    }
    fn poll(&mut self, now: Instant, down: bool, tx: &SyncSender<HotkeyEvent>) {
        self.flush_release(tx);
        if !down {
            if self.pressed {
                self.pressed = false;
                self.blocked = true;
                let _ = tx.try_send(HotkeyEvent::Released {
                    reason: PttReleaseReason::UnavailableOrReset,
                });
            }
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
            let observed_at = if self.announced {
                now
            } else {
                self.initial_observation.expect("activated press")
            };
            if tx
                .try_send(HotkeyEvent::PressLease {
                    observed_at,
                    expires_at: observed_at + PRESS_LEASE,
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
    fn queued_pre_fence_press_keeps_its_original_observation() {
        let now = Instant::now();
        let received = now + POLL * 4;
        let fence = now + POLL * 2;
        let (tx, rx) = mpsc::sync_channel(2);
        let mut state = PressState {
            blocked: true,
            ..PressState::default()
        };
        // An old observed up is queued before the fence and delivered after it.
        state.observe_release(&tx);
        let original = message_observed_at(received, 1_040, 1_010, POLL).unwrap();
        state.activate(original);
        state.poll(received, true, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp
            }
        ));
        let HotkeyEvent::PressLease {
            observed_at,
            expires_at,
            initial,
        } = rx.try_recv().unwrap()
        else {
            panic!("expected initial press");
        };
        assert!(initial);
        assert!(
            observed_at <= fence,
            "queued native press must remain behind the worker's release-all fence"
        );
        assert!(expires_at <= observed_at + PRESS_LEASE);
        assert!(observed_at <= original);
        // This is the worker boundary's acceptance condition after the queued
        // up has rearmed it: a pre-fence initial edge must fence again, not grant.
        assert!(!(initial && observed_at > fence));
    }
    #[test]
    fn unobservable_async_zero_never_claims_physical_key_up() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut state = PressState::default();
        state.activate(now);
        state.poll(now, true, &tx);
        assert!(rx.try_recv().unwrap().is_live(now));
        // The fixture is still physically held, but polling becomes unavailable.
        state.poll(now + POLL, false, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::UnavailableOrReset
            }
        ));
        assert!(state.blocked && !state.pressed);
        state.activate(now + POLL * 2);
        state.poll(now + POLL * 2, true, &tx);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn refuses_kvm_conflicts_and_invalid_bindings() {
        let kvm = HotkeyBinding {
            scan_code: 0x3b,
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
            scan_code: 0x3c,
            ..kvm
        }
        .validate(&[kvm])
        .is_ok());
        assert!(HotkeyBinding {
            scan_code: 0,
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
            scan_code: 0x2a,
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

        state.observe_release(&tx);
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
        state.observe_release(&tx);
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
        state.observe_release(&tx);
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
        state.observe_release(&tx);
        assert!(!state.blocked);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp,
            }
        ));
    }
    #[test]
    fn message_time_wrap_stale_future_and_tick_uncertainty_are_conservative() {
        let now = Instant::now() + Duration::from_secs(1);
        let uncertainty = Duration::from_millis(17);
        let wrapped = message_observed_at(now, 2, u32::MAX - 2, uncertainty).unwrap();
        assert_eq!(now.duration_since(wrapped), Duration::from_millis(22));
        let same_tick = message_observed_at(now, 100, 100, uncertainty).unwrap();
        assert_eq!(now.duration_since(same_tick), uncertainty);
        assert!(message_observed_at(now, 100, 101, uncertainty).is_none());
        assert!(message_observed_at(now, 1_000, 750, uncertainty).is_none());
        assert!(message_observed_at(now, 1_000, 767, uncertainty).is_none());
    }
    #[test]
    fn only_exact_non_injected_scan_identity_can_press_release_or_rearm() {
        let now = Instant::now();
        for (usage, matching_flags, other_flags) in [
            (0x28, LLKHF_UP, LLKHF_UP | LLKHF_EXTENDED),
            (0x58, LLKHF_UP | LLKHF_EXTENDED, LLKHF_UP),
        ] {
            let binding = HotkeyBinding::from_hid(usage, 0).unwrap();
            let mut observation = PhysicalObservation::new(binding);
            let (tx, rx) = mpsc::sync_channel(8);
            let mut press = PressState {
                blocked: true,
                ..PressState::default()
            };
            // Enter and keypad Enter have identical VK_RETURN, but only one can arm.
            observation.observe(WM_KEYUP, 0x1c, 0x0d, other_flags, 10, true);
            assert!(!observation.release_pending);
            observation.observe(WM_KEYDOWN, 0x1c, 0x0d, other_flags, 11, true);
            assert!(observation.pressed_at.is_none() && !observation.down);
            for injected in [LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED] {
                observation.observe(WM_KEYUP, 0x1c, 0x0d, matching_flags | injected, 12, true);
                assert!(!observation.release_pending);
                let down_flags = KBDLLHOOKSTRUCT_FLAGS(matching_flags.0 & !LLKHF_UP.0);
                observation.observe(WM_KEYDOWN, 0x1c, 0x0d, down_flags | injected, 12, true);
                assert!(observation.pressed_at.is_none() && !observation.down);
            }
            observation.observe(WM_KEYUP, 0x1c, 0x0d, matching_flags, 13, true);
            assert!(observation.release_pending);
            press.observe_release(&tx);
            assert!(matches!(
                rx.try_recv().unwrap(),
                HotkeyEvent::Released {
                    reason: PttReleaseReason::PhysicalKeyUp,
                }
            ));
            let down_flags = KBDLLHOOKSTRUCT_FLAGS(matching_flags.0 & !LLKHF_UP.0);
            observation.observe(WM_KEYDOWN, 0x1c, 0x0d, down_flags, 14, true);
            assert_eq!(observation.pressed_at, Some(14));
            press.activate(now);
            press.poll(now, observation.down, &tx);
            assert!(rx.try_recv().unwrap().is_live(now));
            observation.release_pending = false;
            observation.observe(WM_KEYUP, 0x1c, 0x0d, other_flags, 15, true);
            assert!(!observation.release_pending && observation.down);
            observation.observe(WM_KEYUP, 0x1c, 0x0d, matching_flags, 16, true);
            press.observe_release(&tx);
            assert!(!observation.down);
            assert!(matches!(
                rx.try_recv().unwrap(),
                HotkeyEvent::Released {
                    reason: PttReleaseReason::PhysicalKeyUp,
                }
            ));
        }
    }

    #[test]
    fn layout_vks_and_modifier_releases_never_replace_physical_hid_identity() {
        // Same physical HID positions on US/AZERTY/QWERTZ can report different VKs.
        for (usage, scan, layout_vks) in [
            (0x04, 0x1e, [0x41, 0x51, 0x41]),
            (0x1c, 0x15, [0x59, 0x59, 0x5a]),
            (0x1d, 0x2c, [0x5a, 0x57, 0x59]),
        ] {
            let binding = HotkeyBinding::from_hid(usage, 3).unwrap();
            let reserved = HotkeyBinding {
                modifiers: 7,
                ..binding
            };
            assert!(binding.conflicts(&[reserved]));
            for vk in layout_vks {
                let mut observed = PhysicalObservation::new(binding);
                observed.observe(WM_KEYDOWN, scan, vk, KBDLLHOOKSTRUCT_FLAGS(0), 1, true);
                assert_eq!(observed.pressed_at, Some(1));
                for modifier_scan in [0x1d, 0x2a, 0x36, 0x38, 0x5b, 0x5c] {
                    observed.observe(WM_KEYUP, modifier_scan, 0x10, LLKHF_UP, 2, true);
                    assert!(!observed.release_pending);
                }
                observed.observe(WM_KEYUP, scan, vk, LLKHF_UP, 3, true);
                assert!(observed.release_pending && !observed.down);
            }
        }
        let enter = HotkeyBinding::from_hid(0x28, 3).unwrap();
        let keypad_enter = HotkeyBinding::from_hid(0x58, 3).unwrap();
        assert!(!enter.conflicts(&[keypad_enter]));
        assert!(
            HotkeyBinding::from_hid(0x48, 0).is_err(),
            "E1 Pause is not representable"
        );
    }
    #[test]
    fn positive_release_coalesces_under_full_queue_without_rearming_a_held_press() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut state = PressState::default();
        state.activate(now);
        state.poll(now, true, &tx);
        state.observe_release(&tx);
        assert!(state.blocked && state.release_pending);
        state.activate(now + POLL);
        assert!(!state.pressed);
        assert!(rx.try_recv().unwrap().is_live(now));
        state.flush_release(&tx);
        assert!(!state.blocked && !state.release_pending);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::Released {
                reason: PttReleaseReason::PhysicalKeyUp
            }
        ));
        state.poll(now + POLL, true, &tx);
        assert!(
            rx.try_recv().is_err(),
            "a dropped old press cannot resurrect"
        );
        state.activate(now + POLL * 2);
        state.poll(now + POLL * 2, true, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HotkeyEvent::PressLease { initial: true, .. }
        ));
    }
}

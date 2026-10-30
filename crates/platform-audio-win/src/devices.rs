//! Active endpoint discovery and capture-endpoint mute control.
//!
//! All COM objects stay on a native MTA worker (enumeration uses the calling
//! thread's temporary MTA). No operation starts audio or changes default devices.
//! Device notifications are hints: debounce them for **100 ms**, then enumerate
//! again. A full bounded queue drops hints and sets `take_overflowed()`; callers
//! must also re-enumerate/re-read mute after overflow. No endpoint names or IDs
//! are logged. A mute handle binds one explicit capture ID for its entire life;
//! it never follows a new default or restores mute implicitly on shutdown.

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
    core::{implement, Interface, GUID, PCWSTR, PWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::PROPERTYKEY,
        Media::Audio::{
            eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole,
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMEndpoint, IMMNotificationClient,
            IMMNotificationClient_Impl, MMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA,
            DEVICE_STATE, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
        },
        System::Com::{
            CoCreateGuid, CoCreateInstance, CoTaskMemFree,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            CLSCTX_ALL, STGM_READ,
        },
    },
};

use crate::{common::Apartment, Error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFlow {
    Render,
    Capture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}

/// Each flag is independent: an endpoint can be the default for several roles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefaultRoles {
    pub console: bool,
    pub multimedia: bool,
    pub communications: bool,
}

/// Native WAVEFORMATEX values, without pointers or COM ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixFormat {
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub average_bytes_per_second: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    /// The exact `cbSize` extension. For WAVE_FORMAT_EXTENSIBLE this contains
    /// valid bits (u16 LE), channel mask (u32 LE), and subformat GUID (16 bytes).
    pub extension: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    /// Opaque persistent selection key returned by IMMDevice::GetId.
    pub id: String,
    /// Display only; never use this as a selection key.
    pub friendly_name: String,
    pub flow: DeviceFlow,
    pub default_roles: DefaultRoles,
    pub mix_format: MixFormat,
}

fn enumerator() -> Result<IMMDeviceEnumerator, Error> {
    // SAFETY: every caller holds an MTA apartment until the object is released.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

struct TaskMemory<T>(*mut T);
impl<T> Drop for TaskMemory<T> {
    fn drop(&mut self) {
        // SAFETY: these pointers are exclusively owned CoTaskMem allocations.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn device_id(device: &IMMDevice) -> Result<String, Error> {
    let id = TaskMemory(unsafe { device.GetId()? }.0);
    // SAFETY: GetId returns a NUL-terminated CoTaskMem string, held until copied.
    unsafe { PWSTR(id.0).to_string() }
        .map_err(|_| Error::Unsupported("endpoint ID is not valid UTF-16"))
}

fn friendly_name(device: &IMMDevice) -> Result<String, Error> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let mut value = unsafe { store.GetValue(&PKEY_Device_FriendlyName)? };
    let text = unsafe { PropVariantToStringAlloc(&value) };
    // Clear the owned PROPVARIANT even if conversion failed.
    let cleared = unsafe { PropVariantClear(&mut value) };
    let text = TaskMemory(text?.0);
    cleared?;
    Ok(String::from_utf16_lossy(unsafe { PWSTR(text.0).as_wide() }))
}

fn mix_format(device: &IMMDevice) -> Result<MixFormat, Error> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let allocation = TaskMemory(unsafe { client.GetMixFormat()? });
    if allocation.0.is_null() {
        return Err(Error::Unsupported("GetMixFormat returned no format"));
    }
    // SAFETY: GetMixFormat owns a WAVEFORMATEX plus cbSize extension until freed.
    // WAVEFORMATEX is packed; copy it rather than borrow its fields.
    let format = unsafe { allocation.0.read_unaligned() };
    let extension = unsafe {
        std::slice::from_raw_parts(
            allocation
                .0
                .cast::<u8>()
                .add(std::mem::size_of::<WAVEFORMATEX>()),
            usize::from(format.cbSize),
        )
    }
    .to_vec();
    Ok(MixFormat {
        format_tag: format.wFormatTag,
        channels: format.nChannels,
        sample_rate: format.nSamplesPerSec,
        average_bytes_per_second: format.nAvgBytesPerSec,
        block_align: format.nBlockAlign,
        bits_per_sample: format.wBitsPerSample,
        extension,
    })
}

/// Read-only active render and capture enumeration. An endpoint disappearing
/// during the snapshot returns its native error; retry after the debounce window.
/// Must be called on an MTA-compatible thread, not an existing STA thread.
pub fn enumerate_devices() -> Result<Vec<EndpointInfo>, Error> {
    let _apartment = Apartment::mta()?;
    let enumerator = enumerator()?;
    let mut endpoints = Vec::new();
    for (native_flow, flow) in [
        (eRender, DeviceFlow::Render),
        (eCapture, DeviceFlow::Capture),
    ] {
        let mut defaults = [None, None, None];
        for (index, role) in [eConsole, eMultimedia, eCommunications]
            .into_iter()
            .enumerate()
        {
            match unsafe { enumerator.GetDefaultAudioEndpoint(native_flow, role) } {
                Ok(device) => defaults[index] = Some(device_id(&device)?),
                // E_NOTFOUND is normal when a flow has no active default.
                Err(error) if error.code().0 == 0x80070490_u32 as i32 => {}
                Err(error) => return Err(error.into()),
            }
        }
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(native_flow, DEVICE_STATE_ACTIVE)? };
        for index in 0..unsafe { collection.GetCount()? } {
            let device = unsafe { collection.Item(index)? };
            let id = device_id(&device)?;
            let default_roles = DefaultRoles {
                console: defaults[0].as_deref() == Some(id.as_str()),
                multimedia: defaults[1].as_deref() == Some(id.as_str()),
                communications: defaults[2].as_deref() == Some(id.as_str()),
            };
            endpoints.push(EndpointInfo {
                id,
                friendly_name: friendly_name(&device)?,
                flow,
                default_roles,
                mix_format: mix_format(&device)?,
            });
        }
    }
    Ok(endpoints)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    Added {
        id: String,
    },
    Removed {
        id: String,
    },
    StateChanged {
        id: String,
        state: u32,
    },
    DefaultChanged {
        flow: DeviceFlow,
        role: DeviceRole,
        id: Option<String>,
    },
    PropertyChanged {
        id: String,
    },
}

/// Trailing-edge 100 ms coalescing, with a caller-supplied monotonic clock.
/// Call `notify` for every hint (including overflow), and enumerate when
/// `take_ready` returns true. Keep polling even when no further hints arrive.
#[derive(Debug, Default)]
pub struct DeviceDebouncer {
    last_change: Option<Instant>,
}
impl DeviceDebouncer {
    pub const DELAY: Duration = Duration::from_millis(100);

    pub fn notify(&mut self, now: Instant) {
        self.last_change = Some(now);
    }

    pub fn take_ready(&mut self, now: Instant) -> bool {
        if self
            .last_change
            .is_some_and(|last| now.saturating_duration_since(last) >= Self::DELAY)
        {
            self.last_change = None;
            true
        } else {
            false
        }
    }
}

fn queue_hint<T>(sender: &SyncSender<T>, overflowed: &AtomicBool, event: T) {
    if matches!(sender.try_send(event), Err(mpsc::TrySendError::Full(_))) {
        overflowed.store(true, Ordering::Release);
    }
}

fn notification_id(id: &PCWSTR) -> String {
    // SAFETY: the callback owns this NUL-terminated string for the invocation.
    if id.is_null() {
        String::new()
    } else {
        String::from_utf16_lossy(unsafe { id.as_wide() })
    }
}

#[implement(IMMNotificationClient)]
struct DeviceCallback {
    sender: SyncSender<DeviceEvent>,
    overflowed: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
}
impl DeviceCallback_Impl {
    fn emit(&self, event: DeviceEvent) {
        if self.active.load(Ordering::Acquire) {
            queue_hint(&self.sender, &self.overflowed, event);
        }
    }
}
#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceCallback_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        self.emit(DeviceEvent::StateChanged {
            id: notification_id(id),
            state: state.0,
        });
        Ok(())
    }
    fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.emit(DeviceEvent::Added {
            id: notification_id(id),
        });
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.emit(DeviceEvent::Removed {
            id: notification_id(id),
        });
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        let flow = if flow == eRender {
            DeviceFlow::Render
        } else if flow == eCapture {
            DeviceFlow::Capture
        } else {
            return Ok(());
        };
        let role = if role == eConsole {
            DeviceRole::Console
        } else if role == eMultimedia {
            DeviceRole::Multimedia
        } else if role == eCommunications {
            DeviceRole::Communications
        } else {
            return Ok(());
        };
        self.emit(DeviceEvent::DefaultChanged {
            flow,
            role,
            id: if id.is_null() {
                None
            } else {
                Some(notification_id(id))
            },
        });
        Ok(())
    }
    fn OnPropertyValueChanged(&self, id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        self.emit(DeviceEvent::PropertyChanged {
            id: notification_id(id),
        });
        Ok(())
    }
}

pub struct DeviceMonitor {
    pub events: Receiver<DeviceEvent>,
    overflowed: Arc<AtomicBool>,
    stop: Option<SyncSender<()>>,
    worker: Option<JoinHandle<Result<(), Error>>>,
}
impl DeviceMonitor {
    pub fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    /// Revokes the callback and joins its owning apartment before returning.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| Error::WorkerStopped)?,
            None => Ok(()),
        }
    }
}
impl Drop for DeviceMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Starts bounded endpoint notifications. Capacity must be nonzero. Subscribe
/// before the initial enumeration to avoid missing changes during that snapshot.
pub fn watch_devices(capacity: usize) -> Result<DeviceMonitor, Error> {
    if capacity == 0 {
        return Err(Error::InvalidArgument(
            "device event capacity must be nonzero",
        ));
    }
    let (sender, events) = mpsc::sync_channel(capacity);
    let (stop, stopped) = mpsc::sync_channel(1);
    let (ready, started) = mpsc::sync_channel(1);
    let overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflow = Arc::clone(&overflowed);
    let worker = thread::spawn(move || {
        let setup = (|| {
            let apartment = Apartment::mta()?;
            let enumerator = enumerator()?;
            let active = Arc::new(AtomicBool::new(true));
            let callback: IMMNotificationClient = DeviceCallback {
                sender,
                overflowed: callback_overflow,
                active: Arc::clone(&active),
            }
            .into();
            unsafe { enumerator.RegisterEndpointNotificationCallback(&callback)? };
            Ok::<_, Error>((apartment, enumerator, callback, active))
        })();
        let (_apartment, enumerator, callback, active) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = ready.send(Err(error));
                return Ok(());
            }
        };
        if ready.send(Ok(())).is_ok() {
            let _ = stopped.recv();
        }
        active.store(false, Ordering::Release);
        unsafe { enumerator.UnregisterEndpointNotificationCallback(&callback)? };
        Ok(())
    });
    match started.recv() {
        Ok(Ok(())) => Ok(DeviceMonitor {
            events,
            overflowed,
            stop: Some(stop),
            worker: Some(worker),
        }),
        result => {
            drop(stop);
            let _ = worker.join();
            match result {
                Ok(Err(error)) => Err(error),
                _ => Err(Error::WorkerStopped),
            }
        }
    }
}

/// GUID represented as a portable integer (GUID::to_u128 order, not native bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventContext(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MuteChange {
    pub muted: bool,
    pub context: EventContext,
    /// True only for SetMute calls issued through this exact handle.
    pub local: bool,
}

#[implement(IAudioEndpointVolumeCallback)]
struct MuteCallback {
    sender: SyncSender<MuteChange>,
    overflowed: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    context: GUID,
}
#[allow(non_snake_case)]
impl IAudioEndpointVolumeCallback_Impl for MuteCallback_Impl {
    fn OnNotify(
        &self,
        notification: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
    ) -> windows::core::Result<()> {
        if !notification.is_null() && self.active.load(Ordering::Acquire) {
            // SAFETY: Windows keeps the notification header valid for this call.
            // Never read afChannelVolumes (a variable-length trailing array).
            let context = unsafe { (*notification).guidEventContext };
            let muted = unsafe { (*notification).bMuted }.as_bool();
            queue_hint(
                &self.sender,
                &self.overflowed,
                MuteChange {
                    muted,
                    context: EventContext(context.to_u128()),
                    local: context == self.context,
                },
            );
        }
        Ok(())
    }
}

enum MuteCommand {
    Read(SyncSender<Result<bool, Error>>),
    Set(bool, SyncSender<Result<(), Error>>),
    Stop,
}

/// A fixed, explicit capture endpoint and its endpoint-volume subscription.
/// Changes from this handle carry `event_context`; physical/other application
/// changes retain their original GUID. Does not mute on open or restore on drop.
pub struct CaptureMute {
    pub device_id: String,
    pub event_context: EventContext,
    pub changes: Receiver<MuteChange>,
    overflowed: Arc<AtomicBool>,
    commands: Option<SyncSender<MuteCommand>>,
    worker: Option<JoinHandle<Result<(), Error>>>,
}
impl CaptureMute {
    pub fn open(device_id: &str, capacity: usize) -> Result<Self, Error> {
        if device_id.is_empty() || device_id.contains('\0') {
            return Err(Error::InvalidArgument(
                "capture endpoint ID must be nonempty and contain no NUL",
            ));
        }
        if capacity == 0 {
            return Err(Error::InvalidArgument(
                "mute event capacity must be nonzero",
            ));
        }
        let id: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
        let (sender, changes) = mpsc::sync_channel(capacity);
        let (commands, requests) = mpsc::sync_channel(8);
        let (ready, started) = mpsc::sync_channel(1);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflowed);
        let worker = thread::spawn(move || {
            let setup = (|| {
                let apartment = Apartment::mta()?;
                let enumerator = enumerator()?;
                let device = unsafe { enumerator.GetDevice(PCWSTR(id.as_ptr()))? };
                let endpoint: IMMEndpoint = device.cast()?;
                if unsafe { endpoint.GetDataFlow()? } != eCapture {
                    return Err(Error::InvalidArgument(
                        "mute endpoint must be capture, not render",
                    ));
                }
                let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None)? };
                let context = unsafe { CoCreateGuid()? };
                let active = Arc::new(AtomicBool::new(true));
                let callback: IAudioEndpointVolumeCallback = MuteCallback {
                    sender,
                    overflowed: callback_overflow,
                    active: Arc::clone(&active),
                    context,
                }
                .into();
                unsafe { volume.RegisterControlChangeNotify(&callback)? };
                Ok::<_, Error>((apartment, volume, callback, active, context))
            })();
            let (_apartment, volume, callback, active, context) = match setup {
                Ok(setup) => setup,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return Ok(());
                }
            };
            if ready.send(Ok(EventContext(context.to_u128()))).is_ok() {
                while let Ok(command) = requests.recv() {
                    match command {
                        MuteCommand::Read(reply) => {
                            let result = unsafe { volume.GetMute() }
                                .map(|value| value.as_bool())
                                .map_err(Error::from);
                            let _ = reply.try_send(result);
                        }
                        MuteCommand::Set(muted, reply) => {
                            let result =
                                unsafe { volume.SetMute(muted, &context) }.map_err(Error::from);
                            let _ = reply.try_send(result);
                        }
                        MuteCommand::Stop => break,
                    }
                }
            }
            active.store(false, Ordering::Release);
            unsafe { volume.UnregisterControlChangeNotify(&callback)? };
            Ok(())
        });
        match started.recv() {
            Ok(Ok(event_context)) => Ok(Self {
                device_id: device_id.to_owned(),
                event_context,
                changes,
                overflowed,
                commands: Some(commands),
                worker: Some(worker),
            }),
            result => {
                drop(commands);
                let _ = worker.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(Error::WorkerStopped),
                }
            }
        }
    }

    pub fn mute(&self) -> Result<bool, Error> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or(Error::WorkerStopped)?
            .send(MuteCommand::Read(reply))
            .map_err(|_| Error::WorkerStopped)?;
        result.recv().map_err(|_| Error::WorkerStopped)?
    }

    /// Explicitly changes only the capture endpoint selected by `open`.
    pub fn set_mute(&self, muted: bool) -> Result<(), Error> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or(Error::WorkerStopped)?
            .send(MuteCommand::Set(muted, reply))
            .map_err(|_| Error::WorkerStopped)?;
        result.recv().map_err(|_| Error::WorkerStopped)?
    }

    pub fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    pub fn shutdown(&mut self) -> Result<(), Error> {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(MuteCommand::Stop);
        }
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| Error::WorkerStopped)?,
            None => Ok(()),
        }
    }
}
impl Drop for CaptureMute {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_waits_for_one_quiet_window() {
        let start = Instant::now();
        let mut debounce = DeviceDebouncer::default();
        assert!(!debounce.take_ready(start));
        debounce.notify(start);
        assert!(!debounce.take_ready(start + Duration::from_millis(99)));
        debounce.notify(start + Duration::from_millis(75));
        assert!(!debounce.take_ready(start + Duration::from_millis(174)));
        assert!(debounce.take_ready(start + Duration::from_millis(175)));
        assert!(!debounce.take_ready(start + Duration::from_secs(1)));
    }

    #[test]
    fn bounded_hints_report_overflow_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflowed = AtomicBool::new(false);
        queue_hint(&sender, &overflowed, 1);
        queue_hint(&sender, &overflowed, 2);
        assert!(overflowed.swap(false, Ordering::AcqRel));
        assert_eq!(receiver.try_recv().unwrap(), 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn mute_notification_preserves_context_and_ownership() {
        let context = GUID::from_u128(42);
        let (sender, receiver) = mpsc::sync_channel(2);
        let callback: IAudioEndpointVolumeCallback = MuteCallback {
            sender,
            overflowed: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            context,
        }
        .into();
        let mut notification = AUDIO_VOLUME_NOTIFICATION_DATA {
            guidEventContext: context,
            bMuted: true.into(),
            ..Default::default()
        };
        unsafe { callback.OnNotify(&mut notification).unwrap() };
        assert_eq!(
            receiver.try_recv().unwrap(),
            MuteChange {
                muted: true,
                context: EventContext(42),
                local: true,
            }
        );
        notification.guidEventContext = GUID::from_u128(7);
        notification.bMuted = false.into();
        unsafe { callback.OnNotify(&mut notification).unwrap() };
        assert_eq!(
            receiver.try_recv().unwrap(),
            MuteChange {
                muted: false,
                context: EventContext(7),
                local: false,
            }
        );
    }
}

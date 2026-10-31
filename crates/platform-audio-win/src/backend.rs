use crate::{
    backend_devices::{BackendDevices, WindowsMuteGuard},
    backend_media::BackendMedia,
    hotkey::{Hotkey, HotkeyBinding, HotkeyEvent},
    stream::{self, EndpointSelection, StreamHandle, StreamStatus},
    Error,
};
use audio_backend::*;
use std::{cell::RefCell, sync::mpsc::TryRecvError, time::Instant};

/// Control-thread adapter. Construction opens no native device, session, or window.
/// Supply reserved KVM chords to `configure_ptt`; leases use absolute QPC-derived time.
pub struct WindowsBackend {
    devices: BackendDevices,
    media: RefCell<Option<BackendMedia>>,
    qpc_frequency: u64,
    ptt: Option<Hotkey>,
    ptt_binding: Option<HotkeyBinding>,
    pending_release: bool,
    media_turn: bool,
}
impl WindowsBackend {
    pub fn new() -> Result<Self, BackendError> {
        let mut frequency = 0;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|e| map_hresult(e.code().0))?;
        let qpc_frequency = u64::try_from(frequency)
            .ok()
            .filter(|value| *value != 0)
            .ok_or(BackendError::Unsupported)?;
        Ok(Self {
            devices: BackendDevices::new(),
            media: RefCell::new(None),
            qpc_frequency,
            ptt: None,
            ptt_binding: None,
            pending_release: false,
            media_turn: false,
        })
    }
    fn with_media<T>(
        &self,
        f: impl FnOnce(&mut BackendMedia) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let mut media = self.media.borrow_mut();
        if media.is_none() {
            *media = Some(BackendMedia::open()?);
        }
        f(media.as_mut().expect("initialized above"))
    }
    fn poll_ptt(&mut self) -> Option<Event> {
        if self.pending_release {
            self.pending_release = false;
            return Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            });
        }
        let result = self.ptt.as_ref()?.events().try_recv();
        match result {
            Ok(HotkeyEvent::Released { reason }) => Some(Event::PttReleased { reason }),
            Ok(HotkeyEvent::PressLease {
                observed_at,
                expires_at,
                initial,
            }) => {
                // Sample QPC before Instant so conversion can only shorten a lease.
                let qpc = match self.qpc_now_ns() {
                    Ok(value) => value,
                    Err(_) => {
                        return Some(Event::PttReleased {
                            reason: PttReleaseReason::UnavailableOrReset,
                        })
                    }
                };
                let now = Instant::now();
                let observed_at_ns = qpc.saturating_sub(
                    now.saturating_duration_since(observed_at)
                        .as_nanos()
                        .min(u64::MAX as u128) as u64,
                );
                let expires_at_ns = if now < expires_at {
                    qpc.saturating_add(
                        expires_at
                            .duration_since(now)
                            .as_nanos()
                            .min(u64::MAX as u128) as u64,
                    )
                } else {
                    qpc.saturating_sub(
                        now.duration_since(expires_at)
                            .as_nanos()
                            .min(u64::MAX as u128) as u64,
                    )
                };
                Some(if initial {
                    Event::PttPressed {
                        observed_at_ns,
                        expires_at_ns,
                    }
                } else {
                    Event::PttRenewed { expires_at_ns }
                })
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.ptt.take();
                self.ptt_binding = None;
                Some(Event::PttReleased {
                    reason: PttReleaseReason::UnavailableOrReset,
                })
            }
        }
    }
    fn qpc_now_ns(&self) -> Result<u64, BackendError> {
        let mut counter = 0;
        unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut counter) }
            .map_err(|e| map_hresult(e.code().0))?;
        let ticks = u64::try_from(counter).map_err(|_| BackendError::Unsupported)?;
        u64::try_from(u128::from(ticks) * 1_000_000_000 / u128::from(self.qpc_frequency))
            .map_err(|_| BackendError::Unsupported)
    }
}

/// Shared owned stream plus native clock/status diagnostics for the drift controller.
pub struct WindowsStream {
    native: StreamHandle,
    device: Option<DeviceId>,
    feedback: FeedbackGuarantee,
}
impl WindowsStream {
    pub fn native_snapshot(&self) -> stream::StreamSnapshot {
        self.native.snapshot()
    }
}
impl Stream for WindowsStream {
    fn format(&self) -> AudioFormat {
        fixed_format()
    }
    fn device(&self) -> Option<&DeviceId> {
        self.device.as_ref()
    }
    fn feedback_guarantee(&self) -> FeedbackGuarantee {
        self.feedback
    }
    fn snapshot(&self) -> StreamSnapshot {
        stream_snapshot(self.native.snapshot())
    }
    fn stop(&mut self) -> Result<(), BackendError> {
        self.native.stop();
        match self.native.snapshot().status {
            StreamStatus::Stopped => Ok(()),
            StreamStatus::DeviceInvalidated => Err(BackendError::DeviceGone),
            StreamStatus::Failed(code) => Err(map_hresult(code)),
            StreamStatus::Running => Err(BackendError::ServerUnavailable),
        }
    }
}
fn stream_snapshot(native: stream::StreamSnapshot) -> StreamSnapshot {
    StreamSnapshot {
        status: match native.status {
            StreamStatus::Running => audio_backend::StreamStatus::Running,
            StreamStatus::Stopped => audio_backend::StreamStatus::Stopped,
            StreamStatus::DeviceInvalidated => audio_backend::StreamStatus::DeviceInvalidated,
            StreamStatus::Failed(code) => audio_backend::StreamStatus::Failed(map_hresult(code)),
        },
        // Only render supplies a device-clock frequency. Capture's packet-local
        // position may reset while its independently valid QPC keeps advancing.
        device_position: (native.clock_valid && native.clock_frequency != 0)
            .then_some(native.device_position),
        clock_frequency: (native.clock_frequency != 0).then_some(native.clock_frequency),
        host_time_100ns: native.clock_valid.then_some(native.qpc_100ns),
        underrun_frames: native.underrun_frames,
        dropped_capture_frames: native.dropped_capture_frames,
    }
}
impl Backend for WindowsBackend {
    type Stream = WindowsStream;
    type Mute = WindowsMuteGuard;
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            system_capture: true,
            exclude_own_process_tree: true,
            endpoint_mute: true,
            media: true,
            global_ptt: true,
        }
    }
    fn probe_capabilities(&mut self) -> CapabilityProbe {
        let build = stream::os_build().ok();
        let process_exclude = match stream::start_capture_callback(Box::new(|_| {})) {
            Ok(mut stream) => {
                stream.stop();
                Availability::Available
            }
            Err(error) => Availability::Unavailable(map_error(error)),
        };
        let endpoint_mute = availability(
            self.devices
                .capture_mute(&DeviceSelection::FollowDefault(DefaultRole::Communications))
                .map(|_| ()),
        );
        let media = availability(self.with_media(|media| media.sessions().map(|_| ())));
        let probe = HotkeyBinding {
            virtual_key: 0x87,
            modifiers: 7,
        };
        let ptt = availability(Hotkey::register(probe, &[]).map(drop).map_err(map_error));
        CapabilityProbe {
            os_build: build,
            process_exclude,
            endpoint_mute,
            media,
            ptt,
            ptt_release: matches!(ptt, Availability::Available)
                .then_some(ReleaseObservation::Native),
        }
    }
    fn devices(&self) -> Result<Vec<Device>, BackendError> {
        self.devices.devices()
    }
    fn defaults(&self) -> Result<Defaults, BackendError> {
        self.devices.defaults()
    }
    fn start_capture(
        &mut self,
        request: CaptureSpec,
        callback: CaptureCallback,
    ) -> Result<Self::Stream, BackendError> {
        let CaptureSpec::SystemExcludingOwnProcessTree { format } = request else {
            return Err(BackendError::Unsupported);
        };
        require_format(format)?;
        let native = stream::start_capture_callback(callback).map_err(map_error)?;
        Ok(WindowsStream {
            native,
            device: None,
            feedback: FeedbackGuarantee::OwnProcessTreeExcluded,
        })
    }
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<Self::Stream, BackendError> {
        require_format(request.requested)?;
        let id = match request.device {
            DeviceSelection::Explicit(id) => id,
            DeviceSelection::FollowDefault(role) => self
                .defaults()?
                .for_role(role)
                .render
                .clone()
                .ok_or(BackendError::DeviceGone)?,
        };
        let native =
            stream::start_render_callback(EndpointSelection::Id(id.as_str().to_owned()), callback)
                .map_err(map_error)?;
        Ok(WindowsStream {
            native,
            device: Some(id),
            feedback: FeedbackGuarantee::None,
        })
    }
    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError> {
        self.devices.capture_mute(selection)
    }
    fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<Self::Mute, BackendError> {
        self.devices.acquire_mute(selection)
    }
    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        self.with_media(|media| media.sessions())
    }
    fn current_media_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.with_media(|media| media.current_session())
    }
    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        self.with_media(|media| media.admit(request))
    }
    fn cancel_media_commands(&mut self) {
        if let Some(media) = self.media.get_mut().as_mut() {
            media.cancel();
        }
    }
    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        let requested = hotkey.map(binding).transpose()?;
        let reserved = reserved
            .iter()
            .copied()
            .map(binding)
            .collect::<Result<Vec<_>, _>>()?;
        if requested.is_some_and(|key| key.conflicts(&reserved)) {
            return Err(BackendError::Conflict);
        }
        if requested == self.ptt_binding {
            return Ok(());
        }
        let replacement = requested
            .map(|key| Hotkey::register(key, &reserved).map_err(map_error))
            .transpose()?;
        self.pending_release |= self.ptt.is_some();
        self.ptt = replacement;
        self.ptt_binding = requested;
        Ok(())
    }
    fn poll_event(&mut self) -> Option<Event> {
        if let Some(event) = self.poll_ptt() {
            return Some(event);
        }
        self.media_turn = !self.media_turn;
        if self.media_turn {
            if let Some(event) = self
                .media
                .get_mut()
                .as_mut()
                .and_then(BackendMedia::poll_event)
            {
                return Some(event);
            }
        }
        if let Some(event) = self.devices.poll_event() {
            return Some(event);
        }
        self.media
            .get_mut()
            .as_mut()
            .and_then(BackendMedia::poll_event)
    }
}
fn availability(result: Result<(), BackendError>) -> Availability {
    result.map_or_else(Availability::Unavailable, |_| Availability::Available)
}
fn fixed_format() -> AudioFormat {
    AudioFormat {
        sample_rate: stream::SAMPLE_RATE,
        channels: 2,
    }
}
fn require_format(format: AudioFormat) -> Result<(), BackendError> {
    format.validate()?;
    if format != fixed_format() {
        return Err(BackendError::Unsupported);
    }
    Ok(())
}
pub(crate) fn map_error(error: Error) -> BackendError {
    match error {
        Error::Windows(error) | Error::Wasapi(wasapi::WasapiError::Windows(error)) => {
            map_hresult(error.code().0)
        }
        Error::InvalidArgument(_) => BackendError::InvalidArgument,
        Error::Unsupported(_) => BackendError::Unsupported,
        Error::WorkerStopped => BackendError::ServerUnavailable,
        Error::Wasapi(wasapi::WasapiError::DeviceNotFound(_)) => BackendError::DeviceGone,
        Error::Wasapi(_) => BackendError::Unsupported,
    }
}
fn map_hresult(code: i32) -> BackendError {
    match code as u32 {
        0x88890004 | 0x80070490 => BackendError::DeviceGone,
        0x80070005 => BackendError::PermissionDenied,
        0x8889000a | 0x800700aa => BackendError::Busy,
        0x80070057 => BackendError::InvalidArgument,
        0x80004001 | 0x80004002 => BackendError::Unsupported,
        _ => BackendError::ServerUnavailable,
    }
}
fn binding(hotkey: PttHotkey) -> Result<HotkeyBinding, BackendError> {
    hotkey.validate()?;
    let virtual_key = match hotkey.key_code {
        0x04..=0x1d => 0x41 + hotkey.key_code - 0x04,
        0x1e..=0x26 => 0x31 + hotkey.key_code - 0x1e,
        0x27 => 0x30,
        0x28 | 0x58 => 0x0d,
        0x29 => 0x1b,
        0x2a => 0x08,
        0x2b => 0x09,
        0x2c => 0x20,
        0x2d => 0xbd,
        0x2e => 0xbb,
        0x2f => 0xdb,
        0x30 => 0xdd,
        0x31 => 0xdc,
        0x33 => 0xba,
        0x34 => 0xde,
        0x35 => 0xc0,
        0x36 => 0xbc,
        0x37 => 0xbe,
        0x38 => 0xbf,
        0x39 => 0x14,
        0x3a..=0x45 => 0x70 + hotkey.key_code - 0x3a,
        0x46 => 0x2c,
        0x47 => 0x91,
        0x48 => 0x13,
        0x49 => 0x2d,
        0x4a => 0x24,
        0x4b => 0x21,
        0x4c => 0x2e,
        0x4d => 0x23,
        0x4e => 0x22,
        0x4f => 0x27,
        0x50 => 0x25,
        0x51 => 0x28,
        0x52 => 0x26,
        0x53 => 0x90,
        0x54 => 0x6f,
        0x55 => 0x6a,
        0x56 => 0x6d,
        0x57 => 0x6b,
        0x59..=0x61 => 0x61 + hotkey.key_code - 0x59,
        0x62 => 0x60,
        0x63 => 0x6e,
        0x64 => 0xe2,
        0x65 => 0x5d,
        0x68..=0x73 => 0x7c + hotkey.key_code - 0x68,
        _ => return Err(BackendError::Unsupported),
    };
    Ok(HotkeyBinding {
        virtual_key: virtual_key as u16,
        modifiers: u32::from(hotkey.alt)
            | u32::from(hotkey.control) << 1
            | u32::from(hotkey.shift) << 2
            | u32::from(hotkey.meta) << 3,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_capture_snapshot_hides_resetting_positions_but_keeps_qpc() {
        let mut native = stream::StreamSnapshot {
            status: StreamStatus::Running,
            transferred_frames: 480,
            dropped_capture_frames: 3,
            underrun_frames: 0,
            device_position: 479,
            clock_frequency: 0,
            qpc_100ns: 1_099_791,
            clock_valid: true,
        };
        let before = stream_snapshot(native);
        native.device_position = 0;
        native.qpc_100ns = 1_100_000;
        let after = stream_snapshot(native);
        for snapshot in [before, after] {
            assert_eq!(snapshot.device_position, None);
            assert_eq!(snapshot.clock_frequency, None);
            assert_eq!(snapshot.dropped_capture_frames, 3);
        }
        assert_eq!(before.host_time_100ns, Some(1_099_791));
        assert_eq!(after.host_time_100ns, Some(1_100_000));
        assert!(after.host_time_100ns > before.host_time_100ns);
        native.clock_valid = false;
        let invalid = stream_snapshot(native);
        assert_eq!(invalid.device_position, None);
        assert_eq!(invalid.host_time_100ns, None);
    }

    #[test]
    fn render_snapshot_preserves_monotonic_device_clock_and_frequency() {
        let mut native = stream::StreamSnapshot {
            status: StreamStatus::Running,
            transferred_frames: 0,
            dropped_capture_frames: 0,
            underrun_frames: 7,
            device_position: 0,
            clock_frequency: 10_000_000,
            qpc_100ns: 0,
            clock_valid: true,
        };
        let mut previous = None;
        for position in [0, 100_000, 200_000] {
            native.device_position = position;
            native.qpc_100ns = position;
            let snapshot = stream_snapshot(native);
            assert_eq!(snapshot.device_position, Some(position));
            assert_eq!(snapshot.clock_frequency, Some(10_000_000));
            assert_eq!(snapshot.host_time_100ns, Some(position));
            assert_eq!(snapshot.underrun_frames, 7);
            if let Some(previous) = previous {
                assert!(snapshot.device_position > Some(previous));
            }
            previous = snapshot.device_position;
        }
        native.clock_valid = false;
        let invalid = stream_snapshot(native);
        assert_eq!(invalid.device_position, None);
        assert_eq!(invalid.host_time_100ns, None);
        assert_eq!(invalid.clock_frequency, Some(10_000_000));
    }

    #[test]
    fn pending_reconfiguration_release_never_claims_physical_key_up() {
        let mut backend = WindowsBackend::new().unwrap();
        backend.pending_release = true;
        assert_eq!(
            backend.poll_ptt(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            })
        );
        assert_eq!(backend.poll_ptt(), None);
    }
    #[test]
    fn construction_has_no_native_handles_and_hid_is_not_vk() {
        let mut backend = WindowsBackend::new().unwrap();
        assert!(backend.media.borrow().is_none());
        assert!(backend.ptt.is_none());
        let key = PttHotkey {
            key_code: 0x04,
            control: true,
            alt: false,
            shift: false,
            meta: false,
        };
        assert_eq!(
            binding(key).unwrap(),
            HotkeyBinding {
                virtual_key: 0x41,
                modifiers: 2
            }
        );
        assert_eq!(
            backend.configure_ptt(Some(key), &[key]),
            Err(BackendError::Conflict)
        );
        assert!(backend.ptt.is_none());
        assert!(require_format(AudioFormat {
            sample_rate: 44_100,
            channels: 2
        })
        .is_err());
        assert_eq!(map_hresult(0x88890004u32 as i32), BackendError::DeviceGone);
    }
}

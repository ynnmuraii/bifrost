//! Event-driven 48 kHz float stereo streams. The caller allocates the rings before start.
//! No allocation, logging, channel send, or mutex acquisition occurs in either steady-state
//! loop. Poll `snapshot` off the real-time thread. On invalidation, drop the handle and
//! explicitly restart with the original selection (or re-resolve FollowDefault).
use crate::{
    common::{Apartment, Event, ProAudio},
    render_config::{
        initialize_legacy_render, initialize_shared_render, RenderConfiguration, SAFE_PERIOD_FRAMES,
    },
    Error,
};
use rtrb::{Consumer, Producer};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::Audio::*,
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
};

pub const SAMPLE_RATE: u32 = 48_000;
pub type StereoFrame = [f32; 2];

enum CaptureSink {
    Ring(Producer<CapturedFrame>),
    Callback(audio_backend::CaptureCallback),
}
enum CaptureTarget {
    Process { pid: u32, include: bool },
    DefaultEndpointForProbe,
}
enum RenderSource {
    Ring(Consumer<StereoFrame>),
    Callback(audio_backend::RenderCallback),
}
/// Timestamp is WASAPI QPC time converted to 100 ns units, not raw QPC ticks.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapturedFrame {
    pub samples: StereoFrame,
    /// Raw packet-local diagnostic; process loopback can reset it on every packet.
    /// Not a monotonic device clock, even when the QPC timestamp is valid.
    pub device_position: u64,
    pub qpc_100ns: u64,
    pub timestamp_valid: bool,
    pub discontinuity: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointSelection {
    FollowDefaultMultimedia,
    Id(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamStatus {
    Running,
    Stopped,
    DeviceInvalidated,
    Failed(i32),
}
#[derive(Clone, Copy, Debug)]
pub struct StreamSnapshot {
    pub status: StreamStatus,
    pub transferred_frames: u64,
    pub dropped_capture_frames: u64,
    pub underrun_frames: u64,
    /// Zero padding on render event wakes after prefill, including single-period buffers.
    /// This is an observed queue condition, not an OS-reported audio glitch.
    pub render_padding_zero_events: u64,
    /// Subset of zero-padding events when the configured queue holds two full periods.
    /// Only in that case should one period normally remain at an on-time wake.
    pub engine_starvation_events: u64,
    /// None for capture; render negotiation is immutable for the stream lifetime.
    pub render_configuration: Option<RenderConfiguration>,
    /// Render clock units and frequency (not necessarily audio sample frames).
    pub device_position: u64,
    /// Zero for process capture: packet positions do not define a device clock.
    pub clock_frequency: u64,
    pub qpc_100ns: u64,
    /// Whether QPC is valid; device position also requires a nonzero clock frequency.
    pub clock_valid: bool,
}
#[derive(Default)]
struct State {
    stop: AtomicBool,
    status: AtomicU8,
    error: AtomicI32,
    frames: AtomicU64,
    dropped: AtomicU64,
    underruns: AtomicU64,
    padding_zero: AtomicU64,
    starvations: AtomicU64,
    position: AtomicU64,
    frequency: AtomicU64,
    qpc: AtomicU64,
    clock_valid: AtomicBool,
    sequence: AtomicU64,
}
/// Drop/stop joins the native worker; call off latency-sensitive threads.
pub struct StreamHandle {
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
    render_configuration: Option<RenderConfiguration>,
}
impl StreamHandle {
    pub fn snapshot(&self) -> StreamSnapshot {
        let (position, qpc, frequency, clock_valid) = loop {
            let before = self.state.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let values = (
                self.state.position.load(Ordering::SeqCst),
                self.state.qpc.load(Ordering::SeqCst),
                self.state.frequency.load(Ordering::SeqCst),
                self.state.clock_valid.load(Ordering::SeqCst),
            );
            if before == self.state.sequence.load(Ordering::SeqCst) {
                break values;
            }
        };
        StreamSnapshot {
            status: match self.state.status.load(Ordering::Acquire) {
                0 => StreamStatus::Running,
                1 => StreamStatus::Stopped,
                2 => StreamStatus::DeviceInvalidated,
                _ => StreamStatus::Failed(self.state.error.load(Ordering::Relaxed)),
            },
            transferred_frames: self.state.frames.load(Ordering::Relaxed),
            dropped_capture_frames: self.state.dropped.load(Ordering::Relaxed),
            underrun_frames: self.state.underruns.load(Ordering::Relaxed),
            render_padding_zero_events: self.state.padding_zero.load(Ordering::Relaxed),
            engine_starvation_events: self.state.starvations.load(Ordering::Relaxed),
            render_configuration: self.render_configuration,
            device_position: position,
            clock_frequency: frequency,
            qpc_100ns: qpc,
            clock_valid,
        }
    }
    pub fn stop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[repr(C)]
struct OsVersion {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OsVersion) -> i32;
}
/// Read-only runtime gate. Activation can still fail and never falls back to whole-system capture.
pub fn os_build() -> Result<u32, Error> {
    let mut v = OsVersion {
        size: std::mem::size_of::<OsVersion>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    if unsafe { RtlGetVersion(&mut v) } < 0 {
        return Err(Error::Unsupported("OS version unavailable"));
    }
    Ok(v.build)
}
fn format() -> WaveFormat {
    WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None)
}
fn attach_event(client: &IAudioClient) -> Result<Event, Error> {
    let event = Event(unsafe { CreateEventW(None, false, false, PCWSTR::null())? });
    unsafe {
        client.SetEventHandle(event.0)?;
    }
    Ok(event)
}
fn start_worker<F>(run: F) -> Result<StreamHandle, Error>
where
    F: FnOnce(Arc<State>, mpsc::SyncSender<Result<Option<RenderConfiguration>, Error>>)
        + Send
        + 'static,
{
    let state = Arc::new(State::default());
    let shared = state.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || run(shared, tx));
    match rx.recv() {
        Ok(Ok(render_configuration)) => Ok(StreamHandle {
            state,
            worker: Some(worker),
            render_configuration,
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
fn finish(state: &State, result: windows::core::Result<()>) {
    match result {
        Ok(()) => state.status.store(1, Ordering::Release),
        Err(error) => {
            state.error.store(error.code().0, Ordering::Relaxed);
            state.status.store(
                if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    2
                } else {
                    3
                },
                Ordering::Release,
            );
        }
    }
}
/// Exclude the supplied audio-role PID and all descendants, across all render endpoints.
/// `pid` must own every Bifrost render stream. The format is fixed explicitly; process
/// loopback does not implement GetMixFormat/GetDevicePeriod. Full rings drop frames and
/// mark the next delivered frame discontinuous. There is no whole-system fallback.
pub fn start_capture_excluding(
    pid: u32,
    sink: Producer<CapturedFrame>,
) -> Result<StreamHandle, Error> {
    if pid == 0 {
        return Err(Error::InvalidArgument("zero process ID"));
    }
    if os_build()? < 20348 {
        return Err(Error::Unsupported(
            "process loopback requires Windows build 20348",
        ));
    }
    start_capture(
        CaptureTarget::Process {
            pid,
            include: false,
        },
        CaptureSink::Ring(sink),
    )
}
/// Diagnostic positive control used by hw_probe; never use this for a Source.
#[doc(hidden)]
pub fn start_capture_including_for_probe(
    pid: u32,
    sink: Producer<CapturedFrame>,
) -> Result<StreamHandle, Error> {
    if os_build()? < 20348 {
        return Err(Error::Unsupported(
            "process loopback requires Windows build 20348",
        ));
    }
    start_capture(
        CaptureTarget::Process { pid, include: true },
        CaptureSink::Ring(sink),
    )
}
/// Diagnostic capture of the entire default multimedia render endpoint, not a process.
///
/// The endpoint is resolved once at startup; default changes do not retarget the stream.
/// Shared-mode engine conversion requests 48 kHz f32 stereo even for a 44.1 kHz or
/// multichannel mix. Unsupported conversion fails startup; native mix bytes are never
/// reinterpreted as stereo. This does not alter endpoint selection, volume or mute.
/// Silence may produce no packets. Ring overflow and invalidation follow the same
/// rules as process capture. Never use this whole-endpoint probe as a Source.
#[doc(hidden)]
pub fn start_endpoint_loopback_for_probe(
    sink: Producer<CapturedFrame>,
) -> Result<StreamHandle, Error> {
    start_capture(
        CaptureTarget::DefaultEndpointForProbe,
        CaptureSink::Ring(sink),
    )
}
pub(crate) fn start_capture_callback(
    callback: audio_backend::CaptureCallback,
) -> Result<StreamHandle, Error> {
    if os_build()? < 20348 {
        return Err(Error::Unsupported(
            "process loopback requires Windows build 20348",
        ));
    }
    start_capture(
        CaptureTarget::Process {
            pid: std::process::id(),
            include: false,
        },
        CaptureSink::Callback(callback),
    )
}

fn start_capture(target: CaptureTarget, mut sink: CaptureSink) -> Result<StreamHandle, Error> {
    start_worker(move |state, ready| {
        let setup = || -> Result<_, Error> {
            let apartment = Apartment::mta()?;
            let mut client = match target {
                CaptureTarget::Process { pid, include } => {
                    AudioClient::new_application_loopback_client(pid, include)?
                }
                CaptureTarget::DefaultEndpointForProbe => DeviceEnumerator::new()?
                    .get_default_device_for_role(&Direction::Render, &wasapi::Role::Multimedia)?
                    .get_iaudioclient()?,
            };
            // A render endpoint + Capture sets LOOPBACK; autoconvert adds
            // AUTOCONVERTPCM | SRC_DEFAULT_QUALITY for this exact client format.
            client.initialize_client(
                &format(),
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 0,
                },
            )?;
            let raw = client.as_iaudioclient().clone();
            let event = attach_event(&raw)?;
            let capture: IAudioCaptureClient = unsafe { raw.GetService()? };
            Ok((apartment, client, raw, event, capture))
        };
        let (_apartment, _client, raw, event, capture) = match setup() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        // Process-loopback positions reset at packet boundaries; leave frequency at zero.
        let _priority = ProAudio::enter();
        if let Err(e) = unsafe { raw.Start() } {
            let _ = ready.send(Err(e.into()));
            return;
        }
        let _ = ready.send(Ok(None));
        let result = capture_loop(&state, &capture, &event, &mut sink);
        let _ = unsafe { raw.Stop() };
        finish(&state, result);
    })
}
// The SILENT contract permits a null data pointer; test this independently of hardware.
unsafe fn read_frame(pointer: *const u8, silent: bool, index: usize) -> StereoFrame {
    if silent {
        [0.0; 2]
    } else {
        unsafe {
            let p = pointer.cast::<f32>().add(index * 2);
            [p.read_unaligned(), p.add(1).read_unaligned()]
        }
    }
}
fn process_capture_block(
    samples: &[f32],
    qpc: u64,
    flags: u32,
    offset: usize,
) -> audio_backend::CaptureBlock<'_> {
    audio_backend::CaptureBlock {
        samples,
        frames: samples.len() / 2,
        format: audio_backend::AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: 2,
        },
        timestamp_ns: (qpc + offset as u64 * 10_000_000 / u64::from(SAMPLE_RATE))
            .saturating_mul(100),
        timestamp_valid: flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0,
        // Native process-loopback positions can reset every packet (479 -> 0);
        // they are not a monotonic device clock. The correlated QPC remains usable.
        device_position: None,
        discontinuity: offset == 0 && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
    }
}
struct CapturePacket<'a> {
    client: &'a IAudioCaptureClient,
    frames: Option<u32>,
}
impl CapturePacket<'_> {
    fn release(mut self) -> windows::core::Result<()> {
        unsafe {
            self.client
                .ReleaseBuffer(self.frames.take().expect("owned packet"))
        }
    }
}
impl Drop for CapturePacket<'_> {
    fn drop(&mut self) {
        if let Some(frames) = self.frames.take() {
            let _ = unsafe { self.client.ReleaseBuffer(frames) };
        }
    }
}
struct RenderPacket<'a> {
    client: &'a IAudioRenderClient,
    frames: Option<u32>,
}
impl RenderPacket<'_> {
    fn release(mut self) -> windows::core::Result<()> {
        unsafe {
            self.client
                .ReleaseBuffer(self.frames.take().expect("owned packet"), 0)
        }
    }
}
impl Drop for RenderPacket<'_> {
    fn drop(&mut self) {
        if let Some(frames) = self.frames.take() {
            let _ = unsafe {
                self.client
                    .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            };
        }
    }
}
fn capture_loop(
    state: &State,
    capture: &IAudioCaptureClient,
    event: &Event,
    sink: &mut CaptureSink,
) -> windows::core::Result<()> {
    let mut pending_gap = false;
    let silence = [0.0_f32; audio_backend::MAX_CAPTURE_BLOCK_FRAMES * 2];
    while !state.stop.load(Ordering::Acquire) {
        let wait = unsafe { WaitForSingleObject(event.0, 100) };
        if wait == WAIT_TIMEOUT {
            // An unplugged endpoint need not signal another audio event.
            let _ = unsafe { capture.GetNextPacketSize()? };
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            return Err(windows::core::Error::from_thread());
        }
        while unsafe { capture.GetNextPacketSize()? } != 0 {
            let (mut pointer, mut count, mut flags, mut position, mut qpc) =
                (std::ptr::null_mut(), 0, 0, 0, 0);
            unsafe {
                capture.GetBuffer(
                    &mut pointer,
                    &mut count,
                    &mut flags,
                    Some(&mut position),
                    Some(&mut qpc),
                )?;
            }
            let packet = CapturePacket {
                client: capture,
                frames: Some(count),
            };
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let invalid_pointer = !silent && pointer.is_null() && count != 0;
            let oversized_packet = count > SAMPLE_RATE;
            let (mut delivered, mut dropped) = (0, 0);
            // Read only while the WASAPI buffer is held, and never construct a slice for silence.
            if !invalid_pointer && !oversized_packet {
                let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                match sink {
                    CaptureSink::Callback(callback) => {
                        for offset in
                            (0..count as usize).step_by(audio_backend::MAX_CAPTURE_BLOCK_FRAMES)
                        {
                            let frames = (count as usize - offset)
                                .min(audio_backend::MAX_CAPTURE_BLOCK_FRAMES);
                            let samples = if silent {
                                &silence[..frames * 2]
                            } else {
                                // WASAPI f32 buffers are aligned native PCM; null was rejected above.
                                unsafe {
                                    std::slice::from_raw_parts(
                                        pointer.cast::<f32>().add(offset * 2),
                                        frames * 2,
                                    )
                                }
                            };
                            callback(process_capture_block(samples, qpc, flags, offset));
                            delivered += frames as u64;
                        }
                    }
                    CaptureSink::Ring(sink) => {
                        for index in 0..count {
                            let samples = unsafe { read_frame(pointer, silent, index as usize) };
                            let frame = CapturedFrame {
                                samples,
                                device_position: position + u64::from(index),
                                qpc_100ns: qpc
                                    + u64::from(index) * 10_000_000 / u64::from(SAMPLE_RATE),
                                timestamp_valid: flags
                                    & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32
                                    == 0,
                                discontinuity: pending_gap || (index == 0 && discontinuity),
                            };
                            if sink.push(frame).is_err() {
                                pending_gap = true;
                                dropped += 1;
                            } else {
                                pending_gap = false;
                                delivered += 1;
                            }
                        }
                    }
                }
            }
            packet.release()?;
            state.sequence.fetch_add(1, Ordering::SeqCst);
            state.position.store(position, Ordering::SeqCst);
            state.qpc.store(qpc, Ordering::SeqCst);
            state.clock_valid.store(
                flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0,
                Ordering::SeqCst,
            );
            state.sequence.fetch_add(1, Ordering::SeqCst);
            state.frames.fetch_add(delivered, Ordering::Relaxed);
            state.dropped.fetch_add(dropped, Ordering::Relaxed);
            if oversized_packet {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070057u32 as i32,
                )));
            }
            if invalid_pointer {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80004003u32 as i32,
                )));
            }
        }
    }
    Ok(())
}
/// Shared-mode engine conversion accepts 48 kHz f32 stereo even on another native mix.
/// Explicit IDs are never silently replaced. DeviceInvalidated requires a caller-driven rebuild.
/// Prefer IAudioClient3 with a >=5 ms period; unsupported configurations use legacy shared mode.
/// Both paths queue at most two engine periods, irrespective of the WASAPI buffer allocation.
pub fn start_render(
    selection: EndpointSelection,
    source: Consumer<StereoFrame>,
) -> Result<StreamHandle, Error> {
    start_render_source(
        selection,
        RenderSource::Ring(source),
        Some(SAFE_PERIOD_FRAMES),
    )
}
pub(crate) fn start_render_callback(
    selection: EndpointSelection,
    callback: audio_backend::RenderCallback,
) -> Result<StreamHandle, Error> {
    start_render_source(
        selection,
        RenderSource::Callback(callback),
        Some(SAFE_PERIOD_FRAMES),
    )
}
/// Diagnostic candidate floor only; does not change production policy or endpoint settings.
#[doc(hidden)]
pub fn start_render_for_probe(
    selection: EndpointSelection,
    source: Consumer<StereoFrame>,
    floor_frames: u32,
) -> Result<StreamHandle, Error> {
    start_render_source(selection, RenderSource::Ring(source), Some(floor_frames))
}
/// Diagnostic comparison using legacy shared initialization and the same two-period bound.
#[doc(hidden)]
pub fn start_legacy_render_for_probe(
    selection: EndpointSelection,
    source: Consumer<StereoFrame>,
) -> Result<StreamHandle, Error> {
    start_render_source(selection, RenderSource::Ring(source), None)
}
fn start_render_source(
    selection: EndpointSelection,
    mut source: RenderSource,
    floor_frames: Option<u32>,
) -> Result<StreamHandle, Error> {
    start_worker(move |state, ready| {
        let setup = || -> Result<_, Error> {
            let apartment = Apartment::mta()?;
            let enumerator = DeviceEnumerator::new()?;
            let device = match &selection {
                EndpointSelection::FollowDefaultMultimedia => enumerator
                    .get_default_device_for_role(&Direction::Render, &wasapi::Role::Multimedia)?,
                EndpointSelection::Id(id) => enumerator.get_device(id)?,
            };
            let (raw, config) = match floor_frames {
                Some(floor) => initialize_shared_render(&device, floor)?,
                None => initialize_legacy_render(&device, None)?,
            };
            let event = attach_event(&raw)?;
            let render: IAudioRenderClient = unsafe { raw.GetService()? };
            let clock: IAudioClock = unsafe { raw.GetService()? };
            let frequency = unsafe { clock.GetFrequency()? };
            state.frequency.store(frequency, Ordering::SeqCst);
            Ok((apartment, raw, event, render, clock, config))
        };
        let (_apartment, raw, event, render, clock, config) = match setup() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _priority = ProAudio::enter();
        // Prime before Start so the first signalled wake has the same queue invariant.
        if let Err(e) = render_frames(&state, &render, config.queue_limit_frames, &mut source)
            .and_then(|()| unsafe { raw.Start() })
        {
            let _ = ready.send(Err(e.into()));
            return;
        }
        let _ = ready.send(Ok(Some(config)));
        let result = render_loop(&state, &raw, &event, &render, &clock, config, &mut source);
        let _ = unsafe { raw.Stop() };
        finish(&state, result);
    })
}
fn render_loop(
    state: &State,
    raw: &IAudioClient,
    event: &Event,
    render: &IAudioRenderClient,
    clock: &IAudioClock,
    config: RenderConfiguration,
    source: &mut RenderSource,
) -> windows::core::Result<()> {
    while !state.stop.load(Ordering::Acquire) {
        let wait = unsafe { WaitForSingleObject(event.0, 100) };
        if wait == WAIT_TIMEOUT {
            let _ = unsafe { raw.GetCurrentPadding()? };
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            return Err(windows::core::Error::from_thread());
        }
        let padding = unsafe { raw.GetCurrentPadding()? };
        if padding == 0 {
            state.padding_zero.fetch_add(1, Ordering::Relaxed);
            if config.queue_limit_frames >= config.period_frames.saturating_mul(2) {
                state.starvations.fetch_add(1, Ordering::Relaxed);
            }
        }
        let available = config.queue_limit_frames.saturating_sub(padding);
        if available != 0 {
            render_frames(state, render, available, source)?;
        }
        let (mut position, mut qpc) = (0, 0);
        unsafe {
            clock.GetPosition(&mut position, Some(&mut qpc))?;
        }
        state.sequence.fetch_add(1, Ordering::SeqCst);
        state.position.store(position, Ordering::SeqCst);
        state.qpc.store(qpc, Ordering::SeqCst);
        state.clock_valid.store(true, Ordering::SeqCst);
        state.sequence.fetch_add(1, Ordering::SeqCst);
    }
    Ok(())
}
fn render_frames(
    state: &State,
    render: &IAudioRenderClient,
    available: u32,
    source: &mut RenderSource,
) -> windows::core::Result<()> {
    let pointer = unsafe { render.GetBuffer(available)? };
    if pointer.is_null() {
        let _ = unsafe { render.ReleaseBuffer(available, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) };
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x80004003u32 as i32,
        )));
    }
    let packet = RenderPacket {
        client: render,
        frames: Some(available),
    };
    let mut missing = 0;
    match source {
        RenderSource::Callback(callback) => {
            let samples = unsafe {
                std::slice::from_raw_parts_mut(pointer.cast::<f32>(), available as usize * 2)
            };
            samples.fill(0.0);
            callback(samples);
        }
        RenderSource::Ring(source) => {
            for index in 0..available {
                let frame = source.pop().unwrap_or_else(|_| {
                    missing += 1;
                    [0.0; 2]
                });
                unsafe {
                    let p = pointer.cast::<f32>().add(index as usize * 2);
                    p.write_unaligned(frame[0]);
                    p.add(1).write_unaligned(frame[1]);
                }
            }
        }
    }
    packet.release()?;
    state
        .frames
        .fetch_add(u64::from(available), Ordering::Relaxed);
    state.underruns.fetch_add(missing, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_capture_blocks_keep_qpc_without_device_positions() {
        let samples = [0.0; 2];
        // Last sample of one 480-frame packet, then the first sample after its reset.
        let last = process_capture_block(&samples, 1_000_000, 0, 479);
        let next = process_capture_block(&samples, 1_100_000, 0, 0);
        for block in [last, next] {
            assert_eq!(block.device_position, None);
            assert!(block.timestamp_valid);
            assert_eq!(block.frames, 1);
            assert_eq!(block.format.sample_rate, SAMPLE_RATE);
        }
        assert_eq!(last.timestamp_ns, 100_000_000 + 9_979_100);
        assert_eq!(next.timestamp_ns, 110_000_000);
        assert!(next.timestamp_ns > last.timestamp_ns);

        let split = process_capture_block(&samples, 1_100_000, 0, 960);
        assert_eq!(split.device_position, None);
        assert_eq!(split.timestamp_ns, 130_000_000);
        assert!(split.timestamp_valid);
        let invalid =
            process_capture_block(&samples, 0, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32, 0);
        assert_eq!(invalid.device_position, None);
        assert!(!invalid.timestamp_valid);
    }

    #[test]
    fn explicit_format_and_ring_handoff() {
        let f = format();
        assert_eq!(f.get_samplespersec(), SAMPLE_RATE);
        assert_eq!(f.get_nchannels(), 2);
        assert_eq!(f.get_bitspersample(), 32);
        assert_eq!(f.get_validbitspersample(), 32);
        assert_eq!(f.get_blockalign(), 8);
        assert_eq!(f.get_subformat().unwrap(), SampleType::Float);
        let (mut tx, mut rx) = rtrb::RingBuffer::new(2);
        let frame = CapturedFrame {
            samples: [0.25, -0.25],
            device_position: 7,
            ..Default::default()
        };
        tx.push(frame).unwrap();
        assert_eq!(rx.pop().unwrap().samples, frame.samples);
        assert!(rx.pop().is_err());
        assert_eq!(unsafe { read_frame(std::ptr::null(), true, 0) }, [0.0; 2]);
        let samples = [0.25_f32, -0.75];
        assert_eq!(
            unsafe { read_frame(samples.as_ptr().cast(), false, 0) },
            samples
        );
    }
}

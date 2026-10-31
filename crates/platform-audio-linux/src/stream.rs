use crate::{
    backend::{Entry, Key, State},
    raw::{self, Config, Connection},
};
use audio_backend::*;
use pulseaudio::{protocol as p, PlaybackSource, PlaybackStream, RecordStream};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct Signals {
    running: AtomicBool,
    invalidated: AtomicBool,
    failed: AtomicBool,
    released: AtomicBool,
    frames: AtomicU64,
    position: AtomicU64,
    position_valid: AtomicBool,
    host_time: AtomicU64,
    underruns: AtomicU64,
}
impl Signals {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            invalidated: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            released: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            position: AtomicU64::new(0),
            position_valid: AtomicBool::new(false),
            host_time: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
        }
    }
    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
        self.running.store(false, Ordering::Release);
    }
}
#[derive(Clone)]
enum Native {
    Capture(RecordStream),
    Render(PlaybackStream),
}
impl Native {
    fn delete(self) -> Result<(), BackendError> {
        match self {
            Self::Capture(stream) => raw::wait_for(stream.delete(), Duration::from_millis(300)),
            Self::Render(stream) => raw::wait_for(stream.delete(), Duration::from_millis(300)),
        }
    }
    fn timing(&self) -> Result<(i64, u64), BackendError> {
        match self {
            Self::Capture(stream) => {
                raw::wait_for(stream.timing_info(), Duration::from_millis(250))
                    .map(|info| (info.write_offset, 0))
            }
            Self::Render(stream) => raw::wait_for(stream.timing_info(), Duration::from_millis(250))
                .map(|info| (info.read_offset, info.underrun_for)),
        }
    }
}
/// Independent native connection per stream: forced socket shutdown on stop/error cannot
/// interrupt another stream, and releases reactor-owned callback captures even if delete fails.
pub struct LinuxStream {
    native: Option<Native>,
    link: Option<Connection>,
    signals: Arc<Signals>,
    timing: Option<JoinHandle<()>>,
    state: Arc<State>,
    device: DeviceId,
    format: AudioFormat,
}
impl LinuxStream {
    fn finish(
        native: Native,
        link: Connection,
        state: Arc<State>,
        key: Key,
        entry: Entry,
        format: AudioFormat,
        signals: Arc<Signals>,
    ) -> Result<Self, BackendError> {
        state.register(key, entry.generation, &signals)?;
        let worker_native = native.clone();
        let worker_signals = signals.clone();
        let worker_state = state.clone();
        let handle = thread::Builder::new()
            .name("bfa-pulse-time".into())
            .spawn(move || {
                while worker_signals.running.load(Ordering::Acquire) {
                    match worker_native.timing() {
                        Ok((position, underruns)) => {
                            if position >= 0 {
                                worker_signals.position.store(
                                    position as u64 / (u64::from(format.channels) * 4),
                                    Ordering::Release,
                                );
                                worker_signals.position_valid.store(true, Ordering::Release);
                            }
                            if underruns != u64::MAX {
                                worker_signals.underruns.fetch_max(
                                    underruns / (u64::from(format.channels) * 4),
                                    Ordering::AcqRel,
                                );
                            }
                            worker_signals.host_time.store(
                                (worker_state.clock.elapsed().as_nanos() / 100)
                                    .min(u128::from(u64::MAX))
                                    as u64,
                                Ordering::Release,
                            );
                        }
                        Err(BackendError::DeviceGone) => {
                            worker_signals.invalidate();
                            break;
                        }
                        Err(_) => {
                            if worker_signals.running.load(Ordering::Acquire) {
                                worker_signals.failed.store(true, Ordering::Release);
                                worker_signals.running.store(false, Ordering::Release);
                            }
                            break;
                        }
                    }
                    // Leave margin inside the media clock's 100 ms freshness window.
                    // Polling at that exact boundary causes periodic false fallback/unknown depth.
                    thread::park_timeout(Duration::from_millis(25));
                }
            })
            .map_err(|_| BackendError::ServerUnavailable)?;
        Ok(Self {
            native: Some(native),
            link: Some(link),
            signals,
            timing: Some(handle),
            state,
            device: entry.device.id,
            format,
        })
    }
    pub(crate) fn capture(
        config: Arc<Config>,
        state: Arc<State>,
        key: Key,
        entry: Entry,
        source: &str,
        format: AudioFormat,
        callback: CaptureCallback,
    ) -> Result<Self, BackendError> {
        validate_format(format)?;
        let link = config.client()?;
        let signals = Arc::new(Signals::new());
        let adapter = CaptureAdapter::new(callback, signals.clone(), state.clone(), format);
        let params = p::RecordStreamParams {
            source_name: Some(raw::name(source)?),
            sample_spec: sample_spec(format),
            channel_map: channel_map(format),
            cvolume: Some(p::ChannelVolume::norm(format.channels as u8)),
            buffer_attr: p::stream::BufferAttr {
                fragment_size: format.sample_rate / 100 * u32::from(format.channels) * 4,
                max_length: format.sample_rate / 5 * u32::from(format.channels) * 4,
                ..Default::default()
            },
            flags: p::stream::StreamFlags {
                adjust_latency: true,
                no_move: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut adapter = adapter;
        let native = raw::wait(
            link.client
                .create_record_stream(params, move |bytes: &[u8]| adapter.write(bytes)),
        )?;
        if *native.sample_spec() != sample_spec(format) {
            return Err(BackendError::Unsupported);
        }
        Self::finish(
            Native::Capture(native),
            link,
            state,
            key,
            entry,
            format,
            signals,
        )
    }
    pub(crate) fn render(
        config: Arc<Config>,
        state: Arc<State>,
        key: Key,
        entry: Entry,
        format: AudioFormat,
        callback: RenderCallback,
    ) -> Result<Self, BackendError> {
        validate_format(format)?;
        let link = config.client()?;
        let signals = Arc::new(Signals::new());
        let adapter = RenderAdapter {
            callback: Some(callback),
            signals: signals.clone(),
            scratch: [0.0; MAX_CAPTURE_BLOCK_FRAMES * 2],
            pending: [0; 8],
            pending_start: 0,
            pending_len: 0,
            channels: usize::from(format.channels),
        };
        let bytes_per_10ms = format.sample_rate / 100 * u32::from(format.channels) * 4;
        let params = p::PlaybackStreamParams {
            sink_name: Some(raw::name(entry.device.id.as_str())?),
            sample_spec: sample_spec(format),
            channel_map: channel_map(format),
            cvolume: Some(p::ChannelVolume::norm(format.channels as u8)),
            buffer_attr: p::stream::BufferAttr {
                target_length: bytes_per_10ms * 4,
                pre_buffering: bytes_per_10ms,
                minimum_request_length: bytes_per_10ms,
                max_length: bytes_per_10ms * 8,
                fragment_size: u32::MAX,
            },
            flags: p::stream::StreamFlags {
                adjust_latency: true,
                no_move: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let native = raw::wait(link.client.create_playback_stream(params, adapter))?;
        if *native.sample_spec() != sample_spec(format) {
            return Err(BackendError::Unsupported);
        }
        Self::finish(
            Native::Render(native),
            link,
            state,
            key,
            entry,
            format,
            signals,
        )
    }
}
impl Stream for LinuxStream {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn device(&self) -> Option<&DeviceId> {
        Some(&self.device)
    }
    fn feedback_guarantee(&self) -> FeedbackGuarantee {
        FeedbackGuarantee::None
    }
    fn snapshot(&self) -> StreamSnapshot {
        let status = if self.signals.invalidated.load(Ordering::Acquire) {
            StreamStatus::DeviceInvalidated
        } else if self.signals.failed.load(Ordering::Acquire)
            || !self.state.alive.load(Ordering::Acquire)
        {
            StreamStatus::Failed(BackendError::ServerUnavailable)
        } else if !self.signals.running.load(Ordering::Acquire) {
            StreamStatus::Stopped
        } else {
            StreamStatus::Running
        };
        let valid = self.signals.position_valid.load(Ordering::Acquire);
        StreamSnapshot {
            status,
            device_position: valid.then(|| self.signals.position.load(Ordering::Acquire)),
            clock_frequency: valid.then_some(u64::from(self.format.sample_rate)),
            host_time_100ns: valid.then(|| self.signals.host_time.load(Ordering::Acquire)),
            underrun_frames: self.signals.underruns.load(Ordering::Acquire),
            dropped_capture_frames: 0,
        }
    }
    fn stop(&mut self) -> Result<(), BackendError> {
        self.signals.running.store(false, Ordering::Release);
        let mut result = Ok(());
        if let Some(timing) = self.timing.take() {
            timing.thread().unpark();
            if timing.join().is_err() {
                result = Err(BackendError::ServerUnavailable);
            }
        }
        let deleted = self.native.take().map_or(Ok(()), Native::delete);
        if result.is_ok() {
            result = deleted;
        }
        self.link.take(); // Shutdown releases callbacks even on command timeout/disconnection.
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.signals.released.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return Err(BackendError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
        result
    }
}
impl Drop for LinuxStream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn validate_format(format: AudioFormat) -> Result<(), BackendError> {
    if format.sample_rate != 48_000 || !matches!(format.channels, 1 | 2) {
        Err(BackendError::Unsupported)
    } else {
        Ok(())
    }
}
fn sample_spec(format: AudioFormat) -> p::SampleSpec {
    p::SampleSpec {
        format: p::SampleFormat::Float32Le,
        channels: format.channels as u8,
        sample_rate: format.sample_rate,
    }
}
fn channel_map(format: AudioFormat) -> p::ChannelMap {
    if format.channels == 2 {
        p::ChannelMap::stereo()
    } else {
        p::ChannelMap::mono()
    }
}

struct CaptureAdapter {
    callback: Option<CaptureCallback>,
    signals: Arc<Signals>,
    state: Arc<State>,
    format: AudioFormat,
    scratch: [f32; MAX_CAPTURE_BLOCK_FRAMES * 2],
    pending: [u8; 8],
    pending_len: usize,
    origin: u64,
}
impl CaptureAdapter {
    fn new(
        callback: CaptureCallback,
        signals: Arc<Signals>,
        state: Arc<State>,
        format: AudioFormat,
    ) -> Self {
        let origin = state.clock.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        Self {
            callback: Some(callback),
            signals,
            state,
            format,
            scratch: [0.0; MAX_CAPTURE_BLOCK_FRAMES * 2],
            pending: [0; 8],
            pending_len: 0,
            origin,
        }
    }
    fn write(&mut self, mut bytes: &[u8]) {
        // Called under pulseaudio's reactor mutex: ONLY fixed storage, atomics and caller callback.
        if !self.signals.running.load(Ordering::Acquire)
            || !self.state.alive.load(Ordering::Acquire)
        {
            return;
        }
        let frame_bytes = usize::from(self.format.channels) * 4;
        let mut frames = 0;
        while !bytes.is_empty() {
            let count = (frame_bytes - self.pending_len).min(bytes.len());
            self.pending[self.pending_len..self.pending_len + count]
                .copy_from_slice(&bytes[..count]);
            self.pending_len += count;
            bytes = &bytes[count..];
            if self.pending_len == frame_bytes {
                for ch in 0..usize::from(self.format.channels) {
                    self.scratch[frames * usize::from(self.format.channels) + ch] =
                        f32::from_le_bytes(
                            self.pending[ch * 4..ch * 4 + 4]
                                .try_into()
                                .expect("f32 bytes"),
                        );
                }
                self.pending_len = 0;
                frames += 1;
                if frames == MAX_CAPTURE_BLOCK_FRAMES {
                    self.deliver(frames);
                    frames = 0;
                }
            }
        }
        if frames > 0 {
            self.deliver(frames);
        }
    }
    fn deliver(&mut self, frames: usize) {
        let position = self
            .signals
            .frames
            .fetch_add(frames as u64, Ordering::AcqRel);
        let timestamp = self.origin.saturating_add(
            (u128::from(position) * 1_000_000_000 / u128::from(self.format.sample_rate))
                .min(u128::from(u64::MAX)) as u64,
        );
        if let Some(callback) = self.callback.as_mut() {
            callback(CaptureBlock {
                samples: &self.scratch[..frames * usize::from(self.format.channels)],
                frames,
                format: self.format,
                timestamp_ns: timestamp,
                timestamp_valid: true,
                device_position: Some(position),
                discontinuity: false,
            });
        }
    }
}
impl Drop for CaptureAdapter {
    fn drop(&mut self) {
        self.callback.take();
        self.signals.released.store(true, Ordering::Release);
    }
}
struct RenderAdapter {
    callback: Option<RenderCallback>,
    signals: Arc<Signals>,
    scratch: [f32; MAX_CAPTURE_BLOCK_FRAMES * 2],
    pending: [u8; 8],
    pending_start: usize,
    pending_len: usize,
    channels: usize,
}
impl PlaybackSource for RenderAdapter {
    fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, output: &mut [u8]) -> Poll<usize> {
        if output.is_empty() {
            return Poll::Pending;
        }
        if !self.signals.running.load(Ordering::Acquire) {
            output.fill(0);
            return Poll::Ready(output.len());
        }
        let mut written = 0;
        while self.pending_len > 0 && written < output.len() {
            output[written] = self.pending[self.pending_start];
            self.pending_start += 1;
            self.pending_len -= 1;
            written += 1;
        }
        if written == output.len() {
            return Poll::Ready(written);
        }
        let frame_bytes = self.channels * 4;
        let frames = ((output.len() - written) / frame_bytes).clamp(1, MAX_CAPTURE_BLOCK_FRAMES);
        let samples = frames * self.channels;
        let this = &mut *self;
        this.scratch[..samples].fill(0.0);
        if let Some(callback) = this.callback.as_mut() {
            callback(&mut this.scratch[..samples]);
        }
        this.signals
            .frames
            .fetch_add(frames as u64, Ordering::AcqRel);
        for index in 0..samples {
            let sample = if this.scratch[index].is_finite() {
                this.scratch[index]
            } else {
                0.0
            };
            for byte in sample.to_le_bytes() {
                if written < output.len() {
                    output[written] = byte;
                    written += 1;
                } else {
                    if this.pending_len == 0 {
                        this.pending_start = 0;
                    }
                    this.pending[this.pending_len] = byte;
                    this.pending_len += 1;
                }
            }
        }
        Poll::Ready(written)
    }
}
impl Drop for RenderAdapter {
    fn drop(&mut self) {
        self.callback.take();
        self.signals.released.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_fragments_are_frame_aligned_bounded_and_monotonic() {
        let count = Arc::new(AtomicU64::new(0));
        let callback_count = count.clone();
        let maximum = Arc::new(AtomicU64::new(0));
        let callback_maximum = maximum.clone();
        let last = Arc::new(AtomicU64::new(0));
        let callback_last = last.clone();
        let signals = Arc::new(Signals::new());
        let callback = Box::new(move |block: CaptureBlock<'_>| {
            assert_eq!(
                block.device_position,
                Some(callback_count.fetch_add(block.frames as u64, Ordering::AcqRel))
            );
            callback_maximum.fetch_max(block.frames as u64, Ordering::AcqRel);
            assert!(block.timestamp_valid);
            assert!(block.timestamp_ns >= callback_last.swap(block.timestamp_ns, Ordering::AcqRel));
            assert_eq!(block.samples.len(), block.frames * 2);
            assert!(block.samples.iter().all(|x| *x == 0.25));
        });
        let mut adapter = CaptureAdapter::new(
            callback,
            signals.clone(),
            Arc::new(State::new()),
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        );
        let bytes = vec![0.25f32; 2001 * 2]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        adapter.write(&bytes[..3]);
        adapter.write(&bytes[3..]);
        assert_eq!(count.load(Ordering::Acquire), 2001);
        assert_eq!(maximum.load(Ordering::Acquire), 960);
        drop(adapter);
        assert!(signals.released.load(Ordering::Acquire));
    }
    #[test]
    fn playback_partial_byte_requests_preserve_stereo_and_release_capture() {
        let signals = Arc::new(Signals::new());
        let mut adapter = RenderAdapter {
            callback: Some(Box::new(|samples| {
                assert!(samples.len() <= 1920);
                for frame in samples.as_chunks_mut::<2>().0 {
                    frame[0] = 0.25;
                    frame[1] = -0.25;
                }
            })),
            signals: signals.clone(),
            scratch: [0.0; 1920],
            pending: [0; 8],
            pending_start: 0,
            pending_len: 0,
            channels: 2,
        };
        let mut bytes = Vec::new();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        for size in [1, 3, 7, 100, 8000, 17] {
            let mut output = vec![0; size];
            let Poll::Ready(n) = Pin::new(&mut adapter).poll_read(&mut cx, &mut output) else {
                panic!("unexpected pending");
            };
            bytes.extend_from_slice(&output[..n]);
        }
        for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                f32::from_le_bytes(*chunk),
                if index % 2 == 0 { 0.25 } else { -0.25 }
            );
        }
        drop(adapter);
        assert!(signals.released.load(Ordering::Acquire));
    }
}

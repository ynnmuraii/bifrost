//! One native output, independently authenticated receivers, and an off-RT summing worker.
use super::*;
use crate::mixer::{Mixer, MAX_INPUTS};
use rtrb::Producer;
use std::{collections::VecDeque, sync::OnceLock};

// Matches the child lifetime admission bound. Entries retain only callback gates, never keys.
// At cap 3, eight precomputed subsets preserve other lanes when a queued lane is revoked.
// This trades bounded ring storage/off-RT sums for a callback containing only selection/gates.
const MAX_JOINS: usize = 256;
const NO_ENTRY: usize = usize::MAX;
#[derive(Clone, Copy)]
struct MixedFrame {
    pcm: [[f32; 2]; 1 << MAX_INPUTS],
    entries: [usize; MAX_INPUTS],
}
struct Gates {
    entries: [OnceLock<Arc<Shared>>; MAX_JOINS],
}
fn select_frame(frame: &MixedFrame, gates: &Gates) -> [f32; 2] {
    let entries = frame.entries.map(|index| {
        if index == NO_ENTRY {
            None
        } else {
            gates.entries[index].get()
        }
    });
    let mut allowed = 0;
    for (slot, entry) in entries.iter().enumerate() {
        if entry.is_some_and(|entry| !entry.is_stopped()) {
            allowed |= 1 << slot;
        }
    }
    let pcm = frame.pcm[allowed];
    if pcm.iter().any(|sample| sample.abs() > 0.8) {
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(entry) = entry.as_ref().filter(|_| allowed & (1 << slot) != 0) {
                entry.callback_limiter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pcm
}
struct Input {
    shared: Arc<Shared>,
    events: SyncSender<MediaEvent>,
    receiver: Arc<Mutex<Receiver>>,
    decoded: Vec<f32>,
    queue: VecDeque<f32>,
    converter: Option<NativeRate>,
    converted: Vec<f32>,
    staging: [f32; 480],
    staged: usize,
    start: Instant,
    callback_underrun_base: u64,
    gate: usize,
}
struct Inputs {
    slots: [Option<Input>; MAX_INPUTS],
    mixer: Mixer,
}
/// Owns the Listener child's output selection and native stream. Joining does not reopen it.
/// Ethernet and Wi-Fi receivers may coexist; each retains its own jitter/codec/drift state.
/// A different render request is refused rather than silently moving existing sessions.
/// At most 256 sessions are admitted over a group's lifetime, matching the child resource bound.
/// Owner polling closes the native output after the final session ends; create a new group to restart.
pub struct ListenerGroup<S: Stream + 'static> {
    output: MediaSession<S>,
    request: RenderRequest,
    inputs: Arc<Mutex<Inputs>>,
    gates: Arc<Gates>,
    native: Arc<Mutex<StreamSnapshot>>,
    joined: usize,
    stop_error: Option<EngineError>,
}
impl<S: Stream + 'static> ListenerGroup<S> {
    pub fn new<B: Backend<Stream = S>>(
        backend: &mut B,
        request: RenderRequest,
    ) -> Result<Self, EngineError> {
        let shared = Arc::new(Shared::new(Arc::new(AtomicBool::new(true))));
        let gates = Arc::new(Gates {
            entries: std::array::from_fn(|_| OnceLock::new()),
        });
        let (producer, mut consumer) = RingBuffer::<MixedFrame>::new(PCM_RING_SAMPLES / 2);
        let callback_shared = shared.clone();
        let callback_gates = gates.clone();
        let mut stream = backend.start_render(
            request.clone(),
            Box::new(move |output| {
                if callback_shared.is_stopped() || !output.len().is_multiple_of(2) {
                    output.fill(0.0);
                    if !output.len().is_multiple_of(2) {
                        callback_shared
                            .callback_invalid
                            .store(true, Ordering::Release);
                    }
                    return;
                }
                let mut underrun = false;
                for frame in output.as_chunks_mut::<2>().0 {
                    match consumer.pop() {
                        Ok(value) => *frame = select_frame(&value, &callback_gates),
                        Err(_) => {
                            frame.fill(0.0);
                            underrun = true;
                        }
                    }
                }
                if underrun {
                    callback_shared
                        .callback_underruns
                        .fetch_add(1, Ordering::Relaxed);
                }
                let total = callback_shared
                    .render_frames
                    .fetch_add((output.len() / 2) as u64, Ordering::AcqRel)
                    + (output.len() / 2) as u64;
                let rate = callback_shared.native_rate.load(Ordering::Acquire);
                if rate > 0 {
                    callback_shared.render_clock.fetch_max(
                        total.saturating_mul(48_000) / u64::from(rate),
                        Ordering::AcqRel,
                    );
                }
            }),
        )?;
        let format = stream.format();
        if !valid_format(format) {
            stream.stop()?;
            return Err(EngineError::InvalidFormat);
        }
        shared
            .native_rate
            .store(format.sample_rate, Ordering::Release);
        let (tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let native = Arc::new(Mutex::new(stream.snapshot()));
        let mut output = make_session(stream, shared.clone(), events, tx, false);
        let inputs = Arc::new(Mutex::new(Inputs {
            slots: std::array::from_fn(|_| None),
            mixer: Mixer::new(format.sample_rate.div_ceil(100) as usize),
        }));
        let mix_inputs = inputs.clone();
        output.workers.push(
            thread::Builder::new()
                .name("bfa-mix".into())
                .spawn(move || {
                    mix_loop(shared, mix_inputs, producer, format.sample_rate);
                })?,
        );
        Ok(Self {
            output,
            request,
            inputs,
            gates,
            native,
            joined: 0,
            stop_error: None,
        })
    }
    pub fn accepts(&self, request: &RenderRequest) -> bool {
        self.request == *request
    }
    pub fn stream_mut(&mut self) -> Option<&mut S> {
        self.output.stream_mut()
    }
    pub fn refresh(&mut self) {
        self.output.refresh();
        *lock(&self.native) = self.output.last_native;
        let inputs = lock(&self.inputs);
        for input in inputs.slots.iter().flatten() {
            input.shared.render_clock.store(
                self.output.shared.render_clock.load(Ordering::Acquire),
                Ordering::Release,
            );
            if self.output.shared.is_stopped() {
                input.shared.invalidate(&input.events);
            }
        }
        let empty = self.joined > 0
            && inputs
                .slots
                .iter()
                .flatten()
                .all(|input| input.shared.is_stopped());
        drop(inputs);
        if empty && !self.output.stop_attempted {
            let before = self.output.last_native;
            if let Err(error) = self.output.stop() {
                self.stop_error = Some(error);
            }
            // Closing the failed client must not erase its terminal device diagnosis.
            if matches!(
                before.status,
                StreamStatus::DeviceInvalidated | StreamStatus::Failed(_)
            ) {
                self.output.last_native = before;
            }
            *lock(&self.native) = self.output.last_native;
        }
    }
    pub fn on_backend_event(&mut self, event: &BackendEvent) {
        let changed_default = matches!(event, BackendEvent::Device(DeviceEvent::DefaultChanged {
            direction: audio_backend::Direction::Render, role,
        }) if self.request.device == audio_backend::DeviceSelection::FollowDefault(*role));
        if changed_default {
            self.output.shared.invalidate(&self.output.event_tx);
        }
        self.output.on_backend_event(event);
        self.refresh();
    }
    pub fn join(
        &mut self,
        config: SessionConfig,
        material: SessionMaterial,
    ) -> Result<MediaSession<S>, EngineError> {
        self.refresh();
        if !config.authorized.load(Ordering::Acquire) {
            return Err(EngineError::Unauthorized);
        }
        if self.output.shared.is_stopped() {
            return Err(EngineError::Backend(BackendError::DeviceGone));
        }
        if self.joined == MAX_JOINS {
            return Err(EngineError::GroupFull);
        }
        let start = Instant::now();
        let shared = Arc::new(Shared::new(config.authorized));
        let endpoint = config.socket.register(
            config.session_id,
            config.peer,
            shared.socket_rejected.clone(),
            shared.notification.clone(),
        )?;
        let (tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let keys = SessionKeys::new(
            Endpoint::MediaReceiver,
            config.session_id,
            0,
            material.forward,
            material.reverse,
        )
        .map_err(MediaError::Crypto)?;
        let receiver = Arc::new(Mutex::new(Receiver::new(
            config.profile,
            keys.receive,
            u32::from(config.profile.frame_samples()),
        )?));
        let capacity = lock(&receiver).output_capacity_samples();
        let rate = self.output.shared.native_rate.load(Ordering::Acquire);
        shared.native_rate.store(rate, Ordering::Release);
        let converter = if rate == 48_000 {
            None
        } else {
            Some(NativeRate::new(48_000, rate, 240, FixedAsync::Input)?)
        };
        let converted = vec![0.0; converter.as_ref().map_or(0, NativeRate::max_output_frames) * 2];
        let mut worker = ReceiveWorker {
            context: Context {
                endpoint,
                shared: shared.clone(),
                events: tx.clone(),
                start,
            },
            receiver: receiver.clone(),
            sealer: keys.send,
            path: PathState::new(start)?,
        };
        let mut inputs = lock(&self.inputs);
        let slot = inputs
            .slots
            .iter()
            .position(|input| input.as_ref().is_none_or(|input| input.shared.is_stopped()))
            .ok_or(EngineError::GroupFull)?;
        self.gates.entries[self.joined]
            .set(shared.clone())
            .map_err(|_| EngineError::GroupFull)?;
        inputs.mixer.reset_input(slot);
        inputs.slots[slot] = Some(Input {
            shared: shared.clone(),
            events: tx.clone(),
            receiver,
            decoded: vec![0.0; capacity],
            queue: VecDeque::with_capacity(PCM_RING_SAMPLES),
            converter,
            converted,
            staging: [0.0; 480],
            staged: 0,
            start,
            callback_underrun_base: self
                .output
                .shared
                .callback_underruns
                .load(Ordering::Acquire),
            gate: self.joined,
        });
        self.joined += 1;
        drop(inputs);
        let mut session = MediaSession {
            stream: None,
            shared,
            events,
            event_tx: tx,
            workers: Vec::with_capacity(1),
            native_stop: None,
            initial_position: None,
            last_native: *lock(&self.native),
            source: false,
            group_native: Some(self.native.clone()),
            group_owner: None,
            stop_attempted: false,
        };
        if let Err(reason) = worker.send_probe(&mut [0; MAX_DATAGRAM_LEN]) {
            worker.context.finish(Err(reason));
            return Ok(session);
        }
        session.workers.push(
            thread::Builder::new()
                .name("bfa-receive".into())
                .spawn(move || worker.run())?,
        );
        Ok(session)
    }
    pub fn stop(&mut self) -> Result<(), EngineError> {
        for input in lock(&self.inputs).slots.iter().flatten() {
            input.shared.request_stop(StopReason::User);
        }
        let result = self.output.stop();
        self.stop_error.take().map_or(result, Err)
    }
}
impl<S: Stream + 'static> Drop for ListenerGroup<S> {
    fn drop(&mut self) {
        if !self.output.stop_attempted {
            let _ = self.stop();
        }
    }
}
fn mix_loop(
    shared: Arc<Shared>,
    inputs: Arc<Mutex<Inputs>>,
    mut output: Producer<MixedFrame>,
    rate: u32,
) {
    let block = (rate / 100).max(1) as usize;
    let mut scratch: [Vec<f32>; MAX_INPUTS] = std::array::from_fn(|_| vec![0.0; block * 2]);
    let mut mixed = vec![[[0.0; 2]; 1 << MAX_INPUTS]; block];
    while !shared.is_stopped() {
        if shared.callback_invalid.load(Ordering::Acquire) {
            shared.request_stop(StopReason::DeviceInvalidated);
            break;
        }
        let now = shared.render_clock.load(Ordering::Acquire);
        let mut inputs = lock(&inputs);
        for slot in 0..MAX_INPUTS {
            let Some(input) = inputs.slots[slot].as_mut() else {
                continue;
            };
            input.shared.render_clock.store(now, Ordering::Release);
            input.shared.callback_underruns.store(
                shared
                    .callback_underruns
                    .load(Ordering::Acquire)
                    .saturating_sub(input.callback_underrun_base),
                Ordering::Release,
            );
            if input.shared.is_stopped() {
                input.shared.finish(&input.events);
                inputs.slots[slot] = None;
                continue;
            }
            let fill = input.queue.len() as f64 / 2.0 * 48_000.0 / f64::from(rate)
                + (PCM_RING_SAMPLES / 2 - output.slots()) as f64 * 48_000.0 / f64::from(rate);
            let result = lock(&input.receiver).playout(now, fill, &mut input.decoded);
            match result {
                Ok(n) => {
                    let mut overflow = false;
                    if let Some(converter) = input.converter.as_mut() {
                        for &sample in &input.decoded[..n * 2] {
                            input.staging[input.staged] = sample;
                            input.staged += 1;
                            if input.staged == input.staging.len() {
                                match converter.process(&input.staging, &mut input.converted) {
                                    Ok(frames) => {
                                        if input.queue.len() + frames * 2 <= PCM_RING_SAMPLES {
                                            input.queue.extend(&input.converted[..frames * 2]);
                                        } else {
                                            overflow = true;
                                        }
                                    }
                                    Err(error) => input.shared.request_stop(media_reason(error)),
                                }
                                input.staged = 0;
                            }
                        }
                    } else if input.queue.len() + n * 2 <= PCM_RING_SAMPLES {
                        input.queue.extend(&input.decoded[..n * 2]);
                    } else {
                        overflow = true;
                    }
                    if overflow {
                        input.shared.degraded(&input.events);
                    }
                }
                Err(error) => input.shared.request_stop(media_reason(error)),
            }
            let health = lock(&input.receiver).health();
            let mut stats = lock(&input.shared.stats);
            let lost = health.lost_frames > stats.health.lost_frames;
            stats.health = health;
            drop(stats);
            if lost
                || input.shared.started.load(Ordering::Acquire)
                    && input.start.elapsed().as_millis() as u64
                        > input.shared.last_media_ms.load(Ordering::Acquire) + 1000
            {
                input.shared.degraded(&input.events);
            }
        }
        // Keep one 10 ms aggregate block queued; never build latency during a stalled device.
        if output.slots() >= block && PCM_RING_SAMPLES / 2 - output.slots() < block {
            let entries = std::array::from_fn(|slot| {
                inputs.slots[slot]
                    .as_ref()
                    .map_or(NO_ENTRY, |input| input.gate)
            });
            for (slot, buffer) in scratch.iter_mut().enumerate() {
                buffer.fill(0.0);
                let gain = if let Some(input) = inputs.slots[slot].as_mut() {
                    if !input.shared.is_stopped() {
                        for sample in buffer.iter_mut() {
                            *sample = input.queue.pop_front().unwrap_or(0.0);
                        }
                        if input.shared.muted.load(Ordering::Acquire) {
                            0.0
                        } else {
                            f32::from_bits(input.shared.gain.load(Ordering::Acquire))
                        }
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let _ = inputs.mixer.set_gain(slot, gain);
            }
            if inputs
                .mixer
                .process_subsets(&[&scratch[0], &scratch[1], &scratch[2]], &mut mixed)
                .is_err()
            {
                shared.request_stop(StopReason::WorkerFailed);
                break;
            }
            for &pcm in &mixed {
                let _ = output.push(MixedFrame { pcm, entries });
            }
        }
        drop(inputs);
        thread::sleep(Duration::from_millis(1));
    }
    for input in lock(&inputs).slots.iter().flatten() {
        input.shared.request_stop(shared.reason());
        input.shared.finish(&input.events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_backend::{
        dry::{DryBackend, ScriptEvent},
        Capabilities, Device, DeviceId, DeviceSelection, Direction,
    };

    #[test]
    fn queued_revocation_selects_remaining_pcm_and_counts_only_audible_limiting() {
        let gates = Gates {
            entries: std::array::from_fn(|_| OnceLock::new()),
        };
        let members: [Arc<Shared>; MAX_INPUTS] = std::array::from_fn(|index| {
            let shared = Arc::new(Shared::new(Arc::new(AtomicBool::new(true))));
            assert!(gates.entries[index].set(shared.clone()).is_ok());
            shared
        });
        let mut mixer = Mixer::new(480);
        let mut variants = [[[0.0; 2]; 1 << MAX_INPUTS]; 1];
        mixer
            .process_subsets(&[&[0.6; 2]; MAX_INPUTS], &mut variants)
            .unwrap();
        let queued = MixedFrame {
            pcm: variants[0],
            entries: [0, 1, 2],
        };
        let full = select_frame(&queued, &gates);
        assert!(full[0] > 0.8 && full[0] < 1.0);
        for member in &members {
            assert_eq!(member.callback_limiter.load(Ordering::Acquire), 1);
        }
        members[0].authorized.store(false, Ordering::Release);
        let remaining = select_frame(&queued, &gates);
        assert_eq!(remaining, queued.pcm[6]);
        assert!(remaining[0] > 0.6 && remaining[0] < 0.8);
        for member in &members {
            assert_eq!(member.callback_limiter.load(Ordering::Acquire), 1);
        }
        // A replacement lane cannot make an old queued frame use the new authorization.
        let replacement = Arc::new(Shared::new(Arc::new(AtomicBool::new(true))));
        assert!(gates.entries[3].set(replacement).is_ok());
        assert_eq!(select_frame(&queued, &gates), remaining);
    }

    #[test]
    fn render_callback_does_not_take_the_mixing_control_lock() {
        let mut backend = DryBackend::new(Capabilities::default());
        let id = DeviceId::new("DRY_OUTPUT").unwrap();
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        backend
            .apply(ScriptEvent::UpsertDevice(Device {
                id: id.clone(),
                label: "Dry output".into(),
                direction: Direction::Render,
                format,
            }))
            .unwrap();
        let mut group = ListenerGroup::new(
            &mut backend,
            RenderRequest {
                device: DeviceSelection::Explicit(id),
                requested: format,
            },
        )
        .unwrap();
        let mut stream = group.output.stream.take().unwrap();
        let guard = lock(&group.inputs);
        let (tx, rx) = mpsc::sync_channel(1);
        let callback = thread::spawn(move || {
            let result = stream.pump(480);
            assert!(tx.send((result, stream)).is_ok());
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(guard);
        let (result, stream) = result.expect("callback waited for the mixing lock");
        result.unwrap();
        group.output.stream = Some(stream);
        callback.join().unwrap();
        group.stop().unwrap();
    }
}

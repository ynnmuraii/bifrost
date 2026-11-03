//! Outbound classification, bounded queues and motion coalescing.
//!
//! The live link keeps one latest absolute position outside its bounded ordered FIFO.
//! Discrete admission freezes that position atomically before the edge; lease and ownership
//! fences retain their dependency order. Urgent controls have reserved bounded space and
//! eight-to-one whole-frame fairness. Critical admission overflow closes the link instead
//! of silently discarding an edge. Pure classification/coalescing helpers below describe
//! the vocabulary; live admission is implemented by the shared queue state.
//! Handshake refusal has one reserved slot behind Hello, bypassing readiness without
//! releasing normal traffic; admitting it discards normal traffic and seals admission.

use focus_router as fr;
use input_model::InputEvent;

/// Priority class of an outbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// Handshake, lease, ownership and liveness control.
    Control,
    /// Key/button/scroll edges, and any batch that contains one: never dropped, because a
    /// lost key-up is exactly the stuck-state failure the product promises to avoid.
    Edge,
    /// Pure pointer motion, coalescible without crossing a discrete edge.
    Motion,
}

/// Classifies a message for the outbound queue.
pub fn classify(message: &fr::Message) -> MessageClass {
    match message {
        fr::Message::InputBatch { events, .. } => {
            if events.iter().all(is_motion) && !events.is_empty() {
                MessageClass::Motion
            } else {
                MessageClass::Edge
            }
        }
        _ => MessageClass::Control,
    }
}

/// Whether an event is coalescible pointer motion.
///
/// Scroll and buttons are *not* motion: they are discrete edges that the receiver must see
/// one by one.
pub fn is_motion(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::PointerAbsolute(_) | InputEvent::PointerRelative { .. }
    )
}

/// Merges consecutive motion events, keeping everything else in order.
///
/// * consecutive **absolute** positions collapse to the last one (the newest position
///   supersedes the older ones);
/// * consecutive **relative** deltas are summed (dropping one would lose movement);
/// * a run never spans a non-motion event, and absolute and relative runs are not merged
///   with each other.
pub fn coalesce_motion(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for event in events {
        match (out.last_mut(), event) {
            (Some(InputEvent::PointerAbsolute(previous)), InputEvent::PointerAbsolute(next)) => {
                *previous = next;
            }
            (
                Some(InputEvent::PointerRelative { dx, dy }),
                InputEvent::PointerRelative { dx: ndx, dy: ndy },
            ) => {
                *dx = dx.saturating_add(ndx);
                *dy = dy.saturating_add(ndy);
            }
            (_, event) => out.push(event),
        }
    }
    out
}

/// Merges several already-contiguous motion batches into one event list.
pub fn merge_motion_batches(batches: Vec<Vec<InputEvent>>) -> Vec<InputEvent> {
    coalesce_motion(batches.into_iter().flatten().collect())
}

/// Splits an `InputBatch` into coalescible motion or guaranteed discrete/control vocabulary.
pub fn outbound_of(message: fr::Message) -> Outbound {
    match (&message, classify(&message)) {
        (
            fr::Message::InputBatch {
                lease_id, events, ..
            },
            MessageClass::Motion,
        ) => Outbound::Motion {
            lease_id: *lease_id,
            events: events.clone(),
        },
        _ => Outbound::Guaranteed(message),
    }
}

/// The batch the link writes for one merged motion run.
///
/// `lease_id` is the lease the coalescible [`Outbound::Motion`] items carried. A motion-only
/// batch used to be rebuilt with an empty lease id, and the receiver — which validates the
/// lease on every `InputBatch` — answered `LeaseMismatch` to every coalesced pointer frame
/// while the key/button frames (which travel guaranteed, keeping their lease) went through.
/// `input_batch_seq` is stamped by the transport from the envelope sequence, exactly as for
/// a batch that never went through the queue.
pub fn motion_batch(lease_id: [u8; 16], events: Vec<InputEvent>) -> fr::Message {
    fr::Message::InputBatch {
        lease_id,
        input_batch_seq: 0,
        events: coalesce_motion(events),
    }
}

/// One queued item.
#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    /// Must be delivered: control, or any batch with an edge in it.
    Guaranteed(fr::Message),
    /// Bounded audio metadata; admission failure never closes input.
    AudioControl(protocol::wire::AudioControl),
    /// Replacement capability advertisement: guaranteed control, never coalesced.
    CapabilitiesChanged(Vec<i32>),
    /// Remote update control: guaranteed, never coalesced or dropped.
    UpdateRequest(protocol::wire::UpdateRequest),
    /// Remote update progress: guaranteed, never coalesced or dropped.
    UpdateResult(protocol::wire::UpdateResult),
    /// Backend clipboard application result: guaranteed control.
    ClipboardApplied(protocol::wire::ClipboardApplied),
    /// Metadata-only clipboard routing: guaranteed control.
    ClipboardRoute(protocol::wire::ClipboardRoute),
    /// A layout-document message: always guaranteed, never coalesced (KVM-072).
    Layout(crate::convert::LayoutMessage),
    /// A clipboard message: always guaranteed, never coalesced, never dropped. The grant is the
    /// only clipboard message the input link carries, and it decides whether the channel it
    /// authorises ever exists, so it may not be lost behind motion (`KVM-042`).
    Clipboard(crate::clipboard::ClipboardMessage),
    /// Coalescible pointer motion, represented by the live queue's latest-position slot.
    ///
    /// The `lease_id` travels with the events so the link can rebuild the batch it writes
    /// with the lease the motion belongs to. The receiver validates that lease on every
    /// `InputBatch`; rebuilding with an empty id made every coalesced motion frame a
    /// `LeaseMismatch`, which is why pointer motion alone never reached the peer while the
    /// key/button batches (guaranteed, lease intact) did.
    Motion {
        /// The lease the motion events belong to.
        lease_id: [u8; 16],
        /// The motion events.
        events: Vec<InputEvent>,
    },
}

impl Outbound {
    /// The event list, when this is motion.
    pub fn motion_events(&self) -> Option<&[InputEvent]> {
        match self {
            Outbound::Motion { events, .. } => Some(events),
            Outbound::Guaranteed(_)
            | Outbound::AudioControl(_)
            | Outbound::CapabilitiesChanged(_)
            | Outbound::UpdateRequest(_)
            | Outbound::UpdateResult(_)
            | Outbound::ClipboardApplied(_)
            | Outbound::ClipboardRoute(_)
            | Outbound::Layout(_)
            | Outbound::Clipboard(_) => None,
        }
    }

    /// The lease the motion belongs to, when this is motion.
    pub fn motion_lease(&self) -> Option<[u8; 16]> {
        match self {
            Outbound::Motion { lease_id, .. } => Some(*lease_id),
            Outbound::Guaranteed(_)
            | Outbound::AudioControl(_)
            | Outbound::CapabilitiesChanged(_)
            | Outbound::UpdateRequest(_)
            | Outbound::UpdateResult(_)
            | Outbound::ClipboardApplied(_)
            | Outbound::ClipboardRoute(_)
            | Outbound::Layout(_)
            | Outbound::Clipboard(_) => None,
        }
    }
}

use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Timing stays attached to the event through replacement and edge barriers.
#[derive(Debug, Clone)]
pub(crate) struct InputSample {
    pub event: InputEvent,
    pub capture_host_us: Option<u64>,
    pub capture_to_enqueue_us: Option<u64>,
    pub enqueued_at: Instant,
}

#[derive(Debug)]
pub(crate) enum Intent {
    Hello(protocol::wire::Envelope),
    Refuse {
        connection_id: [u8; 16],
        code: protocol::wire::ErrorCode,
        related_seq: u64,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    Payload(protocol::wire::envelope::Payload),
    Outbound(Outbound),
    Input {
        generation: u64,
        lease_id: [u8; 16],
        samples: Vec<InputSample>,
    },
    Unpair(protocol::wire::Unpaired, tokio::sync::oneshot::Sender<bool>),
}

const AUXILIARY_INPUT_BURST: u8 = 8;
#[cfg(test)]
pub(crate) fn is_focus(message: &protocol::wire::AudioControl) -> bool {
    matches!(
        message.body,
        Some(
            protocol::wire::audio_control::Body::Focus(_)
                | protocol::wire::audio_control::Body::FocusRequest(_)
        )
    )
}

pub(crate) fn audio_capability(
    message: &protocol::wire::AudioControl,
) -> protocol::wire::Capability {
    use protocol::wire::{audio_control::Body, Capability};
    match message.body {
        Some(Body::Focus(_) | Body::FocusRequest(_)) => Capability::AudioFocusReportV1,
        Some(Body::Rekey(_)) => Capability::AudioRekeyV1,
        Some(Body::Artwork(_) | Body::ArtworkRequest(_)) => Capability::MediaArtworkV1,
        Some(Body::MicControl(_) | Body::MicState(_) | Body::MicAck(_)) => Capability::MicControlV1,
        Some(Body::MediaInfo(_) | Body::MediaCommand(_) | Body::MediaAck(_)) => {
            Capability::MediaControlV1
        }
        _ => Capability::AudioControlV1,
    }
}

fn has_artwork_id(message: &protocol::wire::AudioControl) -> bool {
    matches!(&message.body, Some(protocol::wire::audio_control::Body::MediaInfo(info)) if !info.artwork_id.is_empty())
}
fn is_seek(message: &protocol::wire::AudioControl) -> bool {
    matches!(&message.body,
        Some(protocol::wire::audio_control::Body::MediaCommand(command))
            if command.command == protocol::wire::MediaCommandKind::Seek as i32)
}

#[derive(Debug)]
struct QueueState {
    generation: u64,
    active_lease: Option<[u8; 16]>,
    accepting: bool,
    minimum_generation: u64,
    selected_input: Option<(u64, [u8; 16])>,
    invalidated_selection: Option<u64>,
    boundary_seen: bool,
    unpair_allowed: bool,
    closed: bool,
    refusing: bool,
    ready: bool,
    close_deadline: Option<Instant>,
    hello: Option<Intent>,
    refusal: Option<Intent>,
    latest: Option<(u64, [u8; 16], InputSample)>,
    admission: u64,
    ordered: VecDeque<(u64, Intent)>,
    urgent: VecDeque<Intent>,
    auxiliary: VecDeque<(u64, Intent)>,
    audio_stops: VecDeque<Intent>,
    local_audio: bool,
    remote_audio: bool,
    local_focus: bool,
    remote_focus: bool,
    local_mic: bool,
    remote_mic: bool,
    local_media: bool,
    remote_media: bool,
    local_seek: bool,
    remote_seek: bool,
    local_rekey: bool,
    remote_rekey: bool,
    local_artwork: bool,
    remote_artwork: bool,
    urgent_run: u8,
    input_run: u8,
}

impl QueueState {
    fn audio_allowed(&self, message: &protocol::wire::AudioControl) -> bool {
        self.ready
            && !self.closed
            && !self.refusing
            && self.family_allowed(audio_capability(message))
            && (!is_seek(message) || self.local_seek && self.remote_seek)
            && (!has_artwork_id(message) || self.local_artwork && self.remote_artwork)
    }

    fn family_allowed(&self, capability: protocol::wire::Capability) -> bool {
        use protocol::wire::Capability;
        match capability {
            Capability::AudioRekeyV1 => {
                self.local_audio && self.remote_audio && self.local_rekey && self.remote_rekey
            }
            Capability::MediaArtworkV1 => {
                self.local_media && self.remote_media && self.local_artwork && self.remote_artwork
            }
            Capability::MicControlV1 => self.local_mic && self.remote_mic,
            Capability::MediaControlV1 => self.local_media && self.remote_media,
            Capability::AudioFocusReportV1 => self.local_focus && self.remote_focus,
            _ => self.local_audio && self.remote_audio,
        }
    }

    fn purge_audio(&mut self) {
        let allowed = |message: &protocol::wire::AudioControl| {
            (!is_seek(message) || self.local_seek && self.remote_seek)
                && (!has_artwork_id(message) || self.local_artwork && self.remote_artwork)
                && match audio_capability(message) {
                    protocol::wire::Capability::AudioRekeyV1 => {
                        self.local_audio
                            && self.remote_audio
                            && self.local_rekey
                            && self.remote_rekey
                    }
                    protocol::wire::Capability::MediaArtworkV1 => {
                        self.local_media
                            && self.remote_media
                            && self.local_artwork
                            && self.remote_artwork
                    }
                    protocol::wire::Capability::MicControlV1 => self.local_mic && self.remote_mic,
                    protocol::wire::Capability::MediaControlV1 => {
                        self.local_media && self.remote_media
                    }
                    protocol::wire::Capability::AudioFocusReportV1 => {
                        self.local_focus && self.remote_focus
                    }
                    _ => self.local_audio && self.remote_audio,
                }
        };
        self.auxiliary.retain(|(_, intent)| match intent {
            Intent::Outbound(Outbound::AudioControl(message)) => allowed(message),
            _ => true,
        });
        self.audio_stops.retain(|intent| match intent {
            Intent::Outbound(Outbound::AudioControl(message)) => allowed(message),
            _ => true,
        });
    }

    fn next_admission(&mut self) -> Result<u64, crate::TransportError> {
        self.admission = self
            .admission
            .checked_add(1)
            .ok_or_else(|| crate::TransportError::Protocol("admission index exhausted".into()))?;
        Ok(self.admission)
    }
}

/// All admission and pre-edge freezing use one short lock. No network waiter lives here.
#[derive(Debug)]
pub(crate) struct InputQueue {
    state: Mutex<QueueState>,
    capacity: usize,
    pub wake: Notify,
}

impl InputQueue {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            state: Mutex::new(QueueState {
                generation: 0,
                active_lease: None,
                accepting: false,
                closed: false,
                refusing: false,
                minimum_generation: 0,
                boundary_seen: false,
                unpair_allowed: false,
                selected_input: None,
                invalidated_selection: None,
                ready: false,
                hello: None,
                refusal: None,
                latest: None,
                close_deadline: None,
                admission: 0,
                ordered: VecDeque::new(),
                urgent: VecDeque::new(),
                auxiliary: VecDeque::new(),
                audio_stops: VecDeque::new(),
                local_audio: false,
                remote_audio: false,
                local_focus: false,
                remote_focus: false,
                local_mic: false,
                remote_mic: false,
                local_media: false,
                remote_media: false,
                local_seek: false,
                remote_seek: false,
                local_rekey: false,
                remote_rekey: false,
                local_artwork: false,
                remote_artwork: false,
                urgent_run: 0,
                input_run: 0,
            }),
            capacity,
            wake: Notify::new(),
        }
    }

    pub fn set_local_audio(&self, capabilities: &[i32]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.local_audio =
            capabilities.contains(&(protocol::wire::Capability::AudioControlV1 as i32));
        state.local_focus =
            capabilities.contains(&(protocol::wire::Capability::AudioFocusReportV1 as i32));
        state.local_mic = capabilities.contains(&(protocol::wire::Capability::MicControlV1 as i32));
        state.local_media =
            capabilities.contains(&(protocol::wire::Capability::MediaControlV1 as i32));
        state.local_seek = capabilities.contains(&(protocol::wire::Capability::MediaSeekV1 as i32));
        state.local_rekey =
            capabilities.contains(&(protocol::wire::Capability::AudioRekeyV1 as i32));
        state.local_artwork =
            capabilities.contains(&(protocol::wire::Capability::MediaArtworkV1 as i32));
        state.purge_audio();
    }

    /// Re-enabling is visible only after its announcement reached the wire.
    pub fn local_capabilities_written(&self, capabilities: &[i32]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for cap in [
            protocol::wire::Capability::AudioControlV1 as i32,
            protocol::wire::Capability::AudioFocusReportV1 as i32,
            protocol::wire::Capability::MicControlV1 as i32,
            protocol::wire::Capability::MediaControlV1 as i32,
            protocol::wire::Capability::MediaSeekV1 as i32,
            protocol::wire::Capability::AudioRekeyV1 as i32,
            protocol::wire::Capability::MediaArtworkV1 as i32,
        ] {
            // A withdrawal admitted while this write was in flight wins immediately.
            if capabilities.contains(&cap)
                && !state.auxiliary.iter().any(|(_, intent)| {
                    matches!(intent, Intent::Outbound(Outbound::CapabilitiesChanged(caps))
                        if !caps.contains(&cap))
                })
            {
                if cap == protocol::wire::Capability::AudioControlV1 as i32 {
                    state.local_audio = true;
                } else if cap == protocol::wire::Capability::MicControlV1 as i32 {
                    state.local_mic = true;
                } else if cap == protocol::wire::Capability::MediaControlV1 as i32 {
                    state.local_media = true;
                } else if cap == protocol::wire::Capability::MediaArtworkV1 as i32 {
                    state.local_artwork = true;
                } else if cap == protocol::wire::Capability::AudioRekeyV1 as i32 {
                    state.local_rekey = true;
                } else if cap == protocol::wire::Capability::MediaSeekV1 as i32 {
                    state.local_seek = true;
                } else {
                    state.local_focus = true;
                }
            }
        }
    }

    pub fn set_remote_audio(&self, capabilities: &[i32]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.remote_audio =
            capabilities.contains(&(protocol::wire::Capability::AudioControlV1 as i32));
        state.remote_focus =
            capabilities.contains(&(protocol::wire::Capability::AudioFocusReportV1 as i32));
        state.remote_mic =
            capabilities.contains(&(protocol::wire::Capability::MicControlV1 as i32));
        state.remote_media =
            capabilities.contains(&(protocol::wire::Capability::MediaControlV1 as i32));
        state.remote_seek =
            capabilities.contains(&(protocol::wire::Capability::MediaSeekV1 as i32));
        state.remote_rekey =
            capabilities.contains(&(protocol::wire::Capability::AudioRekeyV1 as i32));
        state.remote_artwork =
            capabilities.contains(&(protocol::wire::Capability::MediaArtworkV1 as i32));
        state.purge_audio();
    }

    pub fn audio_allowed(&self, message: &protocol::wire::AudioControl) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.audio_allowed(message)
    }

    pub fn begin_close(&self) -> Instant {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.accepting = false;
        *state
            .close_deadline
            .get_or_insert_with(|| Instant::now() + crate::link::GRACEFUL_CLOSE_BUDGET)
    }

    pub fn hello(&self, envelope: protocol::wire::Envelope) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).hello = Some(Intent::Hello(envelope));
        self.wake.notify_one();
    }

    /// Seals normal admission and reserves one refusal without opening the Hello barrier.
    /// The writer completes `reply` with whether it successfully wrote the refusal.
    pub fn refuse(
        &self,
        connection_id: [u8; 16],
        code: protocol::wire::ErrorCode,
        related_seq: u64,
        reply: tokio::sync::oneshot::Sender<bool>,
    ) -> Result<(), crate::TransportError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.refusing {
            return Err(crate::TransportError::LinkClosed);
        }
        state.refusing = true;
        state.accepting = false;
        state.unpair_allowed = false;
        state.latest = None;
        state.ordered.clear();
        state.urgent.clear();
        state.auxiliary.clear();
        state.audio_stops.clear();
        state.refusal = Some(Intent::Refuse {
            connection_id,
            code,
            related_seq,
            reply,
        });
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    pub fn ready(&self, unpair_allowed: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.refusing {
            return;
        }
        state.ready = true;
        state.unpair_allowed = unpair_allowed;
        drop(state);
        self.wake.notify_one();
    }

    pub fn abort(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.generation = state.generation.wrapping_add(1);
        state.refusal = None;
        state.latest = None;
        state.ordered.clear();
        state.urgent.clear();
        state.auxiliary.clear();
        state.audio_stops.clear();
        self.wake.notify_one();
    }

    pub fn unpair_allowed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unpair_allowed
    }

    pub fn depth(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ordered
            .len()
    }

    pub fn abort_lease(&self, lease_id: [u8; 16]) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let abort_active = state.active_lease == Some(lease_id);
        if let Some((generation, selected_lease)) = state.selected_input {
            if abort_active || selected_lease == lease_id {
                state.invalidated_selection = Some(generation);
            }
        }
        let mut discarded = 0;
        if state
            .latest
            .as_ref()
            .is_some_and(|(_, lease, _)| abort_active || *lease == lease_id)
        {
            state.latest = None;
            discarded += 1;
        }
        state.ordered.retain(|(_, intent)| {
            if let Intent::Input {
                lease_id: lease,
                samples,
                ..
            } = intent
            {
                if abort_active || *lease == lease_id {
                    discarded += samples
                        .iter()
                        .filter(|sample| matches!(sample.event, InputEvent::PointerAbsolute(_)))
                        .count() as u64;
                    return false;
                }
            }
            true
        });
        if abort_active {
            state.accepting = false;
            state.boundary_seen = true;
            state.generation = state.generation.wrapping_add(1);
            state.minimum_generation = state.generation;
        }
        drop(state);
        self.wake.notify_one();
        discarded
    }

    pub fn valid_generation(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.closed
            && !state.refusing
            && generation >= state.minimum_generation
            && state.invalidated_selection != Some(generation)
    }

    pub fn input(
        &self,
        lease_id: [u8; 16],
        mut samples: Vec<InputSample>,
    ) -> Result<u64, crate::TransportError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.closed
            && !state.refusing
            && state.close_deadline.is_none()
            && !state.boundary_seen
            && state.active_lease.is_none()
        {
            state.active_lease = Some(lease_id);
            state.accepting = true;
        }
        if state.closed
            || state.refusing
            || !state.accepting
            || state.active_lease != Some(lease_id)
        {
            return Err(crate::TransportError::LinkClosed);
        }
        let generation = state.generation;
        if samples
            .iter()
            .all(|sample| matches!(sample.event, InputEvent::PointerAbsolute(_)))
        {
            let overwritten = samples.len().saturating_sub(1) as u64
                + u64::from(!samples.is_empty() && state.latest.is_some());
            if let Some(sample) = samples.pop() {
                state.latest = Some((generation, lease_id, sample));
            }
            drop(state);
            self.wake.notify_one();
            return Ok(overwritten);
        }
        let mut latest = state.latest.clone();
        let mut ordered = Vec::new();
        let mut run = Vec::new();
        let mut overwritten = 0;
        for sample in samples {
            if matches!(sample.event, InputEvent::PointerRelative { .. }) {
                continue; // Relative deltas have no wire representation.
            }
            if matches!(sample.event, InputEvent::PointerAbsolute(_)) {
                overwritten += u64::from(latest.is_some());
                latest = Some((generation, lease_id, sample));
            } else {
                if let Some((_, _, position)) = latest.take() {
                    run.push(position);
                }
                run.push(sample);
                while run.len() >= 128 {
                    let tail = run.split_off(128);
                    ordered.push(Intent::Input {
                        generation,
                        lease_id,
                        samples: run,
                    });
                    run = tail;
                }
            }
        }
        if !run.is_empty() {
            ordered.push(Intent::Input {
                generation,
                lease_id,
                samples: run,
            });
        }
        if state.ordered.len() + ordered.len() > self.capacity {
            return Err(crate::TransportError::Protocol(
                "critical input queue overflow".into(),
            ));
        }
        let admission = state.next_admission()?;
        state.latest = latest;
        state
            .ordered
            .extend(ordered.into_iter().map(|intent| (admission, intent)));
        drop(state);
        self.wake.notify_one();
        Ok(overwritten)
    }

    pub fn push(&self, item: Outbound) -> Result<(), crate::TransportError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.refusing {
            return Err(crate::TransportError::LinkClosed);
        }
        if let Outbound::AudioControl(message) = &item {
            use protocol::validate::AudioViolation;
            if !state.audio_allowed(message) {
                return Err(crate::TransportError::Audio(AudioViolation::Capability));
            }
            // Privacy closes share Stop's reserved lane, never the ordinary metadata budget.
            let stop = match &message.body {
                Some(protocol::wire::audio_control::Body::Stop(_)) => true,
                Some(protocol::wire::audio_control::Body::MicControl(control)) => {
                    control.op == protocol::wire::MicControlOp::Release as i32
                        || control.op == protocol::wire::MicControlOp::ReleaseAll as i32
                        || (control.op == protocol::wire::MicControlOp::SetMute as i32
                            && control.muted)
                }
                _ => false,
            };
            if stop {
                if state.audio_stops.len() == 4 {
                    return Err(crate::TransportError::Audio(AudioViolation::QueueFull));
                }
                state.audio_stops.push_back(Intent::Outbound(item));
                drop(state);
                self.wake.notify_one();
                return Ok(());
            }
            // Audio has its own eight-item budget inside the ordered metadata
            // FIFO, leaving all sixteen existing metadata admissions available.
            let audio_count = state
                .auxiliary
                .iter()
                .filter(|(_, intent)| matches!(intent, Intent::Outbound(Outbound::AudioControl(_))))
                .count();
            if audio_count == 8 {
                return Err(crate::TransportError::Audio(AudioViolation::QueueFull));
            }
        }
        let admission = state.next_admission()?;
        if let Outbound::Guaranteed(message) = &item {
            match message {
                fr::Message::EnterCommit { lease_id }
                    if state.active_lease.is_some_and(|active| active != *lease_id)
                        || (state.active_lease == Some(*lease_id) && !state.accepting) =>
                {
                    return Err(crate::TransportError::LinkClosed);
                }
                fr::Message::EnterPrepare { lease_id, .. }
                    if state.active_lease == Some(*lease_id) && !state.accepting =>
                {
                    return Err(crate::TransportError::LinkClosed);
                }
                _ => {}
            }
        }
        let fence = matches!(
            &item,
            Outbound::Guaranteed(
                fr::Message::EnterPrepare { .. }
                    | fr::Message::EnterCommit { .. }
                    | fr::Message::Leave { .. }
                    | fr::Message::LeaveAck { .. }
                    | fr::Message::Shutdown { .. }
                    | fr::Message::OwnerClaim { .. }
                    | fr::Message::OwnerState { .. }
            )
        );
        if fence {
            if !matches!(
                &item,
                Outbound::Guaranteed(
                    fr::Message::OwnerClaim { .. } | fr::Message::OwnerState { .. }
                )
            ) {
                state.boundary_seen = true;
            }
            let needed = 1 + usize::from(state.latest.is_some());
            if state.ordered.len() + needed > self.capacity {
                return Err(crate::TransportError::Protocol(
                    "critical fence queue overflow".into(),
                ));
            }
            if let Some((generation, lease_id, sample)) = state.latest.take() {
                state.ordered.push_back((
                    admission,
                    Intent::Input {
                        generation,
                        lease_id,
                        samples: vec![sample],
                    },
                ));
            }
            if let Outbound::Guaranteed(message) = &item {
                match message {
                    fr::Message::EnterPrepare { lease_id, .. }
                    | fr::Message::EnterCommit { lease_id } => {
                        if state.active_lease != Some(*lease_id) {
                            state.generation = state.generation.wrapping_add(1);
                            state.active_lease = Some(*lease_id);
                        }
                        state.accepting = true;
                    }
                    fr::Message::Leave { lease_id, .. }
                        if state.active_lease == Some(*lease_id) =>
                    {
                        state.accepting = false;
                    }
                    fr::Message::Shutdown { .. } => state.accepting = false,
                    _ => {}
                }
            }
            state.ordered.push_back((admission, Intent::Outbound(item)));
        } else if matches!(&item, Outbound::Guaranteed(_)) {
            if state.urgent.len() == 32 {
                return Err(crate::TransportError::Protocol(
                    "urgent control queue overflow".into(),
                ));
            }
            state.urgent.push_back(Intent::Outbound(item));
        } else {
            if !matches!(&item, Outbound::AudioControl(_))
                && state
                    .auxiliary
                    .iter()
                    .filter(|(_, intent)| {
                        !matches!(intent, Intent::Outbound(Outbound::AudioControl(_)))
                    })
                    .count()
                    == 16
            {
                return Err(crate::TransportError::Protocol(
                    "auxiliary queue overflow".into(),
                ));
            }
            if let Outbound::CapabilitiesChanged(capabilities) = &item {
                if !capabilities.contains(&(protocol::wire::Capability::AudioControlV1 as i32)) {
                    state.local_audio = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::AudioFocusReportV1 as i32))
                {
                    state.local_focus = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::MicControlV1 as i32)) {
                    state.local_mic = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::MediaControlV1 as i32)) {
                    state.local_media = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::AudioRekeyV1 as i32)) {
                    state.local_rekey = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::MediaArtworkV1 as i32)) {
                    state.local_artwork = false;
                }
                if !capabilities.contains(&(protocol::wire::Capability::MediaSeekV1 as i32)) {
                    state.local_seek = false;
                }
                state.purge_audio();
            }
            state
                .auxiliary
                .push_back((admission, Intent::Outbound(item)));
        }
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    pub fn urgent(&self, intent: Intent) -> Result<(), crate::TransportError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.refusing {
            return Err(crate::TransportError::LinkClosed);
        }
        if matches!(&intent, Intent::Unpair(..)) {
            let needed = 1 + usize::from(state.latest.is_some());
            if state.ordered.len() + needed > self.capacity {
                return Err(crate::TransportError::Protocol(
                    "critical close queue overflow".into(),
                ));
            }
            let admission = state.next_admission()?;
            if let Some((generation, lease_id, sample)) = state.latest.take() {
                state.ordered.push_back((
                    admission,
                    Intent::Input {
                        generation,
                        lease_id,
                        samples: vec![sample],
                    },
                ));
            }
            state.accepting = false;
            state.ordered.push_back((admission, intent));
        } else {
            if state.urgent.len() == 32 {
                return Err(crate::TransportError::Protocol(
                    "urgent control queue overflow".into(),
                ));
            }
            state.urgent.push_back(intent);
        }
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    pub fn pop(&self) -> Option<Intent> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return None;
        }
        if let Some(hello) = state.hello.take() {
            return Some(hello);
        }
        if let Some(refusal) = state.refusal.take() {
            return Some(refusal);
        }
        if state.refusing {
            return None;
        }
        if !state.ready {
            return None;
        }
        let blocked = state.urgent.front().is_some_and(|intent| {
            let Intent::Outbound(Outbound::Guaranteed(message)) = intent else {
                return false;
            };
            let Some(lease) = crate::convert::lease_id_of_message(message) else {
                return false;
            };
            state.ordered.iter().any(|(_, intent)| {
                matches!(intent,
                Intent::Outbound(Outbound::Guaranteed(fr::Message::EnterPrepare { lease_id, .. }
                    | fr::Message::EnterCommit { lease_id })) if *lease_id == lease)
            })
        });
        if state.auxiliary.is_empty() {
            state.input_run = 0;
        }
        // Metadata must not pass an older fence anywhere in the ordered prefix.
        // Admission indices are nondecreasing in the FIFO.
        let auxiliary_ready = state.auxiliary.front().is_some_and(|(auxiliary_index, _)| {
            state
                .ordered
                .iter()
                .take_while(|(index, _)| index < auxiliary_index)
                .all(|(_, intent)| matches!(intent, Intent::Input { .. }))
        });
        let input_waiting = !state.ordered.is_empty() || state.latest.is_some() || auxiliary_ready;
        if (!state.audio_stops.is_empty() || (!blocked && !state.urgent.is_empty()))
            && (state.urgent_run < 8 || !input_waiting)
        {
            state.urgent_run = if input_waiting {
                state.urgent_run.saturating_add(1)
            } else {
                0
            };
            return state
                .audio_stops
                .pop_front()
                .or_else(|| state.urgent.pop_front());
        }
        state.urgent_run = 0;
        // A fence may depend on metadata already admitted (notably Topology before
        // EnterPrepare). Later metadata must not overtake that fence either.
        let fence = state
            .ordered
            .front()
            .filter(|(_, intent)| !matches!(intent, Intent::Input { .. }));
        let older_auxiliary = fence.is_some_and(|(fence_index, _)| {
            state
                .auxiliary
                .front()
                .is_some_and(|(index, _)| index < fence_index)
        });
        if auxiliary_ready
            && (older_auxiliary || (fence.is_none() && state.input_run >= AUXILIARY_INPUT_BURST))
        {
            if let Some((_, item)) = state.auxiliary.pop_front() {
                state.input_run = 0;
                return Some(item);
            }
        }
        if let Some((_, item)) = state.ordered.pop_front() {
            if let Intent::Input {
                generation,
                lease_id,
                ..
            } = &item
            {
                state.selected_input = Some((*generation, *lease_id));
                state.input_run = state.input_run.saturating_add(1);
            }
            return Some(item);
        }
        if let Some((generation, lease_id, sample)) = state.latest.take() {
            state.selected_input = Some((generation, lease_id));
            state.input_run = state.input_run.saturating_add(1);
            return Some(Intent::Input {
                generation,
                lease_id,
                samples: vec![sample],
            });
        }
        state.input_run = 0;
        state.auxiliary.pop_front().map(|(_, intent)| intent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::wire;
    use tokio::sync::oneshot;

    const CONNECTION: [u8; 16] = [1; 16];
    const LEASE: [u8; 16] = [2; 16];

    fn audio_stop() -> Outbound {
        Outbound::AudioControl(wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::Stop(wire::AudioStop {
                session_id: vec![3; 16],
                reason: wire::AudioStopCode::UserStop as i32,
            })),
        })
    }

    #[test]
    fn audio_stop_bypasses_blocked_lease_urgent_but_yields_to_waiting_input() {
        let queue = InputQueue::new(16);
        let caps = [wire::Capability::AudioControlV1 as i32];
        queue.set_local_audio(&caps);
        queue.set_remote_audio(&caps);
        queue.ready(false);
        queue
            .push(Outbound::Guaranteed(fr::Message::EnterCommit {
                lease_id: LEASE,
            }))
            .unwrap();
        queue
            .push(Outbound::Guaranteed(fr::Message::LeaseRenew {
                lease_id: LEASE,
                nonce: [5; 16],
            }))
            .unwrap();
        queue.push(audio_stop()).unwrap();
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::AudioControl(_)))
        ));
        queue.state.lock().unwrap().urgent_run = 8;
        queue.push(audio_stop()).unwrap();
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::Guaranteed(
                fr::Message::EnterCommit { .. }
            )))
        ));
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::AudioControl(_)))
        ));
    }

    #[test]
    fn reenable_waits_for_write_and_inflight_enable_cannot_undo_withdrawal() {
        let queue = InputQueue::new(16);
        let caps = vec![wire::Capability::AudioControlV1 as i32];
        queue.set_local_audio(&caps);
        queue.set_remote_audio(&caps);
        queue.ready(false);
        queue.push(audio_stop()).unwrap();
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
        assert!(!queue.audio_allowed(&wire::AudioControl::default()));
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::CapabilitiesChanged(_)))
        ));
        queue
            .push(Outbound::CapabilitiesChanged(caps.clone()))
            .unwrap();
        assert!(queue.push(audio_stop()).is_err());
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::CapabilitiesChanged(_)))
        ));
        assert!(queue.push(audio_stop()).is_err());
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
        queue.local_capabilities_written(&caps);
        assert!(!queue.audio_allowed(&wire::AudioControl::default()));
        queue.pop().unwrap();
        queue
            .push(Outbound::CapabilitiesChanged(caps.clone()))
            .unwrap();
        queue.pop().unwrap();
        queue.local_capabilities_written(&caps);
        assert!(queue.audio_allowed(&wire::AudioControl::default()));
        queue.push(audio_stop()).unwrap();
        assert!(matches!(
            queue.pop(),
            Some(Intent::Outbound(Outbound::AudioControl(_)))
        ));
        assert!(queue.pop().is_none());
    }

    fn focus_request() -> wire::AudioControl {
        wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::FocusRequest(
                wire::AudioFocusRequest {
                    group_id: vec![4; 16],
                },
            )),
        }
    }

    #[test]
    fn audio_and_focus_withdrawals_purge_only_their_own_metadata() {
        let audio = wire::Capability::AudioControlV1 as i32;
        let focus = wire::Capability::AudioFocusReportV1 as i32;
        for local in [false, true] {
            for withdrawn in [audio, focus] {
                let queue = InputQueue::new(16);
                queue.set_local_audio(&[audio, focus]);
                queue.set_remote_audio(&[audio, focus]);
                queue.ready(false);
                queue.push(audio_stop()).unwrap();
                queue.push(Outbound::AudioControl(focus_request())).unwrap();
                let remaining = if withdrawn == audio { focus } else { audio };
                if local {
                    queue
                        .push(Outbound::CapabilitiesChanged(vec![remaining]))
                        .unwrap();
                } else {
                    queue.set_remote_audio(&[remaining]);
                }
                assert_eq!(queue.audio_allowed(&focus_request()), withdrawn == audio);
                assert_eq!(
                    queue.audio_allowed(&wire::AudioControl::default()),
                    withdrawn == focus
                );
                let mut received = 0;
                while let Some(intent) = queue.pop() {
                    if let Intent::Outbound(Outbound::AudioControl(message)) = intent {
                        assert_eq!(is_focus(&message), withdrawn == audio);
                        received += 1;
                    }
                }
                assert_eq!(received, 1);
            }
        }
    }

    #[test]
    fn focus_reenable_waits_for_write_and_inflight_enable_cannot_undo_withdrawal() {
        let queue = InputQueue::new(16);
        let caps = vec![wire::Capability::AudioFocusReportV1 as i32];
        queue.set_local_audio(&caps);
        queue.set_remote_audio(&caps);
        queue.ready(false);
        queue.push(Outbound::AudioControl(focus_request())).unwrap();
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
        assert!(!queue.audio_allowed(&focus_request()));
        queue.pop().unwrap();
        assert!(queue.pop().is_none());
        queue
            .push(Outbound::CapabilitiesChanged(caps.clone()))
            .unwrap();
        assert!(!queue.audio_allowed(&focus_request()));
        queue.pop().unwrap();
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
        queue.local_capabilities_written(&caps);
        assert!(!queue.audio_allowed(&focus_request()));
        queue.pop().unwrap();
        queue
            .push(Outbound::CapabilitiesChanged(caps.clone()))
            .unwrap();
        queue.pop().unwrap();
        queue.local_capabilities_written(&caps);
        assert!(queue.audio_allowed(&focus_request()));
        assert!(!queue.audio_allowed(&wire::AudioControl::default()));
    }

    fn motion() -> InputSample {
        InputSample {
            event: InputEvent::PointerAbsolute(input_model::Position::new(1.0, 1.0)),
            capture_host_us: None,
            capture_to_enqueue_us: None,
            enqueued_at: Instant::now(),
        }
    }

    fn ping() -> Intent {
        Intent::Payload(wire::envelope::Payload::Ping(wire::Ping { nonce: 1 }))
    }

    #[tokio::test]
    async fn refusal_bypasses_readiness_without_releasing_input_and_preserves_reply() {
        let queue = InputQueue::new(1);
        queue.input(LEASE, vec![motion()]).unwrap();
        assert!(queue.pop().is_none());
        let (reply, receive) = oneshot::channel();
        queue
            .refuse(CONNECTION, wire::ErrorCode::UnsupportedVersion, 7, reply)
            .unwrap();
        let Some(Intent::Refuse {
            connection_id,
            code,
            related_seq,
            reply,
        }) = queue.pop()
        else {
            panic!("refusal was blocked by readiness");
        };
        assert_eq!(connection_id, CONNECTION);
        assert_eq!(code, wire::ErrorCode::UnsupportedVersion);
        assert_eq!(related_seq, 7);
        assert!(!queue.state.lock().unwrap().ready);
        assert!(queue.pop().is_none());
        reply.send(true).unwrap();
        assert!(receive.await.unwrap());
    }

    #[test]
    fn pending_hello_precedes_refusal_even_when_close_has_begun() {
        let queue = InputQueue::new(1);
        let hello = wire::Envelope {
            seq: 1,
            ..Default::default()
        };
        queue.hello(hello.clone());
        queue.begin_close();
        let (reply, _receive) = oneshot::channel();
        queue
            .refuse(CONNECTION, wire::ErrorCode::InvalidFrame, 2, reply)
            .unwrap();
        assert!(matches!(queue.pop(), Some(Intent::Hello(value)) if value == hello));
        assert!(matches!(queue.pop(), Some(Intent::Refuse { .. })));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn refusal_is_bounded_and_cannot_be_replaced_after_pop() {
        let queue = InputQueue::new(1);
        let (reply, _receive) = oneshot::channel();
        queue
            .refuse(CONNECTION, wire::ErrorCode::InvalidFrame, 1, reply)
            .unwrap();
        for pop_first in [false, true] {
            if pop_first {
                assert!(matches!(
                    queue.pop(),
                    Some(Intent::Refuse { related_seq: 1, .. })
                ));
            }
            let (reply, _receive) = oneshot::channel();
            assert!(matches!(
                queue.refuse(CONNECTION, wire::ErrorCode::InvalidSequence, 2, reply),
                Err(crate::TransportError::LinkClosed)
            ));
        }
        assert!(queue.pop().is_none());
    }

    #[test]
    fn refusal_discards_all_normal_traffic_and_seals_every_admission_path() {
        let queue = InputQueue::new(2);
        queue.ready(true);
        let mut edge = motion();
        edge.event = InputEvent::Button {
            button: input_model::MouseButton::Left,
            state: input_model::KeyState::Down,
        };
        queue.input(LEASE, vec![edge]).unwrap();
        let Some(Intent::Input { generation, .. }) = queue.pop() else {
            panic!("input selection missing");
        };
        queue.input(LEASE, vec![motion()]).unwrap();
        queue
            .push(Outbound::Guaranteed(fr::Message::EnterCommit {
                lease_id: LEASE,
            }))
            .unwrap();
        queue.urgent(ping()).unwrap();
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
        let (reply, _receive) = oneshot::channel();
        queue
            .refuse(CONNECTION, wire::ErrorCode::InvalidFrame, 1, reply)
            .unwrap();
        {
            let state = queue.state.lock().unwrap();
            assert!(state.latest.is_none());
            assert!(state.ordered.is_empty());
            assert!(state.urgent.is_empty());
            assert!(state.auxiliary.is_empty());
        }
        assert!(!queue.valid_generation(generation));
        assert!(!queue.unpair_allowed());
        queue.ready(true);
        assert!(!queue.unpair_allowed());
        assert!(matches!(
            queue.input(LEASE, vec![motion()]),
            Err(crate::TransportError::LinkClosed)
        ));
        assert!(matches!(
            queue.input([3; 16], vec![motion()]),
            Err(crate::TransportError::LinkClosed)
        ));
        assert!(matches!(
            queue.push(Outbound::CapabilitiesChanged(vec![])),
            Err(crate::TransportError::LinkClosed)
        ));
        assert!(matches!(
            queue.push(Outbound::Guaranteed(fr::Message::EnterCommit {
                lease_id: LEASE
            })),
            Err(crate::TransportError::LinkClosed)
        ));
        assert!(matches!(
            queue.urgent(ping()),
            Err(crate::TransportError::LinkClosed)
        ));
        assert!(matches!(queue.pop(), Some(Intent::Refuse { .. })));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn abort_drops_pending_refusal_reply_and_rejects_refusal() {
        let queue = InputQueue::new(1);
        let (reply, mut receive) = oneshot::channel();
        queue
            .refuse(CONNECTION, wire::ErrorCode::InvalidFrame, 1, reply)
            .unwrap();
        queue.abort();
        assert!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(queue.pop().is_none());
        let (reply, _receive) = oneshot::channel();
        assert!(matches!(
            queue.refuse(CONNECTION, wire::ErrorCode::InvalidFrame, 2, reply),
            Err(crate::TransportError::LinkClosed)
        ));
    }

    fn mic_control(op: wire::MicControlOp, muted: bool) -> Outbound {
        Outbound::AudioControl(wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::MicControl(wire::MicControl {
                group_id: vec![1; 16],
                request_id: vec![2; 16],
                expected_revision: 1,
                op: op as i32,
                muted,
                ..Default::default()
            })),
        })
    }

    fn family_message(cap: wire::Capability) -> wire::AudioControl {
        wire::AudioControl {
            control_version: 1,
            body: Some(match cap {
                wire::Capability::MicControlV1 => {
                    wire::audio_control::Body::MicState(wire::MicState::default())
                }
                wire::Capability::MediaControlV1 => {
                    wire::audio_control::Body::MediaInfo(wire::MediaInfo::default())
                }
                _ => panic!("unexpected family"),
            }),
        }
    }

    #[test]
    fn mic_privacy_has_reserved_admission_but_cannot_starve_kvm() {
        let queue = InputQueue::new(16);
        let caps = [wire::Capability::MicControlV1 as i32];
        queue.set_local_audio(&caps);
        queue.set_remote_audio(&caps);
        queue.ready(false);
        for _ in 0..8 {
            queue
                .push(mic_control(wire::MicControlOp::SetMute, false))
                .unwrap();
        }
        assert!(matches!(
            queue.push(mic_control(wire::MicControlOp::SetMute, false)),
            Err(crate::TransportError::Audio(
                protocol::validate::AudioViolation::QueueFull
            ))
        ));
        queue
            .push(mic_control(wire::MicControlOp::SetMute, true))
            .unwrap();
        queue
            .push(mic_control(wire::MicControlOp::Release, false))
            .unwrap();
        queue
            .push(mic_control(wire::MicControlOp::ReleaseAll, false))
            .unwrap();
        queue
            .push(Outbound::Guaranteed(fr::Message::EnterCommit {
                lease_id: LEASE,
            }))
            .unwrap();
        for op in [
            wire::MicControlOp::SetMute,
            wire::MicControlOp::Release,
            wire::MicControlOp::ReleaseAll,
        ] {
            let Some(Intent::Outbound(Outbound::AudioControl(message))) = queue.pop() else {
                panic!("privacy must bypass metadata");
            };
            assert!(
                matches!(message.body, Some(wire::audio_control::Body::MicControl(control)) if control.op == op as i32)
            );
        }
        queue
            .push(mic_control(wire::MicControlOp::Release, false))
            .unwrap();
        queue.state.lock().unwrap().urgent_run = 8;
        // Older metadata remains ahead of the lease fence; the urgent burst
        // budget must nevertheless advance that bounded prefix and then KVM.
        let mut committed = false;
        for _ in 0..81 {
            if matches!(
                queue.pop(),
                Some(Intent::Outbound(Outbound::Guaranteed(
                    fr::Message::EnterCommit { .. }
                )))
            ) {
                committed = true;
                break;
            }
            if queue.state.lock().unwrap().audio_stops.len() < 4 {
                queue
                    .push(mic_control(wire::MicControlOp::Release, false))
                    .unwrap();
            }
        }
        assert!(
            committed,
            "continuous privacy traffic must not starve the lease fence"
        );
    }

    #[test]
    fn mic_and_media_withdrawal_purges_only_own_family_including_privacy_lane() {
        let caps = [
            wire::Capability::MicControlV1,
            wire::Capability::MediaControlV1,
        ];
        for local in [false, true] {
            for withdrawn in caps {
                let queue = InputQueue::new(16);
                let all = caps.map(|cap| cap as i32);
                queue.set_local_audio(&all);
                queue.set_remote_audio(&all);
                queue.ready(false);
                for cap in caps {
                    queue
                        .push(Outbound::AudioControl(family_message(cap)))
                        .unwrap();
                }
                queue
                    .push(mic_control(wire::MicControlOp::Release, false))
                    .unwrap();
                let remaining: Vec<_> = caps
                    .iter()
                    .filter(|cap| **cap != withdrawn)
                    .map(|cap| *cap as i32)
                    .collect();
                if local {
                    queue
                        .push(Outbound::CapabilitiesChanged(remaining))
                        .unwrap();
                } else {
                    queue.set_remote_audio(&remaining);
                }
                let mut count = 0;
                while let Some(intent) = queue.pop() {
                    if let Intent::Outbound(Outbound::AudioControl(message)) = intent {
                        assert_ne!(audio_capability(&message), withdrawn);
                        count += 1;
                    }
                }
                assert_eq!(
                    count,
                    if withdrawn == wire::Capability::MicControlV1 {
                        1
                    } else {
                        2
                    }
                );
            }
        }
    }

    #[test]
    fn mic_media_reenable_barrier_and_inflight_withdrawal_apply_per_family() {
        for cap in [
            wire::Capability::MicControlV1,
            wire::Capability::MediaControlV1,
        ] {
            let queue = InputQueue::new(16);
            let caps = vec![cap as i32];
            let audio = family_message(cap);
            queue.set_local_audio(&caps);
            queue.set_remote_audio(&caps);
            queue.ready(false);
            queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
            assert!(!queue.audio_allowed(&audio));
            queue.pop().unwrap();
            queue
                .push(Outbound::CapabilitiesChanged(caps.clone()))
                .unwrap();
            assert!(!queue.audio_allowed(&audio));
            queue.pop().unwrap();
            queue.local_capabilities_written(&caps);
            assert!(queue.audio_allowed(&audio));
            queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
            queue.local_capabilities_written(&caps);
            assert!(!queue.audio_allowed(&audio));
        }
    }
    #[test]
    fn rekey_and_thumbnail_require_bilateral_caps_and_withdrawal_fences_inflight_writes() {
        for (cap, body) in [
            (
                wire::Capability::AudioRekeyV1,
                wire::audio_control::Body::Rekey(wire::AudioRekey {
                    session_id: vec![1; 16],
                    epoch: 2,
                    stage: 4,
                    nonce: Vec::new(),
                }),
            ),
            (
                wire::Capability::MediaArtworkV1,
                wire::audio_control::Body::ArtworkRequest(wire::MediaArtworkRequest {
                    session_id: vec![1; 16],
                    token: 2,
                    artwork_id: vec![3; 32],
                }),
            ),
        ] {
            let queue = InputQueue::new(16);
            let base = [
                wire::Capability::AudioControlV1 as i32,
                wire::Capability::MediaControlV1 as i32,
            ];
            let mut caps = base.to_vec();
            caps.push(cap as i32);
            let message = wire::AudioControl {
                control_version: 1,
                body: Some(body),
            };
            queue.ready(false);
            queue.set_local_audio(&caps);
            queue.set_remote_audio(&base);
            assert!(!queue.audio_allowed(&message));
            queue.set_remote_audio(&caps);
            assert!(queue.audio_allowed(&message));
            queue.push(Outbound::AudioControl(message.clone())).unwrap();
            queue.set_remote_audio(&base);
            assert!(!queue.audio_allowed(&message));
            assert!(queue.pop().is_none());
            queue.set_remote_audio(&caps);
            queue
                .push(Outbound::CapabilitiesChanged(base.to_vec()))
                .unwrap();
            queue.local_capabilities_written(&caps);
            assert!(!queue.audio_allowed(&message));
        }
    }
    #[test]
    fn seek_requires_both_peers_capability_and_withdrawal_purges_pending_seek() {
        let queue = InputQueue::new(16);
        let media = wire::Capability::MediaControlV1 as i32;
        let seek = wire::Capability::MediaSeekV1 as i32;
        let command = wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::MediaCommand(
                wire::MediaCommand {
                    command: wire::MediaCommandKind::Seek as i32,
                    position_ms: Some(0),
                    ..Default::default()
                },
            )),
        };
        queue.ready(false);
        queue.set_local_audio(&[media, seek]);
        queue.set_remote_audio(&[media]);
        assert!(!queue.audio_allowed(&command));
        queue.set_local_audio(&[media]);
        queue.set_remote_audio(&[media, seek]);
        assert!(!queue.audio_allowed(&command));
        queue.set_local_audio(&[media, seek]);
        assert!(queue.audio_allowed(&command));
        queue.push(Outbound::AudioControl(command.clone())).unwrap();
        queue.set_remote_audio(&[media]);
        assert!(!queue.audio_allowed(&command));
        assert!(queue.pop().is_none());
        let metadata = family_message(wire::Capability::MediaControlV1);
        assert!(queue.audio_allowed(&metadata));
    }
}

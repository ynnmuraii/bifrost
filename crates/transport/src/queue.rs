//! Outbound classification, bounded queues and motion coalescing.
//!
//! ADR-004 and `docs/07-quality-security.md` ask for three things that this module makes
//! explicit:
//!
//! * **bounded queues** — «Загружать clipboard во время движения | Input queue остаётся
//!   bounded, key-up не блокируется»;
//! * **priority** — control messages and key/button edges are never dropped, while motion
//!   may be («Заполнить critical input queue | Disconnect/cleanup, без silent drop
//!   key-up»);
//! * **coalescing only contiguous motions** — «coalescing только contiguous motions»
//!   (ADR-004), which is why coalescing never reorders and never merges across an edge.

use focus_router as fr;
use input_model::InputEvent;

/// Priority class of an outbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// Handshake, lease, ownership, ping: always enqueued, backpressure allowed.
    Control,
    /// Key/button/scroll edges, and any batch that contains one: never dropped, because a
    /// lost key-up is exactly the stuck-state failure the product promises to avoid.
    Edge,
    /// Pure pointer motion: coalescible, and droppable when the queue is saturated (the
    /// next position supersedes it).
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

/// Splits an `InputBatch` into the queue's vocabulary: motion-only batches travel on the
/// droppable path, everything else on the guaranteed path.
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
/// `lease_id` is the one the droppable [`Outbound::Motion`] items carried. A motion-only
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
    /// Replacement capability advertisement: guaranteed control, never coalesced.
    CapabilitiesChanged(Vec<i32>),
    /// Remote update control: guaranteed, never coalesced or dropped.
    UpdateRequest(protocol::wire::UpdateRequest),
    /// Remote update progress: guaranteed, never coalesced or dropped.
    UpdateResult(protocol::wire::UpdateResult),
    /// A layout-document message: always guaranteed, never coalesced (KVM-072).
    Layout(crate::convert::LayoutMessage),
    /// A clipboard message: always guaranteed, never coalesced, never dropped. The grant is the
    /// only clipboard message the input link carries, and it decides whether the channel it
    /// authorises ever exists, so it may not be lost behind motion (`KVM-042`).
    Clipboard(crate::clipboard::ClipboardMessage),
    /// Coalescible pointer motion; may be dropped when the queue is full.
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
            | Outbound::CapabilitiesChanged(_)
            | Outbound::UpdateRequest(_)
            | Outbound::UpdateResult(_)
            | Outbound::Layout(_)
            | Outbound::Clipboard(_) => None,
        }
    }

    /// The lease the motion belongs to, when this is motion.
    pub fn motion_lease(&self) -> Option<[u8; 16]> {
        match self {
            Outbound::Motion { lease_id, .. } => Some(*lease_id),
            Outbound::Guaranteed(_)
            | Outbound::CapabilitiesChanged(_)
            | Outbound::UpdateRequest(_)
            | Outbound::UpdateResult(_)
            | Outbound::Layout(_)
            | Outbound::Clipboard(_) => None,
        }
    }
}

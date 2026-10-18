//! Allocation-free, per-lease FSM accounting. Sent means an FSM `Send` output,
//! not a successful transport write. Ages include matching rejected/replayed frames.
//! Only final snapshots allocate shared, fixed-capacity challenge trace storage.
use crate::wire::{LeaseId, Message, Nonce};
use geometry::NodeId;

/// Number of distinct challenges retained for an expiry diagnostic.
pub const LEASE_TRACE_CAPACITY: usize = 16;

/// Diagnostic stages; timestamps never participate in lease decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseTraceStage {
    Issue,
    ChallengeRawDecode,
    ChallengeDispatch,
    RenewWrite,
    RenewRawDecode,
    RenewDispatch,
    RenewAccept,
    RenewReject,
}

const TRACE_STAGE_NAMES: [&str; 8] = [
    "issue_age_ms",
    "challenge_raw_decode_age_ms",
    "challenge_dispatch_age_ms",
    "renew_write_age_ms",
    "renew_raw_decode_age_ms",
    "renew_dispatch_age_ms",
    "renew_accept_age_ms",
    "renew_reject_age_ms",
];

#[derive(Clone, Copy, PartialEq, Eq)]
struct TraceEntry {
    lease_id: LeaseId,
    nonce: Nonce,
    times: [Option<u64>; 8],
}

impl std::fmt::Debug for TraceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut entry = f.debug_struct("Challenge");
        entry.field("nonce", &self.nonce);
        for (name, time) in TRACE_STAGE_NAMES.iter().zip(self.times) {
            if let Some(time) = time {
                entry.field(name, &time);
            }
        }
        entry.finish()
    }
}

/// Fixed storage keyed by lease and nonce, using the caller's monotonic clock.
#[derive(Debug, Clone)]
pub struct LeaseTraceRing {
    entries: [Option<TraceEntry>; LEASE_TRACE_CAPACITY],
    next: usize,
}

impl Default for LeaseTraceRing {
    fn default() -> Self {
        Self {
            entries: [None; LEASE_TRACE_CAPACITY],
            next: 0,
        }
    }
}

impl LeaseTraceRing {
    /// Preserve the first occurrence of each stage, including replayed frames.
    pub fn record(&mut self, lease_id: LeaseId, nonce: Nonce, stage: LeaseTraceStage, now_ms: u64) {
        let index = self.entries.iter().position(|entry| {
            entry.is_some_and(|entry| entry.lease_id == lease_id && entry.nonce == nonce)
        });
        let index = index.unwrap_or_else(|| {
            let index = self.next;
            self.next = (self.next + 1) % LEASE_TRACE_CAPACITY;
            self.entries[index] = Some(TraceEntry {
                lease_id,
                nonce,
                times: [None; 8],
            });
            index
        });
        if let Some(entry) = &mut self.entries[index] {
            entry.times[stage as usize].get_or_insert(now_ms);
        }
    }

    /// Only final snapshots allocate. Ages align independently anchored local clocks.
    pub fn snapshot(&self, lease_id: LeaseId, now_ms: u64) -> LeaseTraceSnapshot {
        let mut entries = [None; LEASE_TRACE_CAPACITY];
        let mut count = 0;
        for offset in 0..LEASE_TRACE_CAPACITY {
            let index = (self.next + offset) % LEASE_TRACE_CAPACITY;
            if let Some(mut entry) = self.entries[index].filter(|entry| entry.lease_id == lease_id)
            {
                for time in entry.times.iter_mut().flatten() {
                    *time = now_ms.saturating_sub(*time);
                }
                entries[count] = Some(entry);
                count += 1;
            }
        }
        LeaseTraceSnapshot(std::sync::Arc::new(entries))
    }
}

/// Oldest-first ages relative to the final snapshot, not remote clock readings.
#[derive(Clone, PartialEq, Eq)]
pub struct LeaseTraceSnapshot(std::sync::Arc<[Option<TraceEntry>; LEASE_TRACE_CAPACITY]>);

impl std::fmt::Debug for LeaseTraceSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.iter().flatten()).finish()
    }
}

/// The local side of the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRole {
    Source,
    Receiver,
}

/// Cumulative frame and acceptance counts for one lease, including preparation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeaseCounters {
    pub challenge_sent: u64,
    pub challenge_received: u64,
    pub renew_sent: u64,
    pub renew_received: u64,
    pub status_sent: u64,
    pub status_received: u64,
    pub renew_accepted: u64,
    pub renew_rejected: u64,
    pub status_accepted: u64,
    pub status_rejected: u64,
}

/// Milliseconds since the last matching inbound frame; `None` means never seen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeaseInboundAges {
    pub challenge_ms: Option<u64>,
    pub renew_ms: Option<u64>,
    pub status_ms: Option<u64>,
}

/// One-second active lease report, or the single report at the end of Remote.
/// Remaining time is the receiver deadline or the source watchdog, saturating at zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseDiagnosticSnapshot {
    pub lease_id: LeaseId,
    pub peer: NodeId,
    pub role: LeaseRole,
    pub ttl_ms: u64,
    pub remaining_ms: u64,
    pub counters: LeaseCounters,
    pub ages: LeaseInboundAges,
    pub final_snapshot: bool,
    pub challenge_trace: Option<LeaseTraceSnapshot>,
}

#[derive(Debug, Clone)]
pub(crate) struct Accounting {
    pub snapshot: LeaseDiagnosticSnapshot,
    pub deadline_ms: u64,
    pub active: bool,
    pub finished: bool,
    inbound: LeaseInboundAges,
    trace: LeaseTraceRing,
}
impl Accounting {
    pub fn new(lease_id: LeaseId, peer: NodeId, role: LeaseRole, ttl_ms: u64) -> Self {
        Self {
            snapshot: LeaseDiagnosticSnapshot {
                lease_id,
                peer,
                role,
                ttl_ms,
                remaining_ms: 0,
                counters: LeaseCounters::default(),
                ages: LeaseInboundAges::default(),
                final_snapshot: false,
                challenge_trace: None,
            },
            deadline_ms: 0,
            active: false,
            finished: false,
            inbound: LeaseInboundAges::default(),
            trace: LeaseTraceRing::default(),
        }
    }
    pub fn receive(&mut self, peer: NodeId, message: &Message, now_ms: u64) {
        if peer != self.snapshot.peer {
            return;
        }
        let c = &mut self.snapshot.counters;
        match message {
            Message::LeaseChallenge { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.challenge_received = c.challenge_received.saturating_add(1);
                self.inbound.challenge_ms = Some(now_ms);
                if let Message::LeaseChallenge { nonce, .. } = message {
                    self.trace.record(
                        *lease_id,
                        *nonce,
                        LeaseTraceStage::ChallengeDispatch,
                        now_ms,
                    );
                }
            }
            Message::LeaseRenew { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.renew_received = c.renew_received.saturating_add(1);
                self.inbound.renew_ms = Some(now_ms);
                if let Message::LeaseRenew { nonce, .. } = message {
                    self.trace
                        .record(*lease_id, *nonce, LeaseTraceStage::RenewDispatch, now_ms);
                }
            }
            Message::LeaseStatus { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.status_received = c.status_received.saturating_add(1);
                self.inbound.status_ms = Some(now_ms);
            }
            _ => {}
        }
    }
    pub fn send(&mut self, peer: NodeId, message: &Message, now_ms: u64) {
        if peer != self.snapshot.peer {
            return;
        }
        let c = &mut self.snapshot.counters;
        match message {
            Message::LeaseChallenge {
                lease_id, nonce, ..
            } if *lease_id == self.snapshot.lease_id => {
                c.challenge_sent = c.challenge_sent.saturating_add(1);
                self.trace
                    .record(*lease_id, *nonce, LeaseTraceStage::Issue, now_ms);
            }
            Message::LeaseRenew { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.renew_sent = c.renew_sent.saturating_add(1)
            }
            Message::LeaseStatus {
                lease_id,
                nonce,
                accepted,
                ..
            } if *lease_id == self.snapshot.lease_id => {
                c.status_sent = c.status_sent.saturating_add(1);
                let count = if *accepted {
                    &mut c.renew_accepted
                } else {
                    &mut c.renew_rejected
                };
                *count = count.saturating_add(1);
                self.trace.record(
                    *lease_id,
                    *nonce,
                    if *accepted {
                        LeaseTraceStage::RenewAccept
                    } else {
                        LeaseTraceStage::RenewReject
                    },
                    now_ms,
                );
            }
            _ => {}
        }
    }
    pub fn report(&self, now_ms: u64, final_snapshot: bool) -> LeaseDiagnosticSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.remaining_ms = self.deadline_ms.saturating_sub(now_ms);
        snapshot.ages = LeaseInboundAges {
            challenge_ms: self.inbound.challenge_ms.map(|t| now_ms.saturating_sub(t)),
            renew_ms: self.inbound.renew_ms.map(|t| now_ms.saturating_sub(t)),
            status_ms: self.inbound.status_ms.map(|t| now_ms.saturating_sub(t)),
        };
        snapshot.final_snapshot = final_snapshot;
        if final_snapshot {
            snapshot.challenge_trace = Some(self.trace.snapshot(snapshot.lease_id, now_ms));
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_and_expiry_snapshot() {
        let peer = NodeId::new([2; 32]);
        let lease_id = [3; 16];
        let mut a = Accounting::new(lease_id, peer, LeaseRole::Receiver, 600);
        let challenge = Message::LeaseChallenge {
            lease_id,
            nonce: [4; 16],
            ttl_ms: 600,
        };
        let renew = Message::LeaseRenew {
            lease_id,
            nonce: [4; 16],
        };
        a.send(peer, &challenge, 0);
        a.receive(peer, &renew, 120);
        a.send(
            peer,
            &Message::LeaseStatus {
                lease_id,
                nonce: [4; 16],
                accepted: true,
                last_input_batch_seq: 0,
            },
            120,
        );
        a.receive(peer, &renew, 140);
        a.send(
            peer,
            &Message::LeaseStatus {
                lease_id,
                nonce: [4; 16],
                accepted: false,
                last_input_batch_seq: 0,
            },
            140,
        );
        a.receive(NodeId::new([9; 32]), &renew, 150);
        a.deadline_ms = 700;
        let s = a.report(700, true);
        assert_eq!(s.remaining_ms, 0);
        assert!(s.final_snapshot);
        assert_eq!(s.ages.renew_ms, Some(560));
        assert_eq!(s.ages.challenge_ms, None);
        assert_eq!(
            s.counters,
            LeaseCounters {
                challenge_sent: 1,
                renew_received: 2,
                status_sent: 2,
                renew_accepted: 1,
                renew_rejected: 1,
                ..LeaseCounters::default()
            }
        );
    }

    #[test]
    fn trace_is_bounded_ordered_scoped_and_preserves_first_stage_time() {
        let mut ring = LeaseTraceRing::default();
        let lease = [1; 16];
        for i in 0..20u8 {
            ring.record(lease, [i; 16], LeaseTraceStage::Issue, u64::from(i));
        }
        ring.record(lease, [19; 16], LeaseTraceStage::Issue, 99);
        ring.record(lease, [19; 16], LeaseTraceStage::RenewAccept, 25);
        let snapshot = ring.snapshot(lease, 30);
        assert_eq!(snapshot.0.iter().flatten().count(), LEASE_TRACE_CAPACITY);
        assert_eq!(snapshot.0[0].unwrap().nonce, [4; 16]);
        assert_eq!(
            snapshot.0[0].unwrap().times[LeaseTraceStage::Issue as usize],
            Some(26)
        );
        let last = snapshot.0.iter().flatten().last().unwrap();
        assert_eq!(last.times[LeaseTraceStage::Issue as usize], Some(11));
        assert_eq!(last.times[LeaseTraceStage::RenewAccept as usize], Some(5));
        assert_eq!(ring.snapshot([2; 16], 30).0.iter().flatten().count(), 0);
    }

    #[test]
    fn challenge_trace_is_only_copied_into_the_final_snapshot() {
        let mut accounting =
            Accounting::new([1; 16], NodeId::new([2; 32]), LeaseRole::Receiver, 1000);
        accounting.send(
            NodeId::new([2; 32]),
            &Message::LeaseChallenge {
                lease_id: [1; 16],
                nonce: [3; 16],
                ttl_ms: 1000,
            },
            50,
        );
        assert!(accounting.report(60, false).challenge_trace.is_none());
        let snapshot = accounting.report(70, true);
        let trace = snapshot.challenge_trace.unwrap();
        assert_eq!(trace.0.iter().flatten().count(), 1);
        assert_eq!(
            trace.0[0].unwrap().times[LeaseTraceStage::Issue as usize],
            Some(20)
        );
    }
}

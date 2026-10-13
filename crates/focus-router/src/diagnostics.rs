//! Allocation-free, per-lease FSM accounting. Sent means an FSM `Send` output,
//! not a successful transport write. Ages include matching rejected/replayed frames.
use crate::wire::{LeaseId, Message};
use geometry::NodeId;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseDiagnosticSnapshot {
    pub lease_id: LeaseId,
    pub peer: NodeId,
    pub role: LeaseRole,
    pub ttl_ms: u64,
    pub remaining_ms: u64,
    pub counters: LeaseCounters,
    pub ages: LeaseInboundAges,
    pub final_snapshot: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Accounting {
    pub snapshot: LeaseDiagnosticSnapshot,
    pub deadline_ms: u64,
    pub active: bool,
    pub finished: bool,
    inbound: LeaseInboundAges,
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
            },
            deadline_ms: 0,
            active: false,
            finished: false,
            inbound: LeaseInboundAges::default(),
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
            }
            Message::LeaseRenew { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.renew_received = c.renew_received.saturating_add(1);
                self.inbound.renew_ms = Some(now_ms);
            }
            Message::LeaseStatus { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.status_received = c.status_received.saturating_add(1);
                self.inbound.status_ms = Some(now_ms);
            }
            _ => {}
        }
    }
    pub fn send(&mut self, peer: NodeId, message: &Message) {
        if peer != self.snapshot.peer {
            return;
        }
        let c = &mut self.snapshot.counters;
        match message {
            Message::LeaseChallenge { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.challenge_sent = c.challenge_sent.saturating_add(1)
            }
            Message::LeaseRenew { lease_id, .. } if *lease_id == self.snapshot.lease_id => {
                c.renew_sent = c.renew_sent.saturating_add(1)
            }
            Message::LeaseStatus {
                lease_id, accepted, ..
            } if *lease_id == self.snapshot.lease_id => {
                c.status_sent = c.status_sent.saturating_add(1);
                let count = if *accepted {
                    &mut c.renew_accepted
                } else {
                    &mut c.renew_rejected
                };
                *count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    pub fn report(&self, now_ms: u64, final_snapshot: bool) -> LeaseDiagnosticSnapshot {
        let mut snapshot = self.snapshot;
        snapshot.remaining_ms = self.deadline_ms.saturating_sub(now_ms);
        snapshot.ages = LeaseInboundAges {
            challenge_ms: self.inbound.challenge_ms.map(|t| now_ms.saturating_sub(t)),
            renew_ms: self.inbound.renew_ms.map(|t| now_ms.saturating_sub(t)),
            status_ms: self.inbound.status_ms.map(|t| now_ms.saturating_sub(t)),
        };
        snapshot.final_snapshot = final_snapshot;
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
        a.send(peer, &challenge);
        a.receive(peer, &renew, 120);
        a.send(
            peer,
            &Message::LeaseStatus {
                lease_id,
                nonce: [4; 16],
                accepted: true,
                last_input_batch_seq: 0,
            },
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
}

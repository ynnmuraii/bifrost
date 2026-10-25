//! Sender-side destination decisions. Text stays at its origin until a destination settles.
use std::collections::BTreeSet;

use geometry::NodeId;

/// Content identity, independent of history row IDs and transport offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub origin: NodeId,
    pub version: u64,
    pub hash: [u8; 32],
}

/// One delivery to perform, before forwarding the settling input when supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub item: Item,
    pub target: NodeId,
}

#[derive(Default)]
pub(crate) struct Routing {
    carried: Option<Item>,
    delivered: BTreeSet<NodeId>,
    destination: Option<(NodeId, u64)>,
    failed_entry: Option<NodeId>,
}

impl Routing {
    pub fn carried(&self) -> Option<Item> {
        self.carried
    }

    pub fn copy(&mut self, item: Item) {
        self.carried = Some(item);
        self.delivered.clear();
        self.delivered.insert(item.origin);
        self.failed_entry = None;
    }

    pub fn clear(&mut self) {
        self.carried = None;
        self.delivered.clear();
        self.failed_entry = None;
    }

    pub fn enter(&mut self, node: NodeId, now_ms: u64) {
        if self
            .destination
            .is_none_or(|(previous, _)| previous != node)
        {
            self.destination = Some((node, now_ms));
            self.failed_entry = None;
        }
    }

    /// Pointer motion and wheel do not call this with `activation = true`.
    pub fn settle(&mut self, now_ms: u64, settle_ms: u32, activation: bool) -> Option<Delivery> {
        let (target, entered) = self.destination?;
        if self.failed_entry == Some(target) {
            return None;
        }
        if !activation && now_ms.saturating_sub(entered) < u64::from(settle_ms.clamp(200, 3000)) {
            return None;
        }
        let item = self.carried?;
        if !self.delivered.insert(target) {
            return None;
        }
        Some(Delivery { item, target })
    }

    /// An explicitly addressed current item stops following everywhere; an old row does not.
    pub fn addressed(&mut self, hash: [u8; 32]) -> bool {
        if self.carried.is_some_and(|item| item.hash == hash) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// A failure retries only after re-entry or a new copy, never in a background loop.
    pub fn failed(&mut self, delivery: Delivery) {
        if self.carried == Some(delivery.item) {
            self.delivered.remove(&delivery.target);
            self.failed_entry = Some(delivery.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        NodeId::new([n; 32])
    }
    fn item(origin: u8, hash: u8) -> Item {
        Item {
            origin: node(origin),
            version: 1,
            hash: [hash; 32],
        }
    }

    #[test]
    fn transit_motion_and_wheel_do_not_deliver_but_ctrl_settles_before_v() {
        let mut routing = Routing::default();
        routing.copy(item(1, 9));
        routing.enter(node(2), 0);
        assert_eq!(routing.settle(100, 700, false), None);
        routing.enter(node(3), 110);
        let ctrl = routing.settle(120, 700, true).unwrap();
        assert_eq!(ctrl.target, node(3));
        assert_eq!(routing.settle(121, 700, true), None); // V cannot resend.
        routing.enter(node(2), 130);
        assert_eq!(routing.settle(829, 700, false), None);
        assert_eq!(routing.settle(830, 700, false).unwrap().target, node(2));
    }

    #[test]
    fn return_origin_and_reentry_are_noops_remote_copy_replaces_origin() {
        let mut routing = Routing::default();
        routing.copy(item(1, 9));
        routing.enter(node(1), 0);
        assert_eq!(routing.settle(0, 700, true), None);
        routing.enter(node(2), 1);
        assert!(routing.settle(1, 700, true).is_some());
        routing.enter(node(3), 2);
        routing.enter(node(2), 3);
        assert_eq!(routing.settle(3, 700, true), None);
        routing.copy(item(2, 8));
        routing.enter(node(1), 4);
        assert_eq!(routing.settle(4, 700, true).unwrap().item, item(2, 8));
    }

    #[test]
    fn addressing_current_stops_all_following_but_old_item_does_not() {
        let mut routing = Routing::default();
        routing.copy(item(1, 9));
        assert!(!routing.addressed([8; 32]));
        routing.enter(node(3), 0);
        assert_eq!(routing.settle(0, 700, true).unwrap().item.hash, [9; 32]);
        assert!(routing.addressed([9; 32]));
        routing.enter(node(2), 1);
        assert_eq!(routing.settle(900, 700, true), None);
        routing.enter(node(3), 901);
        assert_eq!(routing.settle(1900, 700, true), None);
        routing.copy(item(1, 7));
        assert!(routing.settle(1901, 700, true).is_some());
    }

    #[test]
    fn local_transit_stays_protected_until_observed_press_dwell_or_explicit_return() {
        let mut routing = Routing::default();
        routing.copy(item(2, 9));
        routing.enter(node(1), 100);
        assert_eq!(routing.settle(150, 700, false), None);
        routing.enter(node(3), 200);
        assert_eq!(routing.settle(250, 700, false), None);
        routing.enter(node(1), 300);
        assert_eq!(routing.settle(999, 700, false), None);
        assert_eq!(routing.settle(1000, 700, false).unwrap().target, node(1));

        routing.copy(item(2, 8));
        routing.enter(node(3), 1000);
        routing.enter(node(1), 1001);
        assert_eq!(routing.settle(1001, 700, false), None);
        // Windows/X11 Observe reports a press; explicit local return is also intentional.
        assert_eq!(routing.settle(1001, 700, true).unwrap().target, node(1));
        routing.copy(item(2, 7));
        routing.enter(node(3), 1001);
        routing.enter(node(1), 1002);
        assert_eq!(routing.settle(1002, 700, false), None);
        assert_eq!(routing.settle(1002, 700, true).unwrap().item.hash, [7; 32]);
        // This pure decision does not hold native local input: application remains async.
    }

    #[test]
    fn failed_delivery_retries_only_the_same_item() {
        let mut routing = Routing::default();
        routing.copy(item(1, 9));
        routing.enter(node(2), 0);
        let delivery = routing.settle(0, 700, true).unwrap();
        routing.failed(delivery);
        assert_eq!(routing.settle(1, 700, true), None);
        routing.enter(node(3), 2);
        routing.enter(node(2), 3);
        assert_eq!(routing.settle(3, 700, true), Some(delivery));
        routing.copy(item(1, 8));
        routing.failed(delivery);
        assert_eq!(routing.settle(2, 700, true).unwrap().item.hash, [8; 32]);
    }
}

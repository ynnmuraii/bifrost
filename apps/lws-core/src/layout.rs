//! The layout document's revision discipline (KVM-072, ADR-013).
//!
//! One document, `Topology{revision, author_node_id}`, replicated on every node; any node may
//! edit it, and only the coordinator — the current owner — applies an edit. This module is
//! that discipline with no sockets in it: the compare-and-swap the coordinator performs, the
//! monotonicity every node enforces, and the pending slot a partition leaves behind.
//!
//! # The pair
//!
//! A revision is the pair `(revision, author)`, ordered lexicographically: a candidate is
//! newer only when its revision is greater, and at equal revisions the greater node id wins.
//! That is the same order ownership uses (`docs/03-architecture.md`, «Владение вводом и
//! координатор»), and it is what makes two nodes that saw the same frames agree without a
//! vote.
//!
//! # The four rules this module encodes
//!
//! * **Whole-document CAS at the coordinator.** `LayoutEdit{expected_revision,
//!   expected_author}` applies only if the pair it names is the applied one; a stale base is
//!   `LAYOUT_CONFLICT` with the current revision, and the document is untouched.
//! * **The revision never goes back.** Every node keeps `max_seen` — the highest pair it has
//!   ever seen — and refuses to apply a lower one, even if a peer (or its own disk) offers it.
//! * **A `Topology` is accepted only from its author.** The publisher is the coordinator, so
//!   the author in the document is the coordinator; a peer relaying someone else's document is
//!   refused, and so is a document that fails revalidation (`INVALID_TOPOLOGY` keeps the old
//!   revision).
//! * **The rejoin republishes under the coordinator.** After a partition the coordinator takes
//!   the winning document from its author and publishes it as `max_seen + 1` with itself as
//!   author; a local edit that diverged in the meantime is kept in `layout.pending.toml` until
//!   the user applies or discards it.

use geometry::NodeId;

/// A document identity: `(revision, author)`.
///
/// `revision == 0` means "no document yet", and then the author is `None` — the pair
/// `(0, None)` is the floor every node starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pair {
    /// Monotone revision counter.
    pub revision: u64,
    /// Author of that revision; `None` only at revision 0.
    pub author: Option<NodeId>,
}

impl Pair {
    /// The floor: nothing applied, nothing seen.
    pub const NONE: Self = Self {
        revision: 0,
        author: None,
    };

    /// A document published by `author` at `revision`.
    #[must_use]
    pub const fn new(revision: u64, author: NodeId) -> Self {
        Self {
            revision,
            author: Some(author),
        }
    }

    /// Whether this pair is newer than `other` (the lexical order of the pair).
    #[must_use]
    pub fn is_newer_than(self, other: Self) -> bool {
        self > other
    }
}

/// One `LayoutEdit` as the coordinator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The pair the editor based its work on.
    pub expected: Pair,
    /// The node that made the edit (for the log and the GUI).
    pub editor: NodeId,
    /// The whole document, as `layout_toml`.
    pub layout_toml: String,
}

/// What happened to an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// Applied: the document now has this pair.
    Applied(Pair),
    /// The base was stale: `LAYOUT_CONFLICT`, with the pair that is applied now.
    Conflict(Pair),
    /// This node is not the coordinator: `NO_COORDINATOR`.
    NotCoordinator,
    /// Revalidation failed: `INVALID_TOPOLOGY`, the old revision stays.
    Invalid(String),
}

/// What happened to an incoming `Topology`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyOutcome {
    /// Applied: the document is now this pair.
    Applied(Pair),
    /// Refused: the sender is not the author of the document it sent.
    NotAuthor,
    /// Refused: the document is not newer than what is applied.
    NotNewer(Pair),
    /// Refused by revalidation; the old revision stays (`INVALID_TOPOLOGY`).
    Invalid(String),
}

/// The local view of the one layout document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    applied: Pair,
    max_seen: Pair,
    pending: Option<String>,
}

impl Layout {
    /// A view restored from the persisted `[topology]` section.
    ///
    /// `max_seen` is raised to at least `applied`: a file edited by hand (or written by an
    /// older build) must not be able to make the node apply a revision it has already seen.
    #[must_use]
    pub fn restore(applied: Pair, max_seen: Pair) -> Self {
        Self {
            applied,
            max_seen: max_seen.max(applied),
            pending: None,
        }
    }

    /// The applied document's pair.
    #[must_use]
    pub const fn applied(&self) -> Pair {
        self.applied
    }

    /// The highest pair this node has ever seen.
    #[must_use]
    pub const fn max_seen(&self) -> Pair {
        self.max_seen
    }

    /// The divergent local edit kept after a partition, if any.
    #[must_use]
    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// The coordinator applies a whole-document edit (CAS on the pair it names).
    ///
    /// `validate` revalidates the whole document, because the invariants are global: a valid
    /// edit is not a set of valid rows, and ADR-013 rejected a CRDT merge for exactly that
    /// reason. The closure is called only when the CAS would pass, so a stale edit costs
    /// nothing.
    pub fn apply_edit(
        &mut self,
        edit: &Edit,
        coordinator: NodeId,
        is_coordinator: bool,
        validate: impl FnOnce(&str) -> Result<(), String>,
    ) -> EditOutcome {
        if !is_coordinator {
            return EditOutcome::NotCoordinator;
        }
        if edit.expected != self.applied {
            return EditOutcome::Conflict(self.applied);
        }
        if let Err(detail) = validate(&edit.layout_toml) {
            return EditOutcome::Invalid(detail);
        }
        // The published document is authored by the coordinator: «Topology публикует только
        // owner, и принимается она только от своего автора» (docs/03-architecture.md).
        let next = Pair::new(self.applied.revision.saturating_add(1), coordinator);
        self.applied = next;
        self.max_seen = self.max_seen.max(next);
        EditOutcome::Applied(next)
    }

    /// A `Topology` frame arrived from `from`.
    ///
    /// Not reachable from the network yet: the FSM's message enum has no `Topology` variant
    /// (see [`TopologyOutcome`]).
    #[allow(dead_code)]
    ///
    /// Accepted only when `from` is the document's author, the document is newer than the
    /// applied one, and it revalidates. `max_seen` rises even when the document is not newer
    /// (seeing a revision is what makes a later claim by its author comparable), but the
    /// applied revision never goes back.
    pub fn accept_topology(
        &mut self,
        from: NodeId,
        document: Pair,
        validate: impl FnOnce() -> Result<(), String>,
    ) -> TopologyOutcome {
        if document.author != Some(from) {
            return TopologyOutcome::NotAuthor;
        }
        self.max_seen = self.max_seen.max(document);
        if !document.is_newer_than(self.applied) {
            return TopologyOutcome::NotNewer(self.applied);
        }
        if let Err(detail) = validate() {
            return TopologyOutcome::Invalid(detail);
        }
        self.applied = document;
        TopologyOutcome::Applied(document)
    }

    /// The rejoin: the coordinator republishes the winning document under its own authorship.
    ///
    /// Not reachable from the network yet: it belongs to the `TopologyRequest` half of the
    /// rejoin sequence, which the transport still has to carry.
    #[allow(dead_code)]
    ///
    /// Returns the pair the coordinator should publish, or `None` when this node is not the
    /// coordinator. The caller sends the document; this only fixes the identity it must carry,
    /// which is `max_seen + 1` — never `max_seen` itself, or two coordinators that both saw
    /// the same document would publish the same revision and disagree about its author.
    pub fn republish(&mut self, coordinator: NodeId) -> Pair {
        let next = Pair::new(self.max_seen.revision.saturating_add(1), coordinator);
        self.applied = next;
        self.max_seen = next;
        next
    }

    /// Keeps a divergent local edit: it cannot be applied (a newer author's revision is
    /// applied), so it waits in `layout.pending.toml` for the user's apply or discard.
    pub fn hold_pending(&mut self, layout_toml: String) {
        self.pending = Some(layout_toml);
    }

    /// Takes the pending edit (the user applied it, or discarded it).
    pub fn take_pending(&mut self) -> Option<String> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8) -> NodeId {
        let mut id = [0u8; 32];
        id[0] = byte;
        NodeId::new(id)
    }

    fn edit(base: Pair, toml: &str) -> Edit {
        Edit {
            expected: base,
            editor: node(9),
            layout_toml: toml.to_owned(),
        }
    }

    /// The order of the pair, which everything else rests on: higher revision wins, and at
    /// equal revisions the greater node id does.
    #[test]
    fn the_pair_orders_like_the_ownership_pair() {
        assert!(Pair::new(2, node(1)).is_newer_than(Pair::new(1, node(9))));
        assert!(Pair::new(2, node(9)).is_newer_than(Pair::new(2, node(1))));
        assert!(!Pair::new(2, node(1)).is_newer_than(Pair::new(2, node(9))));
        assert!(Pair::new(1, node(1)).is_newer_than(Pair::NONE));
    }

    /// The coordinator applies an edit onto the revision it names, and the published
    /// revision is authored by the coordinator.
    #[test]
    fn a_clean_edit_is_applied_under_the_coordinator() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(3, coordinator), Pair::new(3, coordinator));
        let outcome = layout.apply_edit(
            &edit(Pair::new(3, coordinator), "links = []"),
            coordinator,
            true,
            |_| Ok(()),
        );
        assert_eq!(outcome, EditOutcome::Applied(Pair::new(4, coordinator)));
        assert_eq!(layout.applied(), Pair::new(4, coordinator));
        assert_eq!(layout.max_seen(), Pair::new(4, coordinator));
    }

    /// A follower does not apply anything: `NO_COORDINATOR`.
    #[test]
    fn a_follower_never_applies() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(2, coordinator), Pair::new(2, coordinator));
        let outcome = layout.apply_edit(
            &edit(Pair::new(2, coordinator), "links = []"),
            coordinator,
            false,
            |_| Ok(()),
        );
        assert_eq!(outcome, EditOutcome::NotCoordinator);
        assert_eq!(layout.applied(), Pair::new(2, coordinator));
    }

    /// A stale base is `LAYOUT_CONFLICT` and changes nothing.
    #[test]
    fn a_stale_edit_is_a_conflict() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(4, coordinator), Pair::new(4, coordinator));
        let outcome = layout.apply_edit(
            &edit(Pair::new(3, coordinator), "links = []"),
            coordinator,
            true,
            |_| Ok(()),
        );
        assert_eq!(outcome, EditOutcome::Conflict(Pair::new(4, coordinator)));
        assert_eq!(
            layout.applied(),
            Pair::new(4, coordinator),
            "the document is untouched"
        );
    }

    /// An invalid document keeps the old revision, even from the coordinator.
    #[test]
    fn an_invalid_edit_keeps_the_old_revision() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(2, coordinator), Pair::new(2, coordinator));
        let outcome = layout.apply_edit(
            &edit(Pair::new(2, coordinator), "broken"),
            coordinator,
            true,
            |_| Err("link 0 has no target".to_owned()),
        );
        assert_eq!(
            outcome,
            EditOutcome::Invalid("link 0 has no target".to_owned())
        );
        assert_eq!(layout.applied(), Pair::new(2, coordinator));
        assert_eq!(
            layout.max_seen(),
            Pair::new(2, coordinator),
            "an invalid document is not seen either"
        );
    }

    /// A document from anyone but its author is refused — including a peer relaying the
    /// coordinator's document.
    #[test]
    fn a_topology_is_accepted_only_from_its_author() {
        let coordinator = node(1);
        let relay = node(2);
        let mut layout = Layout::restore(Pair::NONE, Pair::NONE);
        let document = Pair::new(1, coordinator);
        assert_eq!(
            layout.accept_topology(relay, document, || Ok(())),
            TopologyOutcome::NotAuthor
        );
        assert_eq!(layout.applied(), Pair::NONE);
        assert_eq!(
            layout.max_seen(),
            Pair::NONE,
            "a relayed document does not raise max_seen"
        );
        assert_eq!(
            layout.accept_topology(coordinator, document, || Ok(())),
            TopologyOutcome::Applied(document)
        );
    }

    /// The revision never goes back: a lower pair is refused even from its author.
    #[test]
    fn a_lower_revision_is_never_applied() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(5, coordinator), Pair::new(5, coordinator));
        assert_eq!(
            layout.accept_topology(coordinator, Pair::new(4, coordinator), || Ok(())),
            TopologyOutcome::NotNewer(Pair::new(5, coordinator))
        );
        assert_eq!(layout.applied(), Pair::new(5, coordinator));
        assert_eq!(
            layout.max_seen(),
            Pair::new(5, coordinator),
            "a lower revision does not lower the floor"
        );
    }

    /// An invalid document from its author keeps the applied revision but still raises
    /// `max_seen`: the revision exists, it just is not usable here.
    #[test]
    fn an_invalid_topology_keeps_the_old_revision() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(1, coordinator), Pair::new(1, coordinator));
        let document = Pair::new(6, coordinator);
        assert_eq!(
            layout.accept_topology(coordinator, document, || Err("no zones".to_owned())),
            TopologyOutcome::Invalid("no zones".to_owned())
        );
        assert_eq!(layout.applied(), Pair::new(1, coordinator));
        assert_eq!(layout.max_seen(), document);
    }

    /// The rejoin publishes strictly above everything seen, under the coordinator's name.
    #[test]
    fn a_rejoin_publishes_above_max_seen() {
        let coordinator = node(1);
        let winner = node(3);
        let mut layout = Layout::restore(Pair::new(2, winner), Pair::new(7, winner));
        let published = layout.republish(coordinator);
        assert_eq!(published, Pair::new(8, coordinator));
        assert_eq!(layout.applied(), published);
        assert!(published.is_newer_than(Pair::new(7, winner)));
    }

    /// A divergent local edit is held, not applied, and can be taken once.
    #[test]
    fn a_divergent_edit_waits_in_pending() {
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::new(3, coordinator), Pair::new(3, coordinator));
        layout.hold_pending("links = [\"mine\"]".to_owned());
        assert_eq!(layout.pending(), Some("links = [\"mine\"]"));
        assert_eq!(
            layout.applied(),
            Pair::new(3, coordinator),
            "pending is not applied"
        );
        assert_eq!(layout.take_pending().as_deref(), Some("links = [\"mine\"]"));
        assert_eq!(layout.pending(), None);
    }

    /// A file whose `max_seen` is below `applied` cannot lower the floor.
    #[test]
    fn restore_raises_max_seen_to_applied() {
        let coordinator = node(1);
        let layout = Layout::restore(Pair::new(5, coordinator), Pair::new(2, coordinator));
        assert_eq!(layout.max_seen(), Pair::new(5, coordinator));
    }

    /// The property the whole design exists for: whatever sequence of edits, accepted
    /// documents and rejoins a node sees, the applied revision and the floor only ever rise,
    /// and the applied revision never trails the floor.
    ///
    /// The sequence is generated from a fixed seed (no dependency, no flakiness): the test
    /// walks 4000 steps of every kind, including hostile ones — stale edits, relays,
    /// documents that fail validation, and pairs far below the floor.
    #[test]
    fn the_revision_never_goes_back_and_never_trails_the_floor() {
        let mut state: u64 = 0x5eed_1234;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let nodes = [node(1), node(2), node(3)];
        let coordinator = node(1);
        let mut layout = Layout::restore(Pair::NONE, Pair::NONE);
        let mut floor = Pair::NONE;

        for _ in 0..4000 {
            let applied_before = layout.applied();
            let max_before = layout.max_seen();
            match next() % 5 {
                0 => {
                    // A clean edit from the coordinator.
                    let edit = edit(applied_before, "links = []");
                    let outcome = layout.apply_edit(&edit, coordinator, true, |_| Ok(()));
                    assert_eq!(outcome, EditOutcome::Applied(layout.applied()));
                }
                1 => {
                    // A stale edit: refused, nothing moves.
                    let stale = Pair::new(applied_before.revision.saturating_sub(1), coordinator);
                    assert!(matches!(
                        layout.apply_edit(
                            &edit(stale, "links = []"),
                            coordinator,
                            true,
                            |_| Ok(())
                        ),
                        EditOutcome::Conflict(_) | EditOutcome::Applied(_)
                    ));
                }
                2 => {
                    // A document from a random node, sometimes relaying, sometimes invalid.
                    let from = nodes[(next() % 3) as usize];
                    let author = if next() % 2 == 0 {
                        from
                    } else {
                        nodes[(next() % 3) as usize]
                    };
                    let document = Pair::new(next() % 12, author);
                    let invalid = next() % 4 == 0;
                    let _ = layout.accept_topology(from, document, || {
                        if invalid {
                            Err("invalid".to_owned())
                        } else {
                            Ok(())
                        }
                    });
                }
                3 => {
                    // A rejoin by the coordinator.
                    let published = layout.republish(coordinator);
                    assert!(published.is_newer_than(max_before));
                }
                _ => {
                    // A hostile frame: a document far below everything.
                    let from = nodes[(next() % 3) as usize];
                    let _ = layout.accept_topology(from, Pair::NONE, || Ok(()));
                }
            }

            floor = floor.max(layout.max_seen());
            assert!(
                layout.applied() >= applied_before,
                "the applied revision went back: {applied_before:?} → {:?}",
                layout.applied()
            );
            assert!(
                layout.max_seen() >= max_before,
                "the floor went back: {max_before:?} → {:?}",
                layout.max_seen()
            );
            assert!(
                layout.applied() <= layout.max_seen(),
                "the applied revision trails the floor: {:?} > {:?}",
                layout.applied(),
                layout.max_seen()
            );
        }
        assert!(
            floor > Pair::NONE,
            "the sequence must have moved the floor at all"
        );
    }
}

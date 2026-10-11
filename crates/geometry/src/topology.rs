//! Topology document: nodes, displays, edge segments and transition links.
//!
//! Mirrors `Topology`, `NodeDisplays`, `Endpoint`, `TransitionLink` and
//! `Position` from `docs/protocol.proto`, and the layout document described in
//! `docs/03-architecture.md` (section «Топология и движение») and
//! `docs/config.example.toml` (`[topology]`).

use core::fmt;

use crate::display::{Display, DisplayId, Edge, NodeId, Rect, GEOMETRY_EPSILON};

/// A stretch of one display edge, as a distance interval from the edge's
/// start corner (`Endpoint.segment_start` / `segment_end`).
///
/// Invariant expected by consumers: `0 ≤ start < end ≤ edge_length`
/// (`protocol.proto`, `Endpoint`). [`Segment::is_valid`] checks it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Segment {
    /// Distance along the edge where the segment starts.
    pub start: f64,
    /// Distance along the edge where the segment ends (exclusive of nothing —
    /// the interval is treated as closed in comparisons, `[start, end]`).
    pub end: f64,
}

impl Segment {
    /// Builds a segment.
    pub const fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// Length of the segment (may be negative for an invalid value).
    pub fn len(&self) -> f64 {
        self.end - self.start
    }

    /// `true` when the segment has no positive extent.
    pub fn is_empty(&self) -> bool {
        self.len() <= GEOMETRY_EPSILON
    }

    /// `true` when `distance` lies inside the segment, tolerating both ends.
    pub fn contains(&self, distance: f64) -> bool {
        distance >= self.start - GEOMETRY_EPSILON && distance <= self.end + GEOMETRY_EPSILON
    }

    /// `0 ≤ start < end ≤ edge_length`.
    pub fn is_valid(&self, edge_length: f64) -> bool {
        self.start >= -GEOMETRY_EPSILON
            && self.start + GEOMETRY_EPSILON < self.end
            && self.end <= edge_length + GEOMETRY_EPSILON
    }

    /// Overlap of two segments, or `None` when they only touch.
    pub fn overlap(&self, other: &Segment) -> Option<Segment> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if end - start > GEOMETRY_EPSILON {
            Some(Segment::new(start, end))
        } else {
            None
        }
    }

    /// Intersection with an interval, or `None` when it is empty.
    pub fn intersect_interval(&self, low: f64, high: f64) -> Option<Segment> {
        self.overlap(&Segment::new(low, high))
    }
}

/// One end of a [`TransitionLink`] (`Endpoint` in `protocol.proto`).
#[derive(Clone, PartialEq, Debug)]
pub struct Endpoint {
    /// Node that owns the display.
    pub node: NodeId,
    /// Display inside that node.
    pub display: DisplayId,
    /// Edge of the display.
    pub edge: Edge,
    /// Stretch of that edge, in the display's logical units.
    pub segment: Segment,
}

impl Endpoint {
    /// Builds an endpoint.
    pub fn new(node: NodeId, display: impl Into<DisplayId>, edge: Edge, segment: Segment) -> Self {
        Self {
            node,
            display: display.into(),
            edge,
            segment,
        }
    }

    /// `true` when both endpoints address the same display edge.
    pub fn same_edge(&self, other: &Endpoint) -> bool {
        self.node == other.node && self.display == other.display && self.edge == other.edge
    }
}

/// Stable link identifier (`LinkView.id` in `docs/ipc.proto`, `TRANSITION_LINK.link_id`
/// primary key in the `docs/03-architecture.md` ERD, `[[topology.links]] id` in
/// `config.example.toml`). It is the key of the authored link, so it is unique
/// inside one document and survives re-layouts.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LinkId(String);

impl LinkId {
    /// Wraps a link id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for LinkId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<String> for LinkId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl AsRef<str> for LinkId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A transition between two nodes.
///
/// The field shape follows the *authored* link, not the wire message:
/// `LinkView` in `docs/ipc.proto` (`id`, `from`, `to`, `bidirectional`) and the
/// `TRANSITION_LINK` row of the `docs/03-architecture.md` ERD (`link_id` primary
/// key, `bidirectional = true` «creates reverse link»), which is also what
/// `config.example.toml` writes:
///
/// ```toml
/// [[topology.links]]
/// id = "win-right-to-a-left"
/// from = { node = "self", display = "edid:7a3f…", edge = "right", start = 200, end = 1240 }
/// to   = { node = "peer:9c1e…", display = "eDP-1", edge = "left", start = 0, end = 1080 }
/// bidirectional = true
/// ```
///
/// `protocol.proto TransitionLink` carries only the two endpoints: a conversion
/// to the wire therefore either expands a bidirectional link into two directed
/// links ([`TransitionLink::reversed`]) or drops the flag, and the `id` is not
/// on the wire at all and must be restored from the document.
///
/// When `bidirectional` is `true` the link is an exit on **both** segments:
/// leaving over `source` enters at `target` and leaving over `target` enters at
/// `source` (see `resolve_transition`).
#[derive(Clone, PartialEq, Debug)]
pub struct TransitionLink {
    /// Stable link key, unique inside the document.
    pub id: LinkId,
    /// Where the pointer leaves when travelling forward.
    pub source: Endpoint,
    /// Where the pointer appears when travelling forward.
    pub target: Endpoint,
    /// `true` when the reverse direction is an exit as well.
    pub bidirectional: bool,
}

impl TransitionLink {
    /// Builds a link.
    pub fn new(
        id: impl Into<LinkId>,
        source: Endpoint,
        target: Endpoint,
        bidirectional: bool,
    ) -> Self {
        Self {
            id: id.into(),
            source,
            target,
            bidirectional,
        }
    }

    /// The same link with source and target swapped, keeping the id.
    ///
    /// This is the expansion of `bidirectional = true` into a directed wire
    /// `TransitionLink`; it is not a second authored link.
    pub fn reversed(&self) -> Self {
        Self {
            id: self.id.clone(),
            source: self.target.clone(),
            target: self.source.clone(),
            bidirectional: self.bidirectional,
        }
    }
}

/// One member's display set inside the document (`NodeDisplays` in
/// `protocol.proto`).
#[derive(Clone, PartialEq, Debug)]
pub struct NodeDisplays {
    /// Member identity.
    pub node: NodeId,
    /// Displays of that member, in the member's own logical space.
    pub displays: Vec<Display>,
    /// Advisory liveness hint for the GUI; never a primary decision input.
    pub online: bool,
}

impl NodeDisplays {
    /// Builds a member entry.
    pub fn new(node: NodeId, displays: Vec<Display>, online: bool) -> Self {
        Self {
            node,
            displays,
            online,
        }
    }

    /// Display by stable identifier.
    pub fn display(&self, id: &DisplayId) -> Option<&Display> {
        self.displays.iter().find(|display| &display.id == id)
    }

    /// Smallest logical rectangle containing every display, `None` when the
    /// member has no displays. Negative origins are preserved.
    pub fn bounds(&self) -> Option<Rect> {
        let mut iter = self.displays.iter();
        let first = iter.next()?.logical_bounds;
        Some(iter.fold(first, |acc, display| acc.union(&display.logical_bounds)))
    }
}

/// The replicated layout document (`Topology` in `protocol.proto`).
#[derive(Clone, PartialEq, Debug)]
pub struct Topology {
    /// Applied revision, monotone; authored only by the current owner.
    pub revision: u64,
    /// Author of the applied revision; applied order is `(revision, author)`.
    pub author: NodeId,
    /// Members and their displays.
    pub nodes: Vec<NodeDisplays>,
    /// Transition links.
    pub links: Vec<TransitionLink>,
}

impl Topology {
    /// Builds a document.
    pub fn new(
        revision: u64,
        author: NodeId,
        nodes: Vec<NodeDisplays>,
        links: Vec<TransitionLink>,
    ) -> Self {
        Self {
            revision,
            author,
            nodes,
            links,
        }
    }

    /// Member entry by identity.
    pub fn node(&self, node: &NodeId) -> Option<&NodeDisplays> {
        self.nodes.iter().find(|entry| &entry.node == node)
    }

    /// Display of a member by identity.
    pub fn display(&self, node: &NodeId, display: &DisplayId) -> Option<&Display> {
        self.node(node)?.display(display)
    }
}

/// Pointer position in display-local logical coordinates (`Position` in
/// `protocol.proto`).
///
/// `x`/`y` are relative to the top-left corner of `display_id` and normally lie
/// in `[0, width) × [0, height)`. Coordinates outside that range mean the pointer
/// has left the display and are the input of a transition.
#[derive(Clone, PartialEq, Debug)]
pub struct Position {
    /// Display the coordinates are relative to.
    pub display_id: DisplayId,
    /// Local `x`.
    pub x: f64,
    /// Local `y`.
    pub y: f64,
}

impl Position {
    /// Builds a position.
    pub fn new(display_id: impl Into<DisplayId>, x: f64, y: f64) -> Self {
        Self {
            display_id: display_id.into(),
            x,
            y,
        }
    }
}

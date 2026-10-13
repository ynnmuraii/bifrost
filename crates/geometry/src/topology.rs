//! Topology document: nodes, displays, edge segments and transition links.
//!
//! Mirrors `Topology`, `NodeDisplays`, `Endpoint`, `TransitionLink` and
//! `Position` from `docs/protocol.proto`, and the layout document described in
//! `docs/03-architecture.md` (section «Топология и движение») and
//! `docs/config.example.toml` (`[topology]`).

use core::fmt;

use crate::display::{Display, DisplayId, Edge, NodeId, Rect, GEOMETRY_EPSILON};

/// The display id that marks an endpoint anchored at the *desktop* instead of at one display.
///
/// `[topology.links]` writes it as `display = "auto"` (`config.example.toml`), and the wire
/// carries the same string in `Endpoint.display_id`: an auto anchor is a display reference
/// like any other, so it needs no protocol field of its own. The anchor means «this edge of
/// this node's whole desktop» and is resolved against the live manifests by
/// [`resolve_auto_links`], so a monitor that is switched off, added back, or re-arranged
/// changes the resolved segments with no configuration edit.
pub const AUTO_DISPLAY_ID: &str = "auto";

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

    /// An endpoint anchored at `edge` of the whole desktop of `node` (`display = "auto"`).
    ///
    /// The segment is meaningless as it stands: [`resolve_auto_links`] derives it from the
    /// displays that lie on the desktop's outer boundary *right now*, so an auto endpoint never
    /// reaches a transition unexpanded.
    pub fn auto(node: NodeId, edge: Edge) -> Self {
        Self {
            node,
            display: DisplayId::new(AUTO_DISPLAY_ID),
            edge,
            segment: Segment::new(0.0, 0.0),
        }
    }

    /// `true` when this endpoint names the desktop, not one display.
    pub fn is_auto(&self) -> bool {
        self.display.as_str() == AUTO_DISPLAY_ID
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

/// Displays of `member` whose `edge` side lies on the outer boundary of the member's display
/// bounding box, in the order along that edge (top to bottom for `Left`/`Right`, left to right
/// for `Top`/`Bottom`). Empty when the member reports no display at all.
///
/// This is what «this edge of the whole desktop» means: `right` is every display whose right
/// side equals the bounding box' right side, which is the right-most monitor of a row and
/// *both* monitors of a vertical stack.
pub fn boundary_displays(member: &NodeDisplays, edge: Edge) -> Vec<&Display> {
    let Some(bounds) = member.bounds() else {
        return Vec::new();
    };
    let outer = match edge {
        Edge::Left => bounds.left(),
        Edge::Right => bounds.right(),
        Edge::Top => bounds.top(),
        Edge::Bottom => bounds.bottom(),
    };
    let mut displays: Vec<&Display> = member
        .displays
        .iter()
        .filter(|display| {
            let side = match edge {
                Edge::Left => display.logical_bounds.left(),
                Edge::Right => display.logical_bounds.right(),
                Edge::Top => display.logical_bounds.top(),
                Edge::Bottom => display.logical_bounds.bottom(),
            };
            (side - outer).abs() <= GEOMETRY_EPSILON
        })
        .collect();
    displays.sort_by(|left, right| {
        edge_axis_interval(left, edge)
            .0
            .total_cmp(&edge_axis_interval(right, edge).0)
    });
    displays
}

/// The interval a display occupies along `edge`'s axis, in node logical coordinates:
/// `(top, bottom)` for a vertical edge, `(left, right)` for a horizontal one.
fn edge_axis_interval(display: &Display, edge: Edge) -> (f64, f64) {
    if edge.is_vertical() {
        (
            display.logical_bounds.top(),
            display.logical_bounds.bottom(),
        )
    } else {
        (
            display.logical_bounds.left(),
            display.logical_bounds.right(),
        )
    }
}

/// One concrete display contributing an interval of one side's edge axis.
///
/// `range` is the interval in the axis' own units and `local` the same interval as a segment
/// of the display's own edge, so a crossing inside `local` can be handed to
/// [`crate::transition::map_endpoints`] unchanged.
#[derive(Clone, Copy)]
struct Extent<'a> {
    display: &'a Display,
    range: (f64, f64),
    local: (f64, f64),
}

/// The extents an authored endpoint stands for, against the member's live displays.
///
/// An auto endpoint becomes every boundary display, each with its full edge; a concrete
/// endpoint stays the one display and segment it names (or nothing, when that display is not
/// reported right now).
fn side_extents<'a>(endpoint: &Endpoint, member: &'a NodeDisplays) -> Vec<Extent<'a>> {
    if endpoint.is_auto() {
        boundary_displays(member, endpoint.edge)
            .into_iter()
            .filter_map(|display| {
                let (low, high) = edge_axis_interval(display, endpoint.edge);
                if high - low <= GEOMETRY_EPSILON {
                    // A display with no extent along this edge contributes no segment.
                    return None;
                }
                Some(Extent {
                    display,
                    range: (low, high),
                    local: (0.0, high - low),
                })
            })
            .collect()
    } else {
        match member.display(&endpoint.display) {
            Some(display) => vec![Extent {
                display,
                range: (endpoint.segment.start, endpoint.segment.end),
                local: (endpoint.segment.start, endpoint.segment.end),
            }],
            None => Vec::new(),
        }
    }
}

/// [`Extent::range`] union over a whole side, `None` when the side has no extent at all.
fn side_union(extents: &[Extent<'_>]) -> Option<(f64, f64)> {
    let mut iter = extents.iter();
    let first = iter.next()?.range;
    let (low, high) = iter.fold(first, |(low, high), extent| {
        (low.min(extent.range.0), high.max(extent.range.1))
    });
    if high - low <= GEOMETRY_EPSILON {
        None
    } else {
        Some((low, high))
    }
}

/// The affine image of `value` from the axis `from` onto the axis `to`: the whole of `from`
/// spans the whole of `to`, which is the proportional mapping the transition math uses inside
/// one link.
fn map_axis(value: f64, from: (f64, f64), to: (f64, f64)) -> f64 {
    to.0 + (value - from.0) / (from.1 - from.0) * (to.1 - to.0)
}

/// The document's links with every desktop-anchored (`auto`) endpoint expanded into concrete
/// display endpoints, against the member manifests the document carries.
///
/// One authored auto link can become several concrete links — one per pair of overlapping
/// extents — and each of them is an ordinary [`TransitionLink`], so
/// [`crate::transition::resolve_motion`] and [`crate::transition::map_endpoints`] do the
/// crossing and the proportional entry mapping with no second mapping engine.
///
/// The mapping is proportional over the *whole* edge of each side: a source display covering
/// the upper half of its desktop's edge maps onto the upper half of the peer's edge, which is
/// the same rule a hand-written link expresses by its two segments.
///
/// A link whose auto endpoint names a member with no displays (or none on that edge) cannot be
/// resolved; it is reported in [`AutoResolution::unresolved`] and left out, so a node that has
/// not reported its monitors yet disables nothing else.
#[must_use]
pub fn resolve_auto_links(topology: &Topology) -> AutoResolution {
    let mut links: Vec<TransitionLink> = Vec::with_capacity(topology.links.len());
    let mut unresolved: Vec<LinkId> = Vec::new();

    for link in &topology.links {
        if !link.source.is_auto() && !link.target.is_auto() {
            links.push(link.clone());
            continue;
        }
        let (Some(source_member), Some(target_member)) = (
            topology.node(&link.source.node),
            topology.node(&link.target.node),
        ) else {
            unresolved.push(link.id.clone());
            continue;
        };
        let from = side_extents(&link.source, source_member);
        let to = side_extents(&link.target, target_member);
        let (Some(from_union), Some(to_union)) = (side_union(&from), side_union(&to)) else {
            unresolved.push(link.id.clone());
            continue;
        };

        let mut produced: Vec<TransitionLink> = Vec::new();
        for source in &from {
            let mapped = (
                map_axis(source.range.0, from_union, to_union),
                map_axis(source.range.1, from_union, to_union),
            );
            for target in &to {
                let low = mapped.0.max(target.range.0);
                let high = mapped.1.min(target.range.1);
                if high - low <= GEOMETRY_EPSILON {
                    continue;
                }
                // Back to the source axis and into the source display's own edge, and into
                // the target display's own edge: the two segments are affine images of one
                // another, which is the invariant `map_endpoints` relies on.
                let source_low =
                    source.local.0 + map_axis(low, to_union, from_union) - source.range.0;
                let source_high =
                    source.local.0 + map_axis(high, to_union, from_union) - source.range.0;
                produced.push(TransitionLink::new(
                    link.id.clone(),
                    Endpoint::new(
                        link.source.node,
                        source.display.id.clone(),
                        link.source.edge,
                        Segment::new(source_low, source_high),
                    ),
                    Endpoint::new(
                        link.target.node,
                        target.display.id.clone(),
                        link.target.edge,
                        Segment::new(
                            target.local.0 + low - target.range.0,
                            target.local.0 + high - target.range.0,
                        ),
                    ),
                    link.bidirectional,
                ));
            }
        }

        match produced.len() {
            0 => unresolved.push(link.id.clone()),
            1 => links.append(&mut produced),
            _ => {
                for (index, mut link) in produced.into_iter().enumerate() {
                    link.id = LinkId::new(format!("{}#{index}", link.id));
                    links.push(link);
                }
            }
        }
    }

    AutoResolution { links, unresolved }
}

/// [`resolve_auto_links`]' result: the concrete links plus the authored links that could not
/// be resolved against the manifests.
#[derive(Clone, PartialEq, Debug)]
pub struct AutoResolution {
    /// The document's links, in document order, with every auto endpoint expanded.
    pub links: Vec<TransitionLink>,
    /// The auto links that name a member with no displays on the requested edge.
    pub unresolved: Vec<LinkId>,
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

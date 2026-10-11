//! Validation of a [`Topology`] document against the layout invariants.
//!
//! The rules come from `docs/03-architecture.md`:
//!
//! * «Правила целостности» item 1 — `TRANSITION_LINK.from_*`/`to_*` reference an
//!   existing `DISPLAY`; the nodes are different; segments do not overlap on one
//!   edge; segments do not lie in a «дырка» between monitors.
//! * `TRANSITION_LINK.link_id` is the primary key of the document, so ids are
//!   unique; `bidirectional = true` «creates reverse link», which makes the
//!   `to` endpoint an exit too — both endpoints are checked as exit segments.
//! * «Топология и движение» — «Углы, неоднозначные/перекрывающиеся links и
//!   сегмент в «дырке» между мониторами не допускаются валидатором» and «на
//!   Wayland — только на внешней границе зоны».
//! * `config.example.toml` — `[topology.links]` must satisfy the same rules;
//!   violating documents surface as `INVALID_TOPOLOGY`.
//!
//! See the crate documentation for how «углы», «дырка» and the backend note are
//! interpreted.

use core::fmt;
use std::error::Error;

use crate::display::{Display, DisplayId, Edge, NodeId, GEOMETRY_EPSILON};
use crate::topology::{Endpoint, LinkId, Segment, Topology};

/// Which end of a link an error refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndpointSide {
    /// `link.source`.
    Source,
    /// `link.target`.
    Target,
}

/// What is wrong with a display definition itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayDefect {
    /// `logical_bounds` or `native_bounds` contain a non-finite number.
    NonFiniteBounds,
    /// `logical_bounds` has a non-positive width or height.
    NonPositiveSize,
    /// `scale_x`/`scale_y` is not finite and strictly positive.
    InvalidScale,
}

/// Reference to a display inside the document.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DisplayRef {
    /// Owning node.
    pub node: NodeId,
    /// Display identifier.
    pub display: DisplayId,
}

/// Reference to a display edge inside the document.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EdgeRef {
    /// Owning node.
    pub node: NodeId,
    /// Display identifier.
    pub display: DisplayId,
    /// Edge.
    pub edge: Edge,
}

/// A corner of a display rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corner {
    /// Left/top corner.
    TopLeft,
    /// Right/top corner.
    TopRight,
    /// Right/bottom corner.
    BottomRight,
    /// Left/bottom corner.
    BottomLeft,
}

/// One violated invariant.
#[derive(Clone, PartialEq, Debug)]
pub enum TopologyError {
    /// The same node id appears twice in `Topology.nodes`.
    DuplicateNode {
        /// Duplicated node.
        node: NodeId,
    },
    /// The same display id appears twice inside one member.
    DuplicateDisplay {
        /// Owning node.
        node: NodeId,
        /// Duplicated display.
        display: DisplayId,
    },
    /// A display definition cannot be used geometrically.
    InvalidDisplay {
        /// Owning node.
        node: NodeId,
        /// Broken display.
        display: DisplayId,
        /// What is broken.
        defect: DisplayDefect,
    },
    /// The same link id appears twice in `Topology.links`; `TRANSITION_LINK.link_id`
    /// is the primary key of the authored document.
    DuplicateLink {
        /// Duplicated link id.
        id: LinkId,
        /// Index of the first link carrying this id.
        first_link: usize,
        /// Index of the repeated link.
        second_link: usize,
    },
    /// A link endpoint references a node that is not in `Topology.nodes`.
    UnknownNode {
        /// Index into `Topology.links`.
        link: usize,
        /// Which end.
        side: EndpointSide,
        /// Missing node.
        node: NodeId,
    },
    /// A link endpoint references a display that is not on that node.
    UnknownDisplay {
        /// Index into `Topology.links`.
        link: usize,
        /// Which end.
        side: EndpointSide,
        /// Owning node.
        node: NodeId,
        /// Missing display.
        display: DisplayId,
    },
    /// Both endpoints of a link are on the same node.
    SameNode {
        /// Index into `Topology.links`.
        link: usize,
        /// The shared node.
        node: NodeId,
    },
    /// The segment is not a valid interval on the referenced edge.
    InvalidSegment {
        /// Index into `Topology.links`.
        link: usize,
        /// Which end.
        side: EndpointSide,
        /// Offending segment.
        segment: Segment,
        /// Length of the referenced edge.
        edge_length: f64,
    },
    /// Two link endpoints share one display edge and their segments overlap.
    SegmentOverlap {
        /// Shared edge.
        edge: EdgeRef,
        /// First link index.
        first_link: usize,
        /// Second link index.
        second_link: usize,
        /// Overlapping stretch.
        overlap: Segment,
    },
    /// The segment is not on the outer boundary of the node: another display of
    /// the same node is adjacent across that stretch of the edge.
    SegmentNotOuter {
        /// Index into `Topology.links`.
        link: usize,
        /// Which end.
        side: EndpointSide,
        /// Stretch covered by the neighbouring local display.
        covered: Segment,
    },
    /// The segment faces an empty gap with another local display beyond it
    /// («дырка» between monitors), so a pointer crossing there is ambiguous.
    SegmentInHole {
        /// Index into `Topology.links`.
        link: usize,
        /// Which end.
        side: EndpointSide,
    },
    /// Two links meet at one display corner, so a pointer arriving exactly at
    /// the corner has no unique destination.
    AmbiguousCorner {
        /// Display that owns the corner.
        display: DisplayRef,
        /// The shared corner.
        corner: Corner,
        /// The two edges involved.
        edges: (Edge, Edge),
    },
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopologyError::DuplicateNode { node } => write!(f, "duplicate node {node}"),
            TopologyError::DuplicateDisplay { node, display } => {
                write!(f, "node {node}: duplicate display {display}")
            }
            TopologyError::InvalidDisplay {
                node,
                display,
                defect,
            } => write!(f, "node {node}: display {display}: {defect:?}"),
            TopologyError::DuplicateLink {
                id,
                first_link,
                second_link,
            } => write!(
                f,
                "links {first_link} and {second_link} share the id \"{id}\""
            ),
            TopologyError::UnknownNode { link, side, node } => {
                write!(f, "link {link} ({side:?}): unknown node {node}")
            }
            TopologyError::UnknownDisplay {
                link,
                side,
                node,
                display,
            } => write!(f, "link {link} ({side:?}): node {node} has no display {display}"),
            TopologyError::SameNode { link, node } => {
                write!(f, "link {link}: both endpoints are on node {node}")
            }
            TopologyError::InvalidSegment {
                link,
                side,
                segment,
                edge_length,
            } => write!(
                f,
                "link {link} ({side:?}): segment [{}, {}] is not a valid interval on a {edge_length}-long edge",
                segment.start, segment.end
            ),
            TopologyError::SegmentOverlap {
                edge,
                first_link,
                second_link,
                overlap,
            } => write!(
                f,
                "links {first_link} and {second_link} overlap on {}/{} {:?} edge in [{}, {}]",
                edge.node, edge.display, edge.edge, overlap.start, overlap.end
            ),
            TopologyError::SegmentNotOuter {
                link,
                side,
                covered,
            } => write!(
                f,
                "link {link} ({side:?}): segment is not on the outer boundary, covered in [{}, {}]",
                covered.start, covered.end
            ),
            TopologyError::SegmentInHole { link, side } => write!(
                f,
                "link {link} ({side:?}): segment faces a gap (hole) between local monitors"
            ),
            TopologyError::AmbiguousCorner {
                display,
                corner,
                edges,
            } => write!(
                f,
                "node {} display {}: links meet at {corner:?} ({:?} and {:?})",
                display.node, display.display, edges.0, edges.1
            ),
        }
    }
}

impl Error for TopologyError {}

/// Every invariant the document violates.
#[derive(Clone, PartialEq, Debug)]
pub struct ValidationErrors(Vec<TopologyError>);

impl ValidationErrors {
    /// The individual violations, in document order.
    pub fn errors(&self) -> &[TopologyError] {
        &self.0
    }

    /// `true` when nothing was reported.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of violations.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Consumes the wrapper and returns the violations.
    pub fn into_errors(self) -> Vec<TopologyError> {
        self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} topology violation(s)", self.0.len())?;
        for error in &self.0 {
            write!(f, "; {error}")?;
        }
        Ok(())
    }
}

impl Error for ValidationErrors {}

impl IntoIterator for ValidationErrors {
    type Item = TopologyError;
    type IntoIter = std::vec::IntoIter<TopologyError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Validates a whole document.
///
/// Returns every violation found (not just the first), in a deterministic order:
/// member/display defects first, then links from index `0`, each with its source
/// endpoint before its target endpoint, then overlap and corner conflicts.
pub fn validate_topology(topology: &Topology) -> Result<(), ValidationErrors> {
    let mut errors = Vec::new();

    let mut seen_nodes: Vec<NodeId> = Vec::new();
    for member in &topology.nodes {
        if seen_nodes.contains(&member.node) {
            errors.push(TopologyError::DuplicateNode { node: member.node });
        } else {
            seen_nodes.push(member.node);
        }

        let mut seen_displays: Vec<DisplayId> = Vec::new();
        for display in &member.displays {
            if seen_displays.contains(&display.id) {
                errors.push(TopologyError::DuplicateDisplay {
                    node: member.node,
                    display: display.id.clone(),
                });
                continue;
            }
            seen_displays.push(display.id.clone());
            if let Some(defect) = display_defect(display) {
                errors.push(TopologyError::InvalidDisplay {
                    node: member.node,
                    display: display.id.clone(),
                    defect,
                });
            }
        }
    }

    validate_link_ids(topology, &mut errors);
    validate_link_endpoints(topology, &mut errors);
    validate_edge_conflicts(topology, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}

/// Structural defect of a display, if any.
fn display_defect(display: &Display) -> Option<DisplayDefect> {
    if !display.logical_bounds.is_finite() {
        return Some(DisplayDefect::NonFiniteBounds);
    }
    if let Some(native) = display.native_bounds {
        if !native.is_finite() {
            return Some(DisplayDefect::NonFiniteBounds);
        }
    }
    if !display.logical_bounds.has_positive_area() {
        return Some(DisplayDefect::NonPositiveSize);
    }
    if !display.scale.is_valid() {
        return Some(DisplayDefect::InvalidScale);
    }
    None
}

/// Link ids are the primary key of the authored document (`TRANSITION_LINK.link_id`).
fn validate_link_ids(topology: &Topology, errors: &mut Vec<TopologyError>) {
    let mut seen: Vec<(&LinkId, usize)> = Vec::new();
    for (index, link) in topology.links.iter().enumerate() {
        match seen.iter().find(|(id, _)| **id == link.id) {
            Some((_, first)) => errors.push(TopologyError::DuplicateLink {
                id: link.id.clone(),
                first_link: *first,
                second_link: index,
            }),
            None => seen.push((&link.id, index)),
        }
    }
}

fn validate_link_endpoints(topology: &Topology, errors: &mut Vec<TopologyError>) {
    for (index, link) in topology.links.iter().enumerate() {
        if link.source.node == link.target.node {
            errors.push(TopologyError::SameNode {
                link: index,
                node: link.source.node,
            });
        }

        for (side, endpoint) in [
            (EndpointSide::Source, &link.source),
            (EndpointSide::Target, &link.target),
        ] {
            let Some(member) = topology.node(&endpoint.node) else {
                errors.push(TopologyError::UnknownNode {
                    link: index,
                    side,
                    node: endpoint.node,
                });
                continue;
            };
            let Some(display) = member.display(&endpoint.display) else {
                errors.push(TopologyError::UnknownDisplay {
                    link: index,
                    side,
                    node: endpoint.node,
                    display: endpoint.display.clone(),
                });
                continue;
            };
            if display_defect(display).is_some() {
                continue;
            }
            validate_segment_of_endpoint(topology, index, side, endpoint, display, errors);
        }
    }
}

fn validate_segment_of_endpoint(
    topology: &Topology,
    index: usize,
    side: EndpointSide,
    endpoint: &Endpoint,
    display: &Display,
    errors: &mut Vec<TopologyError>,
) {
    let edge_length = display.edge_length(endpoint.edge);
    if !endpoint.segment.is_valid(edge_length) {
        errors.push(TopologyError::InvalidSegment {
            link: index,
            side,
            segment: endpoint.segment,
            edge_length,
        });
        return;
    }

    let mut covered: Vec<Segment> = Vec::new();
    let mut holes: Vec<Segment> = Vec::new();
    if let Some(member) = topology.node(&endpoint.node) {
        for sibling in &member.displays {
            if sibling.id == display.id {
                continue;
            }
            let Some(interval) = sibling_edge_interval(display, endpoint.edge, sibling) else {
                continue;
            };
            if sibling_is_adjacent(display, endpoint.edge, sibling) {
                covered.push(interval);
            } else if sibling_is_beyond(display, endpoint.edge, sibling) {
                holes.push(interval);
            }
        }
    }

    if let Some(overlap) = first_overlap(endpoint.segment, &merge(&mut covered)) {
        errors.push(TopologyError::SegmentNotOuter {
            link: index,
            side,
            covered: overlap,
        });
    }
    if first_overlap(endpoint.segment, &merge(&mut holes)).is_some() {
        errors.push(TopologyError::SegmentInHole { link: index, side });
    }
}

/// Perpendicular extent of `sibling` mapped to along-edge distances of
/// `display`'s `edge`, clipped to the edge, or `None` when empty.
fn sibling_edge_interval(display: &Display, edge: Edge, sibling: &Display) -> Option<Segment> {
    let base = display.logical_bounds;
    let other = sibling.logical_bounds;
    let (low, high) = if edge.is_vertical() {
        (other.top() - base.top(), other.bottom() - base.top())
    } else {
        (other.left() - base.left(), other.right() - base.left())
    };
    let length = display.edge_length(edge);
    let segment = Segment::new(low.max(0.0), high.min(length));
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

/// The sibling touches the edge from outside.
fn sibling_is_adjacent(display: &Display, edge: Edge, sibling: &Display) -> bool {
    let base = display.logical_bounds;
    let other = sibling.logical_bounds;
    match edge {
        Edge::Left => (base.left() - other.right()).abs() <= GEOMETRY_EPSILON,
        Edge::Right => (other.left() - base.right()).abs() <= GEOMETRY_EPSILON,
        Edge::Top => (base.top() - other.bottom()).abs() <= GEOMETRY_EPSILON,
        Edge::Bottom => (other.top() - base.bottom()).abs() <= GEOMETRY_EPSILON,
    }
}

/// The sibling lies outside the edge with a gap in between.
fn sibling_is_beyond(display: &Display, edge: Edge, sibling: &Display) -> bool {
    let base = display.logical_bounds;
    let other = sibling.logical_bounds;
    match edge {
        Edge::Left => other.right() < base.left() - GEOMETRY_EPSILON,
        Edge::Right => other.left() > base.right() + GEOMETRY_EPSILON,
        Edge::Top => other.bottom() < base.top() - GEOMETRY_EPSILON,
        Edge::Bottom => other.top() > base.bottom() + GEOMETRY_EPSILON,
    }
}

/// Endpoint segment registered under one display edge, for overlap detection.
struct LinkSegment {
    link: usize,
    segment: Segment,
}

/// Endpoint segment registered under one display, for corner detection.
struct EdgeSegment {
    edge: Edge,
    segment: Segment,
    edge_length: f64,
}

/// Sorts and coalesces segments; empty input yields an empty vector.
fn merge(segments: &mut [Segment]) -> Vec<Segment> {
    segments.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments.iter() {
        match merged.last_mut() {
            Some(last) if segment.start <= last.end + GEOMETRY_EPSILON => {
                last.end = last.end.max(segment.end);
            }
            _ => merged.push(*segment),
        }
    }
    merged
}

fn first_overlap(segment: Segment, others: &[Segment]) -> Option<Segment> {
    others.iter().find_map(|other| segment.overlap(other))
}

fn validate_edge_conflicts(topology: &Topology, errors: &mut Vec<TopologyError>) {
    let mut per_edge: Vec<(EdgeRef, Vec<LinkSegment>)> = Vec::new();
    let mut per_display: Vec<(DisplayRef, Vec<EdgeSegment>)> = Vec::new();

    for (index, link) in topology.links.iter().enumerate() {
        for endpoint in [&link.source, &link.target] {
            let Some(display) = topology.display(&endpoint.node, &endpoint.display) else {
                continue;
            };
            if display_defect(display).is_some() {
                continue;
            }
            let length = display.edge_length(endpoint.edge);
            if !endpoint.segment.is_valid(length) {
                continue;
            }

            let edge_ref = EdgeRef {
                node: endpoint.node,
                display: endpoint.display.clone(),
                edge: endpoint.edge,
            };
            let record = LinkSegment {
                link: index,
                segment: endpoint.segment,
            };
            match per_edge.iter_mut().find(|(key, _)| *key == edge_ref) {
                Some((_, list)) => list.push(record),
                None => per_edge.push((edge_ref, vec![record])),
            }

            let display_ref = DisplayRef {
                node: endpoint.node,
                display: endpoint.display.clone(),
            };
            let entry = EdgeSegment {
                edge: endpoint.edge,
                segment: endpoint.segment,
                edge_length: length,
            };
            match per_display.iter_mut().find(|(key, _)| *key == display_ref) {
                Some((_, list)) => list.push(entry),
                None => per_display.push((display_ref, vec![entry])),
            }
        }
    }

    for (edge, segments) in &per_edge {
        for (position, first) in segments.iter().enumerate() {
            for second in &segments[position + 1..] {
                if let Some(overlap) = first.segment.overlap(&second.segment) {
                    errors.push(TopologyError::SegmentOverlap {
                        edge: edge.clone(),
                        first_link: first.link,
                        second_link: second.link,
                        overlap,
                    });
                }
            }
        }
    }

    for (display, entries) in &per_display {
        for (position, first) in entries.iter().enumerate() {
            for second in &entries[position + 1..] {
                if let Some(corner) = shared_corner(first, second) {
                    errors.push(TopologyError::AmbiguousCorner {
                        display: display.clone(),
                        corner,
                        edges: (first.edge, second.edge),
                    });
                }
            }
        }
    }
}

/// Corner touched by both segments, if any.
fn shared_corner(first: &EdgeSegment, second: &EdgeSegment) -> Option<Corner> {
    if first.edge == second.edge {
        return None;
    }
    [
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomRight,
        Corner::BottomLeft,
    ]
    .into_iter()
    .find(|corner| {
        touches_corner(first.edge, first.segment, first.edge_length, *corner)
            && touches_corner(second.edge, second.segment, second.edge_length, *corner)
    })
}

fn touches_corner(edge: Edge, segment: Segment, edge_length: f64, corner: Corner) -> bool {
    match (edge, corner) {
        (Edge::Left | Edge::Top, Corner::TopLeft) => segment.start <= GEOMETRY_EPSILON,
        (Edge::Right | Edge::Top, Corner::TopRight) => {
            segment.end >= edge_length - GEOMETRY_EPSILON
        }
        (Edge::Right | Edge::Bottom, Corner::BottomRight) => {
            segment.end >= edge_length - GEOMETRY_EPSILON
        }
        (Edge::Left | Edge::Bottom, Corner::BottomLeft) => segment.start <= GEOMETRY_EPSILON,
        _ => false,
    }
}

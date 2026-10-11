//! Edge-crossing transitions.
//!
//! Implements the formula from `docs/03-architecture.md` (section «Координаты»
//! and «Топология и движение»):
//!
//! ```text
//! t = clamp((p − from.start) / (from.end − from.start), 0, 1)
//! target = to.start + t · (to.end − to.start)
//! ```
//!
//! followed by «Поставить курсор на 2 logical px внутрь целевого экрана», i.e.
//! [`ENTRY_INSET_LOGICAL`] logical units inside the target display.
//!
//! Everything stays in *logical* units: `Endpoint.segment_start/end` are
//! «в logical units дисплея» and the sender «никогда не пересчитывает координаты
//! через `scale_*`». Native pixels are only produced on demand by
//! [`Transition::target_native`] / [`Display::logical_to_native`].

use crate::display::{Display, Edge, NodeId, Point, Rect, GEOMETRY_EPSILON};
use crate::topology::{Endpoint, Position, Topology, TransitionLink};

/// Distance the pointer is placed inside the target display, in logical units.
pub const ENTRY_INSET_LOGICAL: f64 = 2.0;

/// The edge a pointer left, and where along it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EdgeCrossing {
    /// Edge that was crossed.
    pub edge: Edge,
    /// Distance along that edge, measured like [`Display::point_on_edge`] and
    /// clamped to `[0, edge_length]`.
    pub along: f64,
    /// How far beyond the edge the supplied position already was; `0` when the
    /// crossing was derived from a motion step that ends exactly on the edge.
    pub overshoot: f64,
}

/// Which direction of a [`TransitionLink`] a transition took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkDirection {
    /// `source` → `target`.
    Forward,
    /// `target` → `source`; only possible when the link is bidirectional.
    Reverse,
}

/// A resolved transition.
#[derive(Clone, PartialEq, Debug)]
pub struct Transition {
    /// Index of the matched link in `Topology.links`.
    pub link: usize,
    /// Which direction of that link was taken.
    pub direction: LinkDirection,
    /// Exit endpoint that was crossed, on the local node.
    pub source: Endpoint,
    /// Entry endpoint the pointer appears on, on the remote node.
    pub target: Endpoint,
    /// Clamped position along the source segment, `[0, 1]`.
    pub t: f64,
    /// Pointer position on the target display, in its local logical units.
    pub target_position: Position,
}

impl Transition {
    /// The same target position in device pixels, using the target display's
    /// scale. `None` when the target display has no native bounds
    /// (`native_known = false`), or when the target display passed in does not
    /// match the transition target.
    pub fn target_native(&self, target_display: &Display) -> Option<Position> {
        if target_display.id != self.target.display {
            return None;
        }
        let native = target_display
            .logical_to_native(Point::new(self.target_position.x, self.target_position.y))?;
        Some(Position::new(target_display.id.clone(), native.x, native.y))
    }
}

/// Edge a *position* has already left, if any.
///
/// `position` is display-local (`protocol.proto`: `Position.x // local to display
/// top-left`; `03-architecture.md`, «Координаты»), so the four edges are at `0` and
/// at the display's logical size.
///
/// A point inside `[0, width) × [0, height)` is not a crossing. A point on an edge
/// counts as that edge (overshoot `0`). A point beyond a corner is attributed to
/// the edge with the largest overshoot; ties resolve in the order `Right`, `Left`,
/// `Bottom`, `Top`. `along` is clamped to the edge.
pub fn crossing(display: &Display, position: &Position) -> Option<EdgeCrossing> {
    if display.id != position.display_id {
        return None;
    }
    let size = display.logical_size();
    let candidates = [
        (Edge::Right, position.x - size.width),
        (Edge::Left, 0.0 - position.x),
        (Edge::Bottom, position.y - size.height),
        (Edge::Top, 0.0 - position.y),
    ];

    let mut best: Option<(Edge, f64)> = None;
    for (edge, overshoot) in candidates {
        if overshoot >= 0.0 && best.is_none_or(|(_, previous)| overshoot > previous) {
            best = Some((edge, overshoot));
        }
    }
    let (edge, overshoot) = best?;

    let along = if edge.is_vertical() {
        position.y.clamp(0.0, size.height)
    } else {
        position.x.clamp(0.0, size.width)
    };
    Some(EdgeCrossing {
        edge,
        along,
        overshoot,
    })
}

/// Edge crossed by a motion step, if the step leaves the display.
///
/// Handles the case the position alone cannot: a very fast motion that starts
/// inside the display and ends beyond the edge (or beyond a corner). A zero
/// motion, or a step that stays inside, yields `None`.
pub fn crossing_with_motion(
    display: &Display,
    position: &Position,
    motion: Point,
) -> Option<EdgeCrossing> {
    if let Some(already) = crossing(display, position) {
        return Some(already);
    }
    if display.id != position.display_id {
        return None;
    }
    // Display-local again, as in `crossing`: `position` is relative to the display's
    // top-left, so the local rectangle starts at the origin.
    let size = display.logical_size();
    let local = Rect::new(0.0, 0.0, size.width, size.height);
    let start = Point::new(position.x, position.y);
    if !local.contains(start) {
        return None;
    }
    let (dx, dy) = (motion.x, motion.y);
    if dx == 0.0 && dy == 0.0 {
        return None;
    }

    let exit_x = if dx > 0.0 {
        (size.width - start.x) / dx
    } else if dx < 0.0 {
        (0.0 - start.x) / dx
    } else {
        f64::INFINITY
    };
    let exit_y = if dy > 0.0 {
        (size.height - start.y) / dy
    } else if dy < 0.0 {
        (0.0 - start.y) / dy
    } else {
        f64::INFINITY
    };

    let (t, edge) = if exit_x <= exit_y {
        (exit_x, if dx > 0.0 { Edge::Right } else { Edge::Left })
    } else {
        (exit_y, if dy > 0.0 { Edge::Bottom } else { Edge::Top })
    };
    if t <= 0.0 || t > 1.0 {
        return None;
    }

    let hit = Point::new(start.x + dx * t, start.y + dy * t);
    let along = if edge.is_vertical() {
        hit.y.clamp(0.0, size.height)
    } else {
        hit.x.clamp(0.0, size.width)
    };
    Some(EdgeCrossing {
        edge,
        along,
        overshoot: 0.0,
    })
}

/// Maps an along-edge crossing onto the target segment (forward direction of
/// `link`, i.e. `source` → `target`). See [`map_endpoints`].
pub fn map_crossing(
    link: &TransitionLink,
    target_display: &Display,
    along: f64,
) -> (f64, Position) {
    map_endpoints(&link.source, &link.target, target_display, along)
}

/// Maps an along-edge crossing from one endpoint's segment onto another's.
///
/// Returns the clamped `t` and the target position in the target display's local
/// logical units, placed [`ENTRY_INSET_LOGICAL`] inside the `to` edge. The
/// perpendicular coordinate is the mapped along-edge distance, clamped to the
/// `to` edge (a segment that ends exactly on a corner yields the corner
/// coordinate).
///
/// This is the direction-agnostic core: the reverse direction of a bidirectional
/// link is `map_endpoints(&link.target, &link.source, …, along)`.
pub fn map_endpoints(
    from: &Endpoint,
    to: &Endpoint,
    target_display: &Display,
    along: f64,
) -> (f64, Position) {
    let from_len = from.segment.len();
    let t = if from_len <= GEOMETRY_EPSILON {
        0.0
    } else {
        ((along - from.segment.start) / from_len).clamp(0.0, 1.0)
    };
    let raw = to.segment.start + t * to.segment.len();

    let bounds = target_display.logical_bounds;
    let perpendicular = if to.edge.is_vertical() {
        raw.clamp(0.0, bounds.height)
    } else {
        raw.clamp(0.0, bounds.width)
    };
    let inset = |extent: f64| (extent - ENTRY_INSET_LOGICAL).max(0.0);

    let (x, y) = match to.edge {
        Edge::Left => (ENTRY_INSET_LOGICAL.min(bounds.width), perpendicular),
        Edge::Right => (inset(bounds.width), perpendicular),
        Edge::Top => (perpendicular, ENTRY_INSET_LOGICAL.min(bounds.height)),
        Edge::Bottom => (perpendicular, inset(bounds.height)),
    };
    (t, Position::new(target_display.id.clone(), x, y))
}

/// Resolves a position that is already outside — or exactly on — a local display
/// edge into a transition, if a link covers that crossing.
///
/// Links are tried in document order; a bidirectional link is an exit on both of
/// its segments, so leaving over its `target` yields
/// [`LinkDirection::Reverse`] and maps `target` → `source`.
pub fn resolve_transition(
    topology: &Topology,
    local_node: &NodeId,
    position: &Position,
) -> Option<Transition> {
    let member = topology.node(local_node)?;
    let display = member.display(&position.display_id)?;
    let crossing = crossing(display, position)?;
    build_transition(topology, local_node, display, crossing)
}

/// Resolves a motion step from a position inside a local display.
///
/// `motion` is a delta in the local display's logical units. Returns `None` when
/// the step stays inside the display, when there is no covering link, or when the
/// display does not belong to `local_node`. Bidirectional links are handled as in
/// [`resolve_transition`].
pub fn resolve_motion(
    topology: &Topology,
    local_node: &NodeId,
    position: &Position,
    motion: Point,
) -> Option<Transition> {
    let member = topology.node(local_node)?;
    let display = member.display(&position.display_id)?;
    let crossing = crossing_with_motion(display, position, motion)?;
    build_transition(topology, local_node, display, crossing)
}

fn build_transition(
    topology: &Topology,
    local_node: &NodeId,
    display: &Display,
    crossing: EdgeCrossing,
) -> Option<Transition> {
    for (index, link) in topology.links.iter().enumerate() {
        if is_exit(&link.source, local_node, display, crossing) {
            return assemble(
                topology,
                index,
                LinkDirection::Forward,
                &link.source,
                &link.target,
                crossing.along,
            );
        }
        if link.bidirectional && is_exit(&link.target, local_node, display, crossing) {
            return assemble(
                topology,
                index,
                LinkDirection::Reverse,
                &link.target,
                &link.source,
                crossing.along,
            );
        }
    }
    None
}

/// `endpoint` is an exit for a pointer leaving `display` at `crossing`.
fn is_exit(
    endpoint: &Endpoint,
    local_node: &NodeId,
    display: &Display,
    crossing: EdgeCrossing,
) -> bool {
    endpoint.node == *local_node
        && endpoint.display == display.id
        && endpoint.edge == crossing.edge
        && endpoint.segment.contains(crossing.along)
}

fn assemble(
    topology: &Topology,
    index: usize,
    direction: LinkDirection,
    from: &Endpoint,
    to: &Endpoint,
    along: f64,
) -> Option<Transition> {
    let target_display = topology.display(&to.node, &to.display)?;
    let (t, target_position) = map_endpoints(from, to, target_display, along);
    Some(Transition {
        link: index,
        direction,
        source: from.clone(),
        target: to.clone(),
        t,
        target_position,
    })
}

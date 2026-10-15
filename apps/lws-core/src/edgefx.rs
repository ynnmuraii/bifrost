//! The core's half of the edge effects: what to tell the overlay (in the agent), and when.
//!
//! The core owns the geometry and the pointer samples, so it decides; the agent only draws.
//! Frames are small, rate-limited to ~30 Hz in total ([`edgefx::coalesce`]) and carry no input.
//! A *linked* edge is exactly an edge [`geometry::resolve_motion`] would turn into a
//! transition, so the effect can never appear on an edge that would not cross (and follows
//! the auto-anchor rules for free).

use edgefx::coalesce::Coalescer;
use edgefx::{Edge, Target, Update, REACH_PX};
use geometry::{DisplayId, NodeId, Point, Position, Size, Topology};
use protocol::ipc;

/// `[appearance].edge_effects` plus the rate limiter and the "was near" memory.
#[derive(Debug)]
pub struct Emitter {
    enabled: bool,
    gate: Coalescer<String>,
    near: bool,
    entry_wanted: bool,
}

impl Emitter {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            gate: Coalescer::default(),
            near: false,
            entry_wanted: false,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.near = false;
    }

    /// A receiver lease was prepared: the next pointer position is where input arrived.
    pub fn await_entry(&mut self) {
        self.entry_wanted = true;
    }

    /// `true` once per prepared lease, on the first `Inject` after [`Self::await_entry`].
    pub fn take_entry_wanted(&mut self) -> bool {
        std::mem::take(&mut self.entry_wanted)
    }

    pub fn cancel_entry(&mut self) {
        self.entry_wanted = false;
    }

    /// Whether a proximity sample is worth computing at `now_ms` (cheap: a comparison).
    #[must_use]
    pub fn wants_sample(&self, now_ms: u64) -> bool {
        self.enabled && self.gate.ready(now_ms)
    }

    /// Offers a frame; the frame to send now, if the rate limit allows.
    pub fn push(&mut self, update: Update, display: &str, now_ms: u64) -> Option<ipc::EdgeFx> {
        if !self.enabled {
            return None;
        }
        self.near = matches!(update, Update::Proximity { .. });
        self.gate
            .push(update, display.to_owned(), now_ms)
            .map(|(update, display)| frame(&update, &display))
    }

    /// The pointer is not near a linked edge any more: one `LEFT` if it was.
    pub fn out_of_reach(&mut self, now_ms: u64) -> Option<ipc::EdgeFx> {
        if !self.near {
            return None;
        }
        self.push(Update::Left, "", now_ms)
    }

    /// A held frame whose spacing has elapsed.
    pub fn due(&mut self, now_ms: u64) -> Option<ipc::EdgeFx> {
        self.gate
            .due(now_ms)
            .map(|(update, display)| frame(&update, &display))
    }

    /// When a held frame may go, for the loop's wake-up.
    #[must_use]
    pub fn due_at(&self) -> Option<u64> {
        self.gate.due_at()
    }
}

const fn portal_edge(edge: Edge) -> ipc::PortalEdge {
    match edge {
        Edge::Left => ipc::PortalEdge::Left,
        Edge::Right => ipc::PortalEdge::Right,
        Edge::Top => ipc::PortalEdge::Top,
        Edge::Bottom => ipc::PortalEdge::Bottom,
    }
}

const fn from_geometry(edge: geometry::Edge) -> Edge {
    match edge {
        geometry::Edge::Left => Edge::Left,
        geometry::Edge::Right => Edge::Right,
        geometry::Edge::Top => Edge::Top,
        geometry::Edge::Bottom => Edge::Bottom,
    }
}

/// The IPC frame of one update.
#[must_use]
pub fn frame(update: &Update, display: &str) -> ipc::EdgeFx {
    let mut out = ipc::EdgeFx::default();
    let target = |out: &mut ipc::EdgeFx, target: &Target| {
        out.display_id = display.to_owned();
        out.edge = portal_edge(target.edge) as i32;
        out.seg_start = f64::from(target.start);
        out.seg_end = f64::from(target.end);
        out.along = f64::from(target.along);
    };
    match update {
        Update::Proximity {
            target: t,
            distance,
        } => {
            out.kind = ipc::EdgeFxKind::EdgeFxProximity as i32;
            target(&mut out, t);
            out.distance = f64::from(*distance);
        }
        Update::Crossed(t) => {
            out.kind = ipc::EdgeFxKind::EdgeFxCrossed as i32;
            if let Some(t) = t {
                target(&mut out, t);
            }
        }
        Update::Entered(t) => {
            out.kind = ipc::EdgeFxKind::EdgeFxEntered as i32;
            target(&mut out, t);
        }
        Update::Left => out.kind = ipc::EdgeFxKind::EdgeFxLeft as i32,
        Update::Clear => out.kind = ipc::EdgeFxKind::EdgeFxClear as i32,
    }
    out
}

/// The sides of a display with the pointer's distance to each.
fn sides(size: Size, x: f64, y: f64) -> [(geometry::Edge, f64, Point); 4] {
    [
        (geometry::Edge::Left, x, Point::new(-1.0, 0.0)),
        (geometry::Edge::Right, size.width - x, Point::new(1.0, 0.0)),
        (geometry::Edge::Top, y, Point::new(0.0, -1.0)),
        (
            geometry::Edge::Bottom,
            size.height - y,
            Point::new(0.0, 1.0),
        ),
    ]
}

/// The nearest linked edge within [`REACH_PX`] of `position`, as a `Proximity` update.
///
/// "Linked" is decided by [`geometry::resolve_motion`] with a probe step just past the edge:
/// the very function that opens a transition, so an unlinked edge, an offline peer's gap or a
/// segment the pointer is not on never draws anything.
#[must_use]
pub fn proximity(
    topology: &Topology,
    node: &NodeId,
    position: &Position,
) -> Option<(Update, String)> {
    let display = topology.display(node, &position.display_id)?;
    let size = display.logical_size();
    let mut best: Option<(geometry::Edge, f64, geometry::Transition)> = None;
    for (edge, distance, direction) in sides(size, position.x, position.y) {
        if !(0.0..f64::from(REACH_PX)).contains(&distance) {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|(_, nearest, _)| *nearest <= distance)
        {
            continue;
        }
        let probe = Point::new(
            direction.x * (distance + 1.0),
            direction.y * (distance + 1.0),
        );
        let Some(transition) = geometry::resolve_motion(topology, node, position, probe) else {
            continue;
        };
        if transition.target.node == *node {
            continue;
        }
        best = Some((edge, distance, transition));
    }
    let (edge, distance, transition) = best?;
    let along = if edge.is_vertical() {
        position.y
    } else {
        position.x
    };
    let segment = transition.source.segment;
    Some((
        Update::Proximity {
            target: Target {
                edge: from_geometry(edge),
                start: segment.start as f32,
                end: segment.end as f32,
                along: along as f32,
            },
            distance: distance as f32,
        },
        position.display_id.as_str().to_owned(),
    ))
}

/// Where input arrived on this machine: the display edge nearest to `(x, y)` (an entry is
/// placed two pixels inside the edge it came through), the whole edge as the segment.
#[must_use]
pub fn entry(
    topology: &Topology,
    node: &NodeId,
    display_id: &DisplayId,
    x: f64,
    y: f64,
) -> Option<(Update, String)> {
    let display = topology.display(node, display_id)?;
    let size = display.logical_size();
    let (edge, _, _) = sides(size, x, y)
        .into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    let (length, along) = if edge.is_vertical() {
        (size.height, y)
    } else {
        (size.width, x)
    };
    Some((
        Update::Entered(Target {
            edge: from_geometry(edge),
            start: 0.0,
            end: length as f32,
            along: along as f32,
        }),
        display_id.as_str().to_owned(),
    ))
}

/// [`entry`] for a point in the node's logical desktop space (what an injected
/// `PointerAbsolute` carries): finds the display that holds it and works display-locally.
#[must_use]
pub fn entry_in_desktop(
    topology: &Topology,
    node: &NodeId,
    x: f64,
    y: f64,
) -> Option<(Update, String)> {
    let member = topology.node(node)?;
    let display = member
        .displays
        .iter()
        .find(|display| display.logical_bounds.contains(Point::new(x, y)))?;
    entry(
        topology,
        node,
        &display.id,
        x - display.logical_bounds.x,
        y - display.logical_bounds.y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use geometry::{
        Display, Endpoint, NodeDisplays, Rect, Rotation, Scale, Segment, TransitionLink,
    };

    fn node(n: u8) -> NodeId {
        NodeId::new([n; 32])
    }

    fn display(id: &str, bounds: Rect) -> Display {
        Display {
            id: id.into(),
            logical_bounds: bounds,
            native_bounds: Some(bounds),
            rotation: Rotation::Deg0,
            scale: Scale::ONE,
            primary: true,
        }
    }

    /// Node 1 (1200x1000) linked on its right edge, rows 120..1000, to node 2's left edge.
    fn fixture() -> Topology {
        Topology::new(
            7,
            node(1),
            vec![
                NodeDisplays::new(
                    node(1),
                    vec![display("a", Rect::new(0.0, 0.0, 1200.0, 1000.0))],
                    true,
                ),
                NodeDisplays::new(
                    node(2),
                    vec![display("b", Rect::new(0.0, 0.0, 1000.0, 800.0))],
                    true,
                ),
            ],
            vec![TransitionLink::new(
                "a-to-b",
                Endpoint::new(
                    node(1),
                    "a",
                    geometry::Edge::Right,
                    Segment::new(120.0, 1000.0),
                ),
                Endpoint::new(
                    node(2),
                    "b",
                    geometry::Edge::Left,
                    Segment::new(40.0, 760.0),
                ),
                true,
            )],
        )
    }

    fn at(x: f64, y: f64) -> Position {
        Position::new("a", x, y)
    }

    #[test]
    fn only_a_linked_edge_within_reach_draws_and_the_distance_is_to_that_edge() {
        let topology = fixture();
        let near = proximity(&topology, &node(1), &at(1150.0, 500.0)).expect("near the link");
        let Update::Proximity { target, distance } = near.0 else {
            panic!("proximity expected");
        };
        assert_eq!(near.1, "a");
        assert_eq!(target.edge, Edge::Right);
        assert_eq!(
            (target.start, target.end, target.along),
            (120.0, 1000.0, 500.0)
        );
        assert_eq!(distance, 50.0);
        // Too far, an unlinked edge, and the part of the right edge no link covers.
        assert!(proximity(&topology, &node(1), &at(600.0, 500.0)).is_none());
        assert!(proximity(&topology, &node(1), &at(500.0, 20.0)).is_none());
        assert!(proximity(&topology, &node(1), &at(1190.0, 60.0)).is_none());
    }

    #[test]
    fn the_receiver_entry_edge_is_the_nearest_one() {
        let topology = fixture();
        let (update, display) = entry(&topology, &node(2), &"b".into(), 2.0, 300.0).expect("entry");
        let Update::Entered(target) = update else {
            panic!("entered expected");
        };
        assert_eq!(display, "b");
        assert_eq!(target.edge, Edge::Left);
        assert_eq!(
            (target.start, target.end, target.along),
            (0.0, 800.0, 300.0)
        );
        let (update, _) = entry(&topology, &node(2), &"b".into(), 500.0, 798.0).expect("entry");
        assert!(matches!(
            update,
            Update::Entered(Target {
                edge: Edge::Bottom,
                ..
            })
        ));
    }

    #[test]
    fn frames_carry_the_edge_and_stay_small() {
        let target = Target {
            edge: Edge::Top,
            start: 1.0,
            end: 2.0,
            along: 1.5,
        };
        let out = frame(
            &Update::Proximity {
                target,
                distance: 9.0,
            },
            "a",
        );
        assert_eq!(out.kind, ipc::EdgeFxKind::EdgeFxProximity as i32);
        assert_eq!(out.edge, ipc::PortalEdge::Top as i32);
        assert_eq!((out.display_id.as_str(), out.distance), ("a", 9.0));
        assert_eq!(
            frame(&Update::Clear, "").kind,
            ipc::EdgeFxKind::EdgeFxClear as i32
        );
    }

    #[test]
    fn a_disabled_emitter_sends_nothing_and_remembers_nothing() {
        let mut emitter = Emitter::new(false);
        assert!(!emitter.wants_sample(0));
        assert!(emitter.push(Update::Clear, "", 0).is_none());
        assert!(emitter.out_of_reach(100).is_none());
    }

    #[test]
    fn leaving_the_reach_sends_one_left_and_only_after_an_approach() {
        let mut emitter = Emitter::new(true);
        assert!(emitter.out_of_reach(0).is_none());
        let target = Target {
            edge: Edge::Right,
            start: 0.0,
            end: 10.0,
            along: 5.0,
        };
        assert!(emitter
            .push(
                Update::Proximity {
                    target,
                    distance: 5.0
                },
                "a",
                0
            )
            .is_some());
        let left = emitter.out_of_reach(40).expect("one LEFT");
        assert_eq!(left.kind, ipc::EdgeFxKind::EdgeFxLeft as i32);
        assert!(emitter.out_of_reach(80).is_none());
    }
}

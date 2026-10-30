//! The core's half of the edge effects: what to tell the overlay (in the agent), and when.
//!
//! The core owns the geometry and the pointer samples, so it decides; the agent only draws.
//! Frames are small, rate-limited to ~30 Hz in total ([`edgefx::coalesce`]) and carry no input.
//! A *linked* edge is exactly an edge [`geometry::resolve_motion`] would turn into a
//! transition, so the effect can never appear on an edge that would not cross (and follows
//! the auto-anchor rules for free).

use edgefx::coalesce::Coalescer;
use edgefx::{Edge, Target, Update, REACH_HYSTERESIS_PX, REACH_PX};
use geometry::{DisplayId, NodeId, Point, Position, Size, Topology};
use protocol::ipc;

/// `[appearance].edge_effects` plus the rate limiter and the "was near" memory.
#[derive(Debug)]
pub struct Emitter {
    enabled: bool,
    gate: Coalescer<String>,
    near: bool,
    entry_wanted: bool,
    own: Own,
}

/// What the *receiver's* own edges saw last: the approach to the edge this machine will be
/// left through, which is a different question from the owner's approach
/// ([`Emitter::push`]) and needs its own memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    /// Input has just arrived: the entry bloom owns the overlay. The approach is armed by the
    /// first sample out of reach, so the glow cannot replace the bloom the moment it starts.
    Arrived,
    /// Out of reach of every linked edge: an approach may start.
    Away,
    /// Within reach of a linked edge: an approach is being drawn.
    Near,
}

impl Emitter {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            gate: Coalescer::default(),
            near: false,
            entry_wanted: false,
            own: Own::Away,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.near = false;
        self.gate = Coalescer::default();
        self.entry_wanted = false;
        self.own = Own::Away;
    }

    /// A receiver lease was prepared: the next pointer position is where input arrived.
    pub fn await_entry(&mut self) {
        self.entry_wanted = true;
        self.own = Own::Arrived;
    }

    /// `true` once per prepared lease, on the first `Inject` after [`Self::await_entry`].
    pub fn take_entry_wanted(&mut self) -> bool {
        std::mem::take(&mut self.entry_wanted)
    }

    pub fn cancel_entry(&mut self) {
        self.entry_wanted = false;
        self.own = Own::Away;
    }

    /// Whether a proximity sample is worth computing at `now_ms` (cheap: a comparison).
    #[must_use]
    fn wants_sample(&self, now_ms: u64) -> bool {
        self.enabled && self.gate.ready(now_ms)
    }

    /// The distance from a linked edge the next owner sample must stay within: [`REACH_PX`],
    /// or that band widened by [`REACH_HYSTERESIS_PX`] once an approach is up. A pointer
    /// resting exactly on the boundary then keeps the side it is already on instead of
    /// flipping approach↔leave, which the overlay draws as a flicker.
    #[must_use]
    fn reach(&self) -> f32 {
        if self.near {
            REACH_PX + REACH_HYSTERESIS_PX
        } else {
            REACH_PX
        }
    }

    /// [`Self::reach`] for the receiver's own edges, whose state is [`Own`], not `near`.
    #[must_use]
    fn own_reach(&self) -> f32 {
        if self.own == Own::Near {
            REACH_PX + REACH_HYSTERESIS_PX
        } else {
            REACH_PX
        }
    }

    /// One pointer sample on the owner: the frame to send, if any.
    ///
    /// The rate limit thins the pictures, never the state: while an approach is up every
    /// sample is resolved, so the `LEFT` the overlay fades on cannot be missed; while none is
    /// up the gate spares the link geometry. A frame the gate holds back is released by
    /// [`Self::due`], which the loop wakes for at [`Self::due_at`].
    pub fn sample(
        &mut self,
        topology: Option<&Topology>,
        node: &NodeId,
        position: &Position,
        now_ms: u64,
    ) -> Option<ipc::EdgeFx> {
        if !self.wants_sample(now_ms) && !self.near {
            return None;
        }
        let reach = self.reach();
        match topology.and_then(|topology| proximity(topology, node, position, reach)) {
            Some((update, display)) => self.push(update, &display, now_ms),
            None => self.out_of_reach(now_ms),
        }
    }

    /// One sample of the receiver's injected pointer: the approach to the edge this machine
    /// will be left through. [`Self::sample`] rules; dropped while the entry bloom plays
    /// (`Own::Arrived`), which is [`Self::own_near`]'s business.
    pub fn sample_own(
        &mut self,
        topology: Option<&Topology>,
        node: &NodeId,
        x: f64,
        y: f64,
        now_ms: u64,
    ) -> Option<ipc::EdgeFx> {
        if !self.wants_sample(now_ms) && self.own != Own::Near {
            return None;
        }
        let reach = self.own_reach();
        match topology.and_then(|topology| proximity_in_desktop(topology, node, x, y, reach)) {
            Some((update, display)) => self.own_near(update, &display, now_ms),
            None => self.own_away(now_ms),
        }
    }

    /// Offers a frame; the frame to send now, if the rate limit allows.
    pub fn push(&mut self, update: Update, display: &str, now_ms: u64) -> Option<ipc::EdgeFx> {
        self.near = matches!(update, Update::Proximity { .. });
        self.emit(update, display, now_ms)
    }

    /// The receiver's injected pointer is near one of *this* machine's linked edges: the exit
    /// it would leave through, which is where the core draws the gesture it never saw. Silently
    /// dropped while the entry bloom plays (`Own::Arrived`).
    pub fn own_near(&mut self, update: Update, display: &str, now_ms: u64) -> Option<ipc::EdgeFx> {
        if self.own == Own::Arrived {
            return None;
        }
        self.own = Own::Near;
        self.emit(update, display, now_ms)
    }

    /// The receiver's injected pointer is out of reach of its own linked edges: one `LEFT` if
    /// an approach was being drawn, and the approach is armed for the next one.
    pub fn own_away(&mut self, now_ms: u64) -> Option<ipc::EdgeFx> {
        let was_near = self.own == Own::Near;
        self.own = Own::Away;
        if !was_near {
            return None;
        }
        self.emit(Update::Left, "", now_ms)
    }

    /// The pointer is not near a linked edge any more: one `LEFT` if it was.
    pub fn out_of_reach(&mut self, now_ms: u64) -> Option<ipc::EdgeFx> {
        if !self.near {
            return None;
        }
        self.near = false;
        self.emit(Update::Left, "", now_ms)
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

    /// Queues one frame behind the rate limit; nothing when the effects are off.
    fn emit(&mut self, update: Update, display: &str, now_ms: u64) -> Option<ipc::EdgeFx> {
        if !self.enabled {
            return None;
        }
        self.gate
            .push(update, display.to_owned(), now_ms)
            .map(|(update, display)| frame(&update, &display))
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

/// The nearest linked edge within `reach` of `position`, as a `Proximity` update.
///
/// "Linked" is decided by [`geometry::resolve_motion`] with a probe step just past the edge:
/// the very function that opens a transition, so an unlinked edge, an offline peer's gap or a
/// segment the pointer is not on never draws anything. `reach` is the caller's: the emitter
/// widens it while an approach is up ([`Emitter::reach`]), which is what makes the band
/// hysteretic.
#[must_use]
pub fn proximity(
    topology: &Topology,
    node: &NodeId,
    position: &Position,
    reach: f32,
) -> Option<(Update, String)> {
    let display = topology.display(node, &position.display_id)?;
    let size = display.logical_size();
    let mut best: Option<(geometry::Edge, f64, geometry::Transition)> = None;
    for (edge, distance, direction) in sides(size, position.x, position.y) {
        if !(0.0..f64::from(reach)).contains(&distance) {
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
    let local = local_position(topology, node, x, y)?;
    entry(topology, node, &local.display_id, local.x, local.y)
}

/// [`proximity`] for a point in the node's logical desktop space.
///
/// This is the *receiver's* half of the effects: it injects absolute positions it did not
/// measure, so the edge it will be left through is a projection of numbers, not a pointer it
/// can watch. Only the core can do it — the agent knows the position but not the topology —
/// and the frame goes to this node's own agent, which is the overlay that draws it.
#[must_use]
pub fn proximity_in_desktop(
    topology: &Topology,
    node: &NodeId,
    x: f64,
    y: f64,
    reach: f32,
) -> Option<(Update, String)> {
    let local = local_position(topology, node, x, y)?;
    proximity(topology, node, &local, reach)
}

/// A point in the node's logical desktop space as a display-local [`Position`].
///
/// The desktop space is the convention every injected `PointerAbsolute` is in: the agent maps
/// it onto the virtual desktop (`inject_plan::VirtualDesktop`), and the display whose
/// `logical_bounds` hold it is the one it lands on.
fn local_position(topology: &Topology, node: &NodeId, x: f64, y: f64) -> Option<Position> {
    let member = topology.node(node)?;
    let display = member
        .displays
        .iter()
        .find(|display| display.logical_bounds.contains(Point::new(x, y)))?;
    Some(Position::new(
        display.id.clone(),
        x - display.logical_bounds.x,
        y - display.logical_bounds.y,
    ))
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
    fn disabling_discards_pending_frames_and_entry_intent() {
        let mut emitter = Emitter::new(true);
        assert!(emitter.push(Update::Clear, "", 0).is_some());
        assert!(emitter.push(Update::Crossed(None), "", 1).is_none());
        emitter.await_entry();
        emitter.set_enabled(false);
        assert!(emitter.due(100).is_none());
        assert!(emitter.due_at().is_none());
        assert!(!emitter.take_entry_wanted());
        emitter.set_enabled(true);
        assert!(emitter.due(200).is_none());
    }

    #[test]
    fn only_a_linked_edge_within_reach_draws_and_the_distance_is_to_that_edge() {
        let topology = fixture();
        let near =
            proximity(&topology, &node(1), &at(1150.0, 500.0), REACH_PX).expect("near the link");
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
        assert!(proximity(&topology, &node(1), &at(600.0, 500.0), REACH_PX).is_none());
        assert!(proximity(&topology, &node(1), &at(500.0, 20.0), REACH_PX).is_none());
        assert!(proximity(&topology, &node(1), &at(1190.0, 60.0), REACH_PX).is_none());
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
    fn the_receiver_nearest_edge_is_the_exit_it_would_leave_through() {
        let topology = fixture();
        // Node 2's left edge (40..760) is linked back to node 1; the injected pointer is in
        // *its* desktop space, which is what `InputBatch` absolute positions carry.
        let (update, display) = proximity_in_desktop(&topology, &node(2), 90.0, 300.0, REACH_PX)
            .expect("the return edge is within reach");
        assert_eq!(display, "b");
        let Update::Proximity { target, distance } = update else {
            panic!("proximity expected");
        };
        assert_eq!(target.edge, Edge::Left);
        assert_eq!(
            (target.start, target.end, target.along),
            (40.0, 760.0, 300.0)
        );
        assert_eq!(distance, 90.0);
        // In the middle of the desktop, and off a display: nothing to draw.
        assert!(proximity_in_desktop(&topology, &node(2), 500.0, 400.0, REACH_PX).is_none());
        assert!(proximity_in_desktop(&topology, &node(2), 4000.0, 4000.0, REACH_PX).is_none());
    }

    #[test]
    fn a_receivers_own_approach_starts_only_after_it_has_left_the_reach() {
        let mut emitter = Emitter::new(true);
        let target = Target {
            edge: Edge::Left,
            start: 0.0,
            end: 800.0,
            along: 300.0,
        };
        let near = Update::Proximity {
            target,
            distance: 90.0,
        };
        emitter.await_entry();
        // The entry bloom owns the overlay: the position it arrived at cannot start an
        // approach, and no LEFT is pending either.
        assert!(emitter.own_near(near, "b", 0).is_none());
        assert!(emitter.own_away(40).is_none());
        let frame = emitter.own_near(near, "b", 60).expect("the approach");
        assert_eq!(frame.kind, ipc::EdgeFxKind::EdgeFxProximity as i32);
        assert_eq!(frame.display_id, "b");
        assert!(emitter.own_near(near, "b", 70).is_none(), "rate limited");
        let left = emitter.own_away(100).expect("one LEFT");
        assert_eq!(left.kind, ipc::EdgeFxKind::EdgeFxLeft as i32);
        assert!(emitter.own_away(140).is_none());
        // Off while the effects are switched off, and the entry still wins afterwards.
        emitter.set_enabled(false);
        assert!(emitter.own_near(near, "b", 200).is_none());
        emitter.set_enabled(true);
        emitter.await_entry();
        assert!(emitter.own_near(near, "b", 300).is_none());
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
        let topology = fixture();
        let mut emitter = Emitter::new(false);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 0)
            .is_none());
        assert!(emitter.push(Update::Clear, "", 0).is_none());
        assert!(emitter.out_of_reach(100).is_none());
        // Switching the effects off mid-approach drops it: no LEFT, nothing held.
        emitter.set_enabled(true);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 0)
            .is_some());
        emitter.set_enabled(false);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 40)
            .is_none());
        assert!(emitter.due(80).is_none());
    }

    /// A pointer resting on the reach boundary: the approach stays up until it is
    /// [`REACH_HYSTERESIS_PX`] past it, so nothing here flips approach↔leave.
    #[test]
    fn a_pointer_on_the_boundary_keeps_the_approach_up() {
        let topology = fixture();
        let mut emitter = Emitter::new(true);
        // 1200 px wide: x = 1040 is exactly REACH_PX from the linked right edge, 1045 is
        // inside it, 1035 is outside the reach but inside the hysteresis band.
        let mut now = 0;
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1045.0, 500.0), now)
            .is_some());
        for x in [1035.0, 1045.0, 1035.0, 1045.0, 1030.0] {
            now += 40;
            let frame = emitter.sample(Some(&topology), &node(1), &at(x, 500.0), now);
            assert_eq!(
                frame.map(|frame| frame.kind),
                Some(ipc::EdgeFxKind::EdgeFxProximity as i32),
                "x = {x} ended the approach"
            );
        }
        // Past the band it is a leave, once, and the approach can start again.
        now += 40;
        let left = emitter
            .sample(Some(&topology), &node(1), &at(1000.0, 500.0), now)
            .expect("one LEFT");
        assert_eq!(left.kind, ipc::EdgeFxKind::EdgeFxLeft as i32);
        now += 40;
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1000.0, 500.0), now)
            .is_none());
    }

    /// The band is hysteretic, not a longer reach: outside REACH_PX there is nothing to draw,
    /// so a pointer that was never inside it draws nothing at all.
    #[test]
    fn the_hysteresis_band_does_not_start_an_approach() {
        let topology = fixture();
        let mut emitter = Emitter::new(true);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1035.0, 500.0), 0)
            .is_none());
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1035.0, 500.0), 40)
            .is_none());
    }

    /// The rate limit thins the pictures, never the state: a leave that arrives inside the
    /// 34 ms gap ends the approach anyway, on the frame the gate holds for it.
    #[test]
    fn a_leave_the_rate_limit_would_drop_still_ends_the_approach() {
        let topology = fixture();
        let mut emitter = Emitter::new(true);
        let approach = emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 0)
            .expect("the approach");
        assert_eq!(approach.kind, ipc::EdgeFxKind::EdgeFxProximity as i32);
        // Well out of reach 5 ms later: too soon to send, but not to notice.
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(600.0, 500.0), 5)
            .is_none());
        assert_eq!(emitter.due_at(), Some(34), "the LEFT is not held");
        let left = emitter.due(34).expect("the held LEFT");
        assert_eq!(left.kind, ipc::EdgeFxKind::EdgeFxLeft as i32);
    }

    /// A held `LEFT` outranks a later approach: the overlay hears the leave before the next
    /// approach, so it can never be left glowing with the pointer gone.
    #[test]
    fn an_approach_does_not_bury_a_held_left() {
        let topology = fixture();
        let mut emitter = Emitter::new(true);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 0)
            .is_some());
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(600.0, 500.0), 5)
            .is_none());
        // The pointer is back inside 1 ms later, while the LEAVE is still held: the leave is
        // what the overlay must hear, and the approach follows on the next sample the gate
        // lets through.
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 6)
            .is_none());
        let left = emitter.due(34).expect("the held LEFT");
        assert_eq!(left.kind, ipc::EdgeFxKind::EdgeFxLeft as i32);
        assert!(emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 40)
            .is_none());
        let approach = emitter
            .sample(Some(&topology), &node(1), &at(1150.0, 500.0), 68)
            .expect("the approach, right after the LEFT");
        assert_eq!(approach.kind, ipc::EdgeFxKind::EdgeFxProximity as i32);
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

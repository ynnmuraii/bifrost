//! Desktop-anchored (`display = "auto"`) links: resolution against the live manifests.
//!
//! The rule under test is the owner's requirement: a monitor that is switched off, added back
//! or re-arranged must change the resolved edge with no configuration edit.

use geometry::{
    boundary_displays, map_endpoints, resolve_auto_links, Display, Edge, Endpoint, NodeDisplays,
    NodeId, Rect, Rotation, Scale, Topology, TransitionLink, AUTO_DISPLAY_ID,
};

fn display(id: &str, bounds: Rect) -> Display {
    Display {
        id: id.into(),
        logical_bounds: bounds,
        native_bounds: Some(bounds),
        rotation: Rotation::Deg0,
        scale: Scale::ONE,
        primary: false,
    }
}

fn member(id: u8, displays: Vec<Display>) -> NodeDisplays {
    NodeDisplays::new(NodeId::new([id; 32]), displays, true)
}

/// One authored auto link: `local.edge` of the local desktop to `peer.edge` of the peer's.
fn auto_link(local: u8, local_edge: Edge, peer: u8, peer_edge: Edge) -> TransitionLink {
    TransitionLink::new(
        "auto",
        Endpoint::auto(NodeId::new([local; 32]), local_edge),
        Endpoint::auto(NodeId::new([peer; 32]), peer_edge),
        true,
    )
}

fn document(local: NodeDisplays, peer: NodeDisplays, links: Vec<TransitionLink>) -> Topology {
    Topology::new(
        1,
        NodeId::new([local.node.as_bytes()[0]; 32]),
        vec![local, peer],
        links,
    )
}

#[test]
fn auto_bottom_to_top_uses_only_the_outer_desktop_boundaries() {
    let local = member(
        1,
        vec![
            display("upper", Rect::new(-200.0, -600.0, 1200.0, 600.0)),
            display("lower", Rect::new(-200.0, 0.0, 1200.0, 800.0)),
        ],
    );
    let peer = member(
        2,
        vec![
            display("upper", Rect::new(100.0, -400.0, 1000.0, 400.0)),
            display("lower", Rect::new(100.0, 0.0, 1000.0, 600.0)),
        ],
    );
    for (source_edge, target_edge, source_display, target_display) in [
        (Edge::Bottom, Edge::Top, "lower", "upper"),
        (Edge::Top, Edge::Bottom, "upper", "lower"),
    ] {
        let topology = document(
            local.clone(),
            peer.clone(),
            vec![auto_link(1, source_edge, 2, target_edge)],
        );
        let resolved = resolve_auto_links(&topology);
        assert!(resolved.unresolved.is_empty());
        assert_eq!(resolved.links.len(), 1);
        let link = &resolved.links[0];
        assert_eq!(link.source.display.as_str(), source_display);
        assert_eq!(link.target.display.as_str(), target_display);
        assert_eq!(link.source.edge, source_edge);
        assert_eq!(link.target.edge, target_edge);
        assert_eq!(link.source.segment, geometry::Segment::new(0.0, 1200.0));
        assert_eq!(link.target.segment, geometry::Segment::new(0.0, 1000.0));
    }
}

/// (a) Two monitors side by side: the `right` edge is the right-most monitor only.
#[test]
fn a_right_edge_of_a_row_is_the_right_most_monitor() {
    let local = member(
        1,
        vec![
            display("left", Rect::new(0.0, 0.0, 1920.0, 1080.0)),
            display("right", Rect::new(1920.0, 0.0, 1920.0, 1080.0)),
        ],
    );
    assert_eq!(
        boundary_displays(&local, Edge::Right)
            .iter()
            .map(|display| display.id.as_str())
            .collect::<Vec<_>>(),
        vec!["right"]
    );
    assert_eq!(
        boundary_displays(&local, Edge::Left)
            .iter()
            .map(|display| display.id.as_str())
            .collect::<Vec<_>>(),
        vec!["left"]
    );

    let peer = member(
        2,
        vec![display("only", Rect::new(0.0, 0.0, 1920.0, 1080.0))],
    );
    let topology = document(local, peer, vec![auto_link(1, Edge::Right, 2, Edge::Left)]);
    let resolved = resolve_auto_links(&topology);

    assert!(resolved.unresolved.is_empty());
    assert_eq!(resolved.links.len(), 1);
    assert_eq!(resolved.links[0].source.display.as_str(), "right");
    assert_eq!(
        resolved.links[0].source.segment,
        geometry::Segment::new(0.0, 1080.0)
    );
    assert_eq!(resolved.links[0].target.display.as_str(), "only");
}

/// (b) Two monitors stacked vertically: the `right` edge is both displays.
#[test]
fn a_right_edge_of_a_stack_is_both_monitors() {
    let local = member(
        1,
        vec![
            display("top", Rect::new(0.0, 0.0, 1080.0, 1920.0)),
            display("bottom", Rect::new(0.0, 1920.0, 1080.0, 1920.0)),
        ],
    );
    assert_eq!(
        boundary_displays(&local, Edge::Right)
            .iter()
            .map(|display| display.id.as_str())
            .collect::<Vec<_>>(),
        vec!["top", "bottom"]
    );

    let peer = member(
        2,
        vec![display("only", Rect::new(0.0, 0.0, 1745.0, 1091.0))],
    );
    let topology = document(local, peer, vec![auto_link(1, Edge::Right, 2, Edge::Left)]);
    let resolved = resolve_auto_links(&topology);

    assert!(resolved.unresolved.is_empty());
    assert_eq!(resolved.links.len(), 2, "one segment per stacked monitor");
    assert_eq!(resolved.links[0].source.display.as_str(), "top");
    assert_eq!(
        resolved.links[0].source.segment,
        geometry::Segment::new(0.0, 1920.0)
    );
    assert_eq!(resolved.links[1].source.display.as_str(), "bottom");
    assert_eq!(
        resolved.links[1].source.segment,
        geometry::Segment::new(0.0, 1920.0)
    );
    // The peer's single edge is split proportionally: the two halves touch, never overlap.
    assert_eq!(
        resolved.links[0].target.segment,
        geometry::Segment::new(0.0, 545.5)
    );
    assert_eq!(
        resolved.links[1].target.segment,
        geometry::Segment::new(545.5, 1091.0)
    );
}

/// (c) One monitor switched off: the auto link resolves to what is left, with nothing inactive.
#[test]
fn a_monitor_switched_off_leaves_no_inactive_link() {
    let peer = member(
        2,
        vec![display("only", Rect::new(0.0, 0.0, 1080.0, 1920.0))],
    );
    let two = member(
        1,
        vec![
            display("left", Rect::new(0.0, 0.0, 1920.0, 1080.0)),
            display("right", Rect::new(1920.0, 0.0, 1920.0, 1080.0)),
        ],
    );
    let one = member(
        1,
        vec![display("left", Rect::new(0.0, 0.0, 1920.0, 1080.0))],
    );
    let link = auto_link(1, Edge::Right, 2, Edge::Left);

    let before = resolve_auto_links(&document(two, peer.clone(), vec![link.clone()]));
    assert_eq!(before.links.len(), 1);
    assert_eq!(before.links[0].source.display.as_str(), "right");

    let after = resolve_auto_links(&document(one, peer, vec![link]));
    assert!(
        after.unresolved.is_empty(),
        "the link is not inactive, it adapts"
    );
    assert_eq!(after.links.len(), 1);
    assert_eq!(after.links[0].source.display.as_str(), "left");
}

/// (g) Removed and added back: the edge returns to the right-most monitor.
#[test]
fn a_monitor_added_back_takes_the_edge_again() {
    let peer = member(
        2,
        vec![display("only", Rect::new(0.0, 0.0, 1080.0, 1920.0))],
    );
    let two = member(
        1,
        vec![
            display("left", Rect::new(0.0, 0.0, 1920.0, 1080.0)),
            display("right", Rect::new(1920.0, 0.0, 1920.0, 1080.0)),
        ],
    );
    let one = member(
        1,
        vec![display("left", Rect::new(0.0, 0.0, 1920.0, 1080.0))],
    );
    let link = auto_link(1, Edge::Right, 2, Edge::Left);

    let edge_of = |member: NodeDisplays| {
        let resolved = resolve_auto_links(&document(member, peer.clone(), vec![link.clone()]));
        resolved.links[0].source.display.as_str().to_owned()
    };
    assert_eq!(edge_of(two.clone()), "right");
    assert_eq!(edge_of(one), "left");
    assert_eq!(
        edge_of(two),
        "right",
        "the edge comes back with the monitor"
    );
}

/// (e) Proportional mapping: the middle of the PC's vertical edge is the middle of the
/// laptop's.
#[test]
fn a_vertical_edge_maps_proportionally_onto_the_peer() {
    let local = member(1, vec![display("pc", Rect::new(0.0, 0.0, 1080.0, 1920.0))]);
    let peer = member(
        2,
        vec![display("laptop", Rect::new(0.0, 0.0, 1745.0, 1091.0))],
    );
    let topology = document(
        local,
        peer.clone(),
        vec![auto_link(1, Edge::Right, 2, Edge::Left)],
    );
    let resolved = resolve_auto_links(&topology);

    assert_eq!(resolved.links.len(), 1);
    let link = &resolved.links[0];
    assert_eq!(link.source.segment, geometry::Segment::new(0.0, 1920.0));
    assert_eq!(link.target.segment, geometry::Segment::new(0.0, 1091.0));

    let laptop = peer
        .display(&geometry::DisplayId::new("laptop"))
        .expect("the peer display");
    let (t, position) = map_endpoints(&link.source, &link.target, laptop, 960.0);
    assert_eq!(t, 0.5);
    assert_eq!(position.display_id.as_str(), "laptop");
    assert_eq!(position.x, 2.0, "two logical units inside the left edge");
    assert_eq!(position.y, 545.5);
}

/// An auto endpoint names the desktop in the document; `AUTO_DISPLAY_ID` is the one spelling.
#[test]
fn an_auto_endpoint_names_the_desktop() {
    let endpoint = Endpoint::auto(NodeId::new([1; 32]), Edge::Bottom);
    assert!(endpoint.is_auto());
    assert_eq!(endpoint.display.as_str(), AUTO_DISPLAY_ID);
    assert!(!Endpoint::new(
        NodeId::new([1; 32]),
        "auto-left",
        Edge::Bottom,
        geometry::Segment::new(0.0, 1.0)
    )
    .is_auto());
}

/// A member with no monitor on the edge resolves to nothing, and says so.
#[test]
fn a_desktop_with_no_displays_resolves_to_nothing() {
    let local = member(1, Vec::new());
    let peer = member(
        2,
        vec![display("only", Rect::new(0.0, 0.0, 1920.0, 1080.0))],
    );
    let topology = document(local, peer, vec![auto_link(1, Edge::Right, 2, Edge::Left)]);
    let resolved = resolve_auto_links(&topology);
    assert!(resolved.links.is_empty());
    assert_eq!(resolved.unresolved, vec![geometry::LinkId::new("auto")]);
}

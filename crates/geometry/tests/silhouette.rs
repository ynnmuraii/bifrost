//! An auto edge is the desktop's *silhouette* on that side: every stretch of a display's edge
//! with no other display of the same node beyond it. The bounding box is not the silhouette:
//! a landscape monitor next to a taller portrait one has its bottom edge exposed even though
//! the portrait monitor reaches lower (owner's live test: the laptop below the PC crossed only
//! through the portrait monitor).

use geometry::{
    boundary_displays, resolve_auto_links, validate_topology, Display, Edge, Endpoint,
    NodeDisplays, NodeId, Rect, Rotation, Scale, Segment, Topology, TransitionLink,
};

fn display(id: &str, x: f64, y: f64, w: f64, h: f64) -> Display {
    let bounds = Rect::new(x, y, w, h);
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

fn document(local: NodeDisplays, peer: NodeDisplays, edge: Edge, peer_edge: Edge) -> Topology {
    Topology::new(
        1,
        NodeId::new([1; 32]),
        vec![local, peer],
        vec![TransitionLink::new(
            "auto",
            Endpoint::auto(NodeId::new([1; 32]), edge),
            Endpoint::auto(NodeId::new([2; 32]), peer_edge),
            true,
        )],
    )
}

fn peer() -> NodeDisplays {
    member(2, vec![display("lap", 0.0, 0.0, 1920.0, 1080.0)])
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// The owner's setup: a landscape primary and a 1080x1920 portrait monitor to its right.
fn landscape_and_portrait() -> NodeDisplays {
    member(
        1,
        vec![
            display("landscape", 0.0, 0.0, 1920.0, 1080.0),
            display("portrait", 1920.0, 0.0, 1080.0, 1920.0),
        ],
    )
}

#[test]
fn the_bottom_of_a_landscape_beside_a_taller_portrait_monitor_is_exposed() {
    let local = landscape_and_portrait();
    let ids: Vec<&str> = boundary_displays(&local, Edge::Bottom)
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(ids, ["landscape", "portrait"], "left to right, both");
}

#[test]
fn a_laptop_below_crosses_from_both_monitors_and_enters_proportionally() {
    let topology = document(landscape_and_portrait(), peer(), Edge::Bottom, Edge::Top);
    let resolved = resolve_auto_links(&topology);
    assert!(resolved.unresolved.is_empty());
    assert_eq!(resolved.links.len(), 2, "one link per exposed bottom edge");
    let landscape = &resolved.links[0];
    let portrait = &resolved.links[1];
    assert_eq!(landscape.source.display.as_str(), "landscape");
    assert_eq!(portrait.source.display.as_str(), "portrait");
    // The combined silhouette is 1920 + 1080 = 3000 long; the laptop's top is 1920 long.
    assert!(close(landscape.source.segment.start, 0.0));
    assert!(
        close(landscape.source.segment.end, 1920.0 - 2.0 * 0.0)
            || landscape.source.segment.end <= 1920.0
    );
    assert!(close(landscape.target.segment.start, 0.0));
    let split = 1920.0 * 1920.0 / 3000.0;
    assert!(
        (landscape.target.segment.end - split).abs() < 3.0,
        "landscape owns its share of the laptop's top: {:?}",
        landscape.target.segment
    );
    assert!((portrait.target.segment.start - split).abs() < 3.0);
    assert!(portrait.target.segment.end <= 1920.0 + 1e-6);
    validate_topology(&Topology::new(
        1,
        topology.author,
        topology.nodes.clone(),
        resolved.links,
    ))
    .expect("a valid document");
}

#[test]
fn a_monitor_hanging_over_another_loses_only_the_covered_part() {
    let local = member(
        1,
        vec![
            display("base", 0.0, 500.0, 1920.0, 1080.0),
            // Its left half sits right above `base`, its right half hangs over nothing.
            display("hanging", 960.0, 0.0, 1920.0, 500.0),
        ],
    );
    let resolved = resolve_auto_links(&document(local, peer(), Edge::Bottom, Edge::Top));
    let hanging: Vec<_> = resolved
        .links
        .iter()
        .filter(|l| l.source.display.as_str() == "hanging")
        .collect();
    assert_eq!(hanging.len(), 1);
    assert_eq!(
        hanging[0].source.segment,
        Segment::new(960.0, 1920.0),
        "only the stretch past `base`'s right edge is exposed"
    );
    assert!(resolved
        .links
        .iter()
        .any(|l| l.source.display.as_str() == "base" && close(l.source.segment.end, 1920.0)));
}

#[test]
fn a_monitor_shifted_down_exposes_only_its_uncovered_right_edge() {
    let local = member(
        1,
        vec![
            display("a", 0.0, 0.0, 1920.0, 1080.0),
            display("b", 1920.0, 300.0, 1920.0, 1080.0),
        ],
    );
    let right: Vec<&str> = boundary_displays(&local, Edge::Right)
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(
        right,
        ["a", "b"],
        "b is the right edge; a shows above b's top (nothing to its right for 0..300)"
    );
    let resolved_right =
        resolve_auto_links(&document(local.clone(), peer(), Edge::Right, Edge::Left));
    let a = resolved_right
        .links
        .iter()
        .find(|l| l.source.display.as_str() == "a")
        .expect("a has a right segment");
    assert_eq!(a.source.segment, Segment::new(0.0, 300.0));
    // Facing left, `a` is exposed over its whole height and `b` is behind `a` for 300..1080.
    let left: Vec<&str> = boundary_displays(&local, Edge::Left)
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(left, ["a", "b"], "b's lower part has nothing to its left");
    let resolved = resolve_auto_links(&document(local, peer(), Edge::Left, Edge::Right));
    let b = resolved
        .links
        .iter()
        .find(|l| l.source.display.as_str() == "b")
        .expect("b has a left segment");
    assert_eq!(b.source.segment, Segment::new(780.0, 1080.0));
}

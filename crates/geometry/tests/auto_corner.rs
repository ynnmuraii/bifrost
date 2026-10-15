//! Auto links on two adjacent sides of one desktop must not make each other ambiguous
//! (review G1): the editor writes one auto link per facing pair, and every one of them used to
//! span its whole edge, so «right» and «bottom» both reached the bottom-right corner.

use geometry::{
    resolve_auto_links, validate_topology, Display, Edge, Endpoint, NodeDisplays, NodeId, Rect,
    Rotation, Scale, Topology, TransitionLink,
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

fn member(id: u8, w: f64, h: f64) -> NodeDisplays {
    NodeDisplays::new(
        NodeId::new([id; 32]),
        vec![display("only", Rect::new(0.0, 0.0, w, h))],
        true,
    )
}

fn link(id: &str, a: u8, a_edge: Edge, b: u8, b_edge: Edge) -> TransitionLink {
    TransitionLink::new(
        id,
        Endpoint::auto(NodeId::new([a; 32]), a_edge),
        Endpoint::auto(NodeId::new([b; 32]), b_edge),
        true,
    )
}

fn topology(links: Vec<TransitionLink>) -> Topology {
    Topology::new(
        1,
        NodeId::new([1; 32]),
        vec![
            member(1, 1920.0, 1080.0),
            member(2, 1920.0, 1080.0),
            member(3, 1920.0, 1080.0),
        ],
        links,
    )
}

fn resolved(links: Vec<TransitionLink>) -> Topology {
    let source = topology(links);
    let resolution = resolve_auto_links(&source);
    assert!(resolution.unresolved.is_empty());
    Topology::new(
        source.revision,
        source.author,
        source.nodes.clone(),
        resolution.links,
    )
}

#[test]
fn auto_right_and_auto_bottom_of_one_desktop_do_not_share_the_corner() {
    let document = resolved(vec![
        link("ab", 1, Edge::Right, 2, Edge::Left),
        link("ac", 1, Edge::Bottom, 3, Edge::Top),
    ]);
    validate_topology(&document).expect("two adjacent auto links are a valid layout");
}

#[test]
fn auto_links_on_all_four_sides_of_the_middle_desktop_are_valid() {
    let document = Topology::new(
        1,
        NodeId::new([1; 32]),
        vec![
            member(1, 1920.0, 1080.0),
            member(2, 1920.0, 1080.0),
            member(3, 1920.0, 1080.0),
            member(4, 1920.0, 1080.0),
            member(5, 1920.0, 1080.0),
        ],
        vec![
            link("r", 1, Edge::Right, 2, Edge::Left),
            link("b", 1, Edge::Bottom, 3, Edge::Top),
            link("l", 1, Edge::Left, 4, Edge::Right),
            link("t", 1, Edge::Top, 5, Edge::Bottom),
        ],
    );
    let resolution = resolve_auto_links(&document);
    let flat = Topology::new(1, document.author, document.nodes.clone(), resolution.links);
    validate_topology(&flat).expect("four auto sides");
    assert_eq!(flat.links.len(), 4, "nothing is dropped, only inset");
}

#[test]
fn the_inset_is_small_and_the_corner_free_side_keeps_its_full_edge() {
    let document = resolved(vec![
        link("ab", 1, Edge::Right, 2, Edge::Left),
        link("ac", 1, Edge::Bottom, 3, Edge::Top),
    ]);
    let ab = &document.links[0];
    let ac = &document.links[1];
    // The earlier link keeps its corner; the later one gives way by a few pixels.
    assert_eq!(ab.source.segment, geometry::Segment::new(0.0, 1080.0));
    assert!(ac.source.segment.start == 0.0);
    assert!(ac.source.segment.end < 1920.0 && ac.source.segment.end > 1920.0 - 10.0);
}

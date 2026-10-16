use geometry::{
    crossing, crossing_with_motion, map_crossing, map_endpoints, resolve_motion,
    resolve_transition, validate_topology, Display, Edge, EdgeCrossing, Endpoint, LinkDirection,
    NodeDisplays, NodeId, Point, Position, Rect, Rotation, Scale, Segment, Topology,
    TransitionLink,
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

fn fixture(bidirectional: bool) -> Topology {
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
            Endpoint::new(node(1), "a", Edge::Right, Segment::new(120.0, 1000.0)),
            Endpoint::new(node(2), "b", Edge::Left, Segment::new(40.0, 760.0)),
            bidirectional,
        )],
    )
}

fn hit(edge: Edge, along: f64, overshoot: f64) -> Option<EdgeCrossing> {
    Some(EdgeCrossing {
        edge,
        along,
        overshoot,
    })
}

#[test]
fn inside_points_do_not_cross_but_points_exactly_on_each_edge_do() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    assert_eq!(crossing(&screen, &Position::new("a", 50.0, 40.0)), None);
    for (x, y, edge, along) in [
        (0.0, 30.0, Edge::Left, 30.0),
        (100.0, 30.0, Edge::Right, 30.0),
        (30.0, 0.0, Edge::Top, 30.0),
        (30.0, 80.0, Edge::Bottom, 30.0),
    ] {
        assert_eq!(
            crossing(&screen, &Position::new("a", x, y)),
            hit(edge, along, 0.0)
        );
    }
}

#[test]
fn corner_crossings_choose_largest_overshoot_then_documented_tie_order() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for (x, y, edge, along, overshoot) in [
        (110.0, 85.0, Edge::Right, 80.0, 10.0),
        (105.0, 90.0, Edge::Bottom, 100.0, 10.0),
        (-10.0, -5.0, Edge::Left, 0.0, 10.0),
        (-5.0, -10.0, Edge::Top, 0.0, 10.0),
        (110.0, 90.0, Edge::Right, 80.0, 10.0),
        (110.0, -10.0, Edge::Right, 0.0, 10.0),
        (-10.0, 90.0, Edge::Left, 80.0, 10.0),
        (-10.0, -10.0, Edge::Left, 0.0, 10.0),
        (100.0, 80.0, Edge::Right, 80.0, 0.0),
        (0.0, 0.0, Edge::Left, 0.0, 0.0),
    ] {
        assert_eq!(
            crossing(&screen, &Position::new("a", x, y)),
            hit(edge, along, overshoot),
            "({x}, {y})"
        );
    }
}

#[test]
fn crossing_clamps_along_coordinates_to_both_ends_on_both_axes() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for (x, y, edge, along) in [
        (120.0, -10.0, Edge::Right, 0.0),
        (120.0, 90.0, Edge::Right, 80.0),
        (-20.0, -10.0, Edge::Left, 0.0),
        (-20.0, 90.0, Edge::Left, 80.0),
        (-10.0, -20.0, Edge::Top, 0.0),
        (110.0, -20.0, Edge::Top, 100.0),
        (-10.0, 100.0, Edge::Bottom, 0.0),
        (110.0, 100.0, Edge::Bottom, 100.0),
    ] {
        assert_eq!(
            crossing(&screen, &Position::new("a", x, y)),
            hit(edge, along, 20.0)
        );
    }
}

#[test]
fn mismatched_position_display_cannot_cross_with_or_without_motion() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for position in [
        Position::new("other", 50.0, 40.0),
        Position::new("other", 200.0, 100.0),
    ] {
        assert_eq!(crossing(&screen, &position), None);
        assert_eq!(
            crossing_with_motion(&screen, &position, Point::new(100.0, 0.0)),
            None
        );
    }
}

#[test]
fn crossing_uses_display_local_positions_independent_of_node_origin() {
    // Position's public contract and protocol.proto both specify display-local
    // coordinates, even when logical_bounds is wholly negative in node space.
    let actual: Vec<_> = [(0.0, 0.0), (-300.0, -200.0), (300.0, 200.0)]
        .into_iter()
        .map(|(x, y)| {
            let screen = display("a", Rect::new(x, y, 100.0, 80.0));
            [
                crossing(&screen, &Position::new("a", 50.0, 40.0)),
                crossing(&screen, &Position::new("a", 100.0, 30.0)),
            ]
        })
        .collect();
    assert_eq!(actual, vec![[None, hit(Edge::Right, 30.0, 0.0)]; 3]);
}

#[test]
fn zero_motion_and_motion_staying_inside_do_not_cross() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for motion in [
        Point::new(0.0, 0.0),
        Point::new(49.0, 39.0),
        Point::new(-49.0, -39.0),
    ] {
        assert_eq!(
            crossing_with_motion(&screen, &Position::new("a", 50.0, 40.0), motion),
            None
        );
    }
}

#[test]
fn motion_starting_outside_uses_the_existing_crossing() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    let position = Position::new("a", 110.0, 30.0);
    for motion in [
        Point::new(0.0, 0.0),
        Point::new(-100.0, 0.0),
        Point::new(100.0, 100.0),
    ] {
        assert_eq!(
            crossing_with_motion(&screen, &position, motion),
            hit(Edge::Right, 30.0, 10.0)
        );
    }
}

#[test]
fn motion_hitting_an_edge_exactly_is_a_crossing_in_all_directions() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for (motion, edge, along) in [
        (Point::new(50.0, 0.0), Edge::Right, 40.0),
        (Point::new(-50.0, 0.0), Edge::Left, 40.0),
        (Point::new(0.0, 40.0), Edge::Bottom, 50.0),
        (Point::new(0.0, -40.0), Edge::Top, 50.0),
    ] {
        assert_eq!(
            crossing_with_motion(&screen, &Position::new("a", 50.0, 40.0), motion),
            hit(edge, along, 0.0)
        );
    }
}

#[test]
fn motion_hitting_a_corner_chooses_the_x_axis() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    for (motion, edge, along) in [
        (Point::new(50.0, 40.0), Edge::Right, 80.0),
        (Point::new(50.0, -40.0), Edge::Right, 0.0),
        (Point::new(-50.0, 40.0), Edge::Left, 80.0),
        (Point::new(-50.0, -40.0), Edge::Left, 0.0),
    ] {
        assert_eq!(
            crossing_with_motion(&screen, &Position::new("a", 50.0, 40.0), motion),
            hit(edge, along, 0.0)
        );
    }
}

#[test]
fn long_motion_maps_the_first_boundary_hit_instead_of_its_final_position() {
    let screen = display("a", Rect::new(0.0, 0.0, 100.0, 80.0));
    assert_eq!(
        crossing_with_motion(
            &screen,
            &Position::new("a", 50.0, 40.0),
            Point::new(100.0, 20.0)
        ),
        hit(Edge::Right, 50.0, 0.0)
    );
    assert_eq!(
        crossing_with_motion(
            &screen,
            &Position::new("a", 50.0, 40.0),
            Point::new(20.0, 80.0)
        ),
        hit(Edge::Bottom, 60.0, 0.0)
    );
    assert_eq!(
        crossing_with_motion(
            &screen,
            &Position::new("a", 50.0, 40.0),
            Point::new(100.0, 80.0)
        ),
        hit(Edge::Right, 80.0, 0.0)
    );
}

#[test]
fn motion_uses_display_local_positions_independent_of_node_origin() {
    let actual: Vec<_> = [(0.0, 0.0), (-300.0, -200.0), (300.0, 200.0)]
        .into_iter()
        .map(|(x, y)| {
            let screen = display("a", Rect::new(x, y, 100.0, 80.0));
            crossing_with_motion(
                &screen,
                &Position::new("a", 50.0, 40.0),
                Point::new(100.0, 20.0),
            )
        })
        .collect();
    assert_eq!(actual, vec![hit(Edge::Right, 50.0, 0.0); 3]);
}

#[test]
fn unequal_segments_map_proportionally_and_clamp_at_both_ends() {
    let topology = fixture(true);
    let link = &topology.links[0];
    let mut target = topology.nodes[1].displays[0].clone();
    target.logical_bounds.x = -1000.0;
    target.logical_bounds.y = -800.0;
    target.rotation = Rotation::Deg270;
    target.scale = Scale { x: 1.25, y: 2.0 };
    // 880 source units map to 720 target units, always in logical coordinates.
    for (along, t, y) in [
        (-50.0, 0.0, 40.0),
        (120.0, 0.0, 40.0),
        (340.0, 0.25, 220.0),
        (560.0, 0.5, 400.0),
        (780.0, 0.75, 580.0),
        (1000.0, 1.0, 760.0),
        (1100.0, 1.0, 760.0),
    ] {
        let expected = (t, Position::new("b", 2.0, y));
        assert_eq!(map_crossing(link, &target, along), expected);
        assert_eq!(
            map_endpoints(&link.source, &link.target, &target, along),
            expected
        );
    }
}

#[test]
fn mapping_enters_two_logical_units_inside_each_target_edge() {
    let topology = fixture(true);
    let mut link = topology.links[0].clone();
    let target = &topology.nodes[1].displays[0];
    for (edge, expected) in [
        (Edge::Left, Position::new("b", 2.0, 400.0)),
        (Edge::Right, Position::new("b", 998.0, 400.0)),
        (Edge::Top, Position::new("b", 400.0, 2.0)),
        (Edge::Bottom, Position::new("b", 400.0, 798.0)),
    ] {
        link.target.edge = edge;
        assert_eq!(map_crossing(&link, target, 560.0), (0.5, expected));
    }
}

#[test]
fn segments_ending_at_corners_preserve_the_along_edge_corner_coordinate() {
    let topology = fixture(true);
    let mut link = topology.links[0].clone();
    let target = &topology.nodes[1].displays[0];
    for (edge, length, start, end) in [
        (
            Edge::Left,
            800.0,
            Position::new("b", 2.0, 0.0),
            Position::new("b", 2.0, 800.0),
        ),
        (
            Edge::Right,
            800.0,
            Position::new("b", 998.0, 0.0),
            Position::new("b", 998.0, 800.0),
        ),
        (
            Edge::Top,
            1000.0,
            Position::new("b", 0.0, 2.0),
            Position::new("b", 1000.0, 2.0),
        ),
        (
            Edge::Bottom,
            1000.0,
            Position::new("b", 0.0, 798.0),
            Position::new("b", 1000.0, 798.0),
        ),
    ] {
        link.target.edge = edge;
        link.target.segment = Segment::new(0.0, length);
        assert_eq!(map_crossing(&link, target, 120.0), (0.0, start));
        assert_eq!(map_crossing(&link, target, 1000.0), (1.0, end));
    }
}

#[test]
fn zero_length_source_mapping_is_guarded_and_uses_target_start() {
    let topology = fixture(false);
    let mut link = topology.links[0].clone();
    link.source.segment = Segment::new(120.0, 120.0);
    for along in [0.0, 120.0, 1000.0] {
        assert_eq!(
            map_crossing(&link, &topology.nodes[1].displays[0], along),
            (0.0, Position::new("b", 2.0, 40.0))
        );
    }
}

#[test]
fn tiny_target_displays_clamp_entry_insets_without_negative_coordinates() {
    let topology = fixture(false);
    let mut link = topology.links[0].clone();
    let target = display("b", Rect::new(-10.0, -20.0, 1.0, 1.5));
    for (edge, length, expected) in [
        (Edge::Left, 1.5, Position::new("b", 1.0, 0.75)),
        (Edge::Right, 1.5, Position::new("b", 0.0, 0.75)),
        (Edge::Top, 1.0, Position::new("b", 0.5, 1.5)),
        (Edge::Bottom, 1.0, Position::new("b", 0.5, 0.0)),
    ] {
        link.target.edge = edge;
        link.target.segment = Segment::new(0.0, length);
        assert_eq!(map_crossing(&link, &target, 560.0), (0.5, expected));
    }
}

#[test]
fn mapping_clamps_out_of_range_target_segments_to_the_target_edge() {
    let topology = fixture(false);
    let mut link = topology.links[0].clone();
    let target = &topology.nodes[1].displays[0];
    for (edge, length, start, end) in [
        (
            Edge::Left,
            800.0,
            Position::new("b", 2.0, 0.0),
            Position::new("b", 2.0, 800.0),
        ),
        (
            Edge::Top,
            1000.0,
            Position::new("b", 0.0, 2.0),
            Position::new("b", 1000.0, 2.0),
        ),
    ] {
        link.target.edge = edge;
        link.target.segment = Segment::new(-10.0, length + 10.0);
        assert_eq!(map_crossing(&link, target, 120.0), (0.0, start));
        assert_eq!(map_crossing(&link, target, 1000.0), (1.0, end));
    }
}

#[test]
fn forward_then_reverse_mapping_preserves_along_coordinate_for_every_edge_pair() {
    let topology = fixture(true);
    let source = &topology.nodes[0].displays[0];
    let target = &topology.nodes[1].displays[0];
    let mut link = topology.links[0].clone();
    for source_edge in Edge::ALL {
        link.source.edge = source_edge;
        for target_edge in Edge::ALL {
            link.target.edge = target_edge;
            // Eighths of the 880-unit segment have exactly representable images
            // on the 720-unit segment. Only the along axis should round-trip.
            for along in [
                120.0, 230.0, 340.0, 450.0, 560.0, 670.0, 780.0, 890.0, 1000.0,
            ] {
                let (_, forward) = map_endpoints(&link.source, &link.target, target, along);
                let target_along = match target_edge {
                    Edge::Left | Edge::Right => forward.y,
                    Edge::Top | Edge::Bottom => forward.x,
                };
                let (_, reverse) = map_endpoints(&link.target, &link.source, source, target_along);
                let restored = match source_edge {
                    Edge::Left | Edge::Right => reverse.y,
                    Edge::Top | Edge::Bottom => reverse.x,
                };
                assert_eq!(restored, along, "{source_edge:?} -> {target_edge:?}");
            }
        }
    }
}

#[test]
fn bidirectional_link_resolves_forward_with_authored_endpoints() {
    let topology = fixture(true);
    assert_eq!(validate_topology(&topology), Ok(()));
    let transition =
        resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)).unwrap();
    assert_eq!(transition.link, 0);
    assert_eq!(transition.direction, LinkDirection::Forward);
    assert_eq!(transition.source, topology.links[0].source);
    assert_eq!(transition.target, topology.links[0].target);
    assert_eq!(transition.t, 0.5);
    assert_eq!(transition.target_position, Position::new("b", 2.0, 400.0));
    assert_eq!(
        resolve_motion(
            &topology,
            &node(1),
            &Position::new("a", 1100.0, 560.0),
            Point::new(200.0, 0.0)
        ),
        Some(transition)
    );
}

#[test]
fn bottom_edge_transition_maps_to_the_targets_top_edge() {
    let mut topology = fixture(true);
    topology.links[0].source =
        Endpoint::new(node(1), "a", Edge::Bottom, Segment::new(200.0, 1000.0));
    topology.links[0].target = Endpoint::new(node(2), "b", Edge::Top, Segment::new(100.0, 900.0));
    assert_eq!(validate_topology(&topology), Ok(()));
    let transition = resolve_transition(&topology, &node(1), &Position::new("a", 600.0, 1000.0))
        .expect("the bottom edge joins the peer's top edge");
    assert_eq!(transition.direction, LinkDirection::Forward);
    assert_eq!(transition.source.edge, Edge::Bottom);
    assert_eq!(transition.target.edge, Edge::Top);
    assert_eq!(transition.t, 0.5);
    assert_eq!(transition.target_position, Position::new("b", 500.0, 2.0));
    assert_eq!(
        resolve_motion(
            &topology,
            &node(1),
            &Position::new("a", 600.0, 990.0),
            Point::new(0.0, 20.0),
        ),
        Some(transition),
    );
    let reverse = resolve_transition(&topology, &node(2), &Position::new("b", 500.0, 0.0))
        .expect("the link remains bidirectional");
    assert_eq!(reverse.direction, LinkDirection::Reverse);
    assert_eq!(reverse.target_position, Position::new("a", 600.0, 998.0));
}

#[test]
fn concrete_contact_segments_only_cross_within_overlap_on_both_axes_and_directions() {
    for horizontal in [false, true] {
        let mut topology = fixture(true);
        let (source_edge, target_edge) = if horizontal {
            (Edge::Bottom, Edge::Top)
        } else {
            (Edge::Right, Edge::Left)
        };
        topology.links[0].source.edge = source_edge;
        topology.links[0].source.segment = Segment::new(200.0, 600.0);
        topology.links[0].target.edge = target_edge;
        topology.links[0].target.segment = Segment::new(100.0, 500.0);
        assert_eq!(validate_topology(&topology), Ok(()));

        for reverse in [false, true] {
            let (owner, id, start, end, normal, boundary, target_id, target_normal) = if reverse {
                (
                    node(2),
                    "b",
                    100.0,
                    500.0,
                    -1.0,
                    0.0,
                    "a",
                    if horizontal { 998.0 } else { 1198.0 },
                )
            } else {
                (
                    node(1),
                    "a",
                    200.0,
                    600.0,
                    1.0,
                    if horizontal { 1000.0 } else { 1200.0 },
                    "b",
                    2.0,
                )
            };
            for along in [start - 1.0, start, (start + end) / 2.0, end, end + 1.0] {
                let position = if horizontal {
                    Position::new(id, along, boundary)
                } else {
                    Position::new(id, boundary, along)
                };
                let delta = if horizontal {
                    Point::new(0.0, normal * 20.0)
                } else {
                    Point::new(normal * 20.0, 0.0)
                };
                let inside =
                    Position::new(id, position.x - delta.x / 2.0, position.y - delta.y / 2.0);
                let resolved = resolve_transition(&topology, &owner, &position);
                assert_eq!(resolve_motion(&topology, &owner, &inside, delta), resolved);
                if along < start || along > end {
                    assert!(resolved.is_none(), "{horizontal} {reverse} {along}");
                    continue;
                }
                let transition = resolved.expect("the concrete contact is traversable");
                let mapped = along + if reverse { 100.0 } else { -100.0 };
                let expected = if horizontal {
                    Position::new(target_id, mapped, target_normal)
                } else {
                    Position::new(target_id, target_normal, mapped)
                };
                assert_eq!(transition.target_position, expected);
                assert_eq!(
                    transition.direction,
                    if reverse {
                        LinkDirection::Reverse
                    } else {
                        LinkDirection::Forward
                    }
                );
            }
        }
    }
}

#[test]
fn bidirectional_link_resolves_reverse_with_swapped_endpoints() {
    let topology = fixture(true);
    let transition =
        resolve_transition(&topology, &node(2), &Position::new("b", 0.0, 400.0)).unwrap();
    assert_eq!(transition.link, 0);
    assert_eq!(transition.direction, LinkDirection::Reverse);
    assert_eq!(transition.source, topology.links[0].target);
    assert_eq!(transition.target, topology.links[0].source);
    assert_eq!(transition.t, 0.5);
    assert_eq!(
        transition.target_position,
        Position::new("a", 1198.0, 560.0)
    );
    assert_eq!(
        resolve_motion(
            &topology,
            &node(2),
            &Position::new("b", 100.0, 400.0),
            Point::new(-200.0, 0.0)
        ),
        Some(transition)
    );
}

#[test]
fn one_way_link_requires_an_explicit_reversed_link_to_exit_its_target() {
    let mut topology = fixture(false);
    let position = Position::new("b", 0.0, 400.0);
    assert_eq!(resolve_transition(&topology, &node(2), &position), None);
    assert_eq!(
        resolve_motion(
            &topology,
            &node(2),
            &Position::new("b", 100.0, 400.0),
            Point::new(-200.0, 0.0)
        ),
        None
    );
    assert!(resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)).is_some());

    // reversed() expands the wire direction, preserving the authored id; it is
    // not an additional authored record with a duplicate document key.
    let reversed = topology.links[0].reversed();
    assert_eq!(reversed.id.as_str(), "a-to-b");
    assert!(!reversed.bidirectional);
    topology.links = vec![reversed];
    assert_eq!(validate_topology(&topology), Ok(()));
    let transition = resolve_transition(&topology, &node(2), &position).unwrap();
    assert_eq!(transition.direction, LinkDirection::Forward);
    assert_eq!(transition.source, topology.links[0].source);
    assert_eq!(transition.target, topology.links[0].target);
    assert_eq!(
        transition.target_position,
        Position::new("a", 1198.0, 560.0)
    );
    assert_eq!(
        resolve_motion(
            &topology,
            &node(2),
            &Position::new("b", 100.0, 400.0),
            Point::new(-200.0, 0.0)
        ),
        Some(transition)
    );
    assert_eq!(
        resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)),
        None
    );
}

#[test]
fn resolvers_choose_document_order_at_touching_segment_boundaries() {
    for reversed_first in [false, true] {
        let mut topology = fixture(true);
        topology.links[0].source.segment.end = 560.0;
        topology.links[0].target.segment.end = 400.0;
        topology.links.push(TransitionLink::new(
            "second",
            Endpoint::new(node(1), "a", Edge::Right, Segment::new(560.0, 1000.0)),
            Endpoint::new(node(2), "b", Edge::Left, Segment::new(10.0, 30.0)),
            false,
        ));
        if reversed_first {
            topology.links[0] = topology.links[0].reversed();
        }
        assert_eq!(validate_topology(&topology), Ok(()));
        for swap_order in [false, true] {
            if swap_order {
                topology.links.swap(0, 1);
            }
            let transition =
                resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0))
                    .unwrap();
            assert_eq!(transition.link, 0);
            assert_eq!(
                transition.direction,
                if reversed_first && !swap_order {
                    LinkDirection::Reverse
                } else {
                    LinkDirection::Forward
                }
            );
            assert_eq!(transition.t, if swap_order { 0.0 } else { 1.0 });
            assert_eq!(
                transition.target_position,
                Position::new("b", 2.0, if swap_order { 10.0 } else { 400.0 })
            );
            assert_eq!(
                resolve_motion(
                    &topology,
                    &node(1),
                    &Position::new("a", 1100.0, 560.0),
                    Point::new(100.0, 0.0)
                ),
                Some(transition)
            );
        }
    }
}

#[test]
fn uncovered_edges_and_uncovered_parts_of_a_linked_edge_stay_local() {
    let topology = fixture(true);
    for (outside, inside, motion) in [
        (
            Position::new("a", 0.0, 560.0),
            Position::new("a", 100.0, 560.0),
            Point::new(-100.0, 0.0),
        ),
        (
            Position::new("a", 500.0, 0.0),
            Position::new("a", 500.0, 100.0),
            Point::new(0.0, -100.0),
        ),
        (
            Position::new("a", 1200.0, 100.0),
            Position::new("a", 1100.0, 100.0),
            Point::new(100.0, 0.0),
        ),
    ] {
        assert_eq!(resolve_transition(&topology, &node(1), &outside), None);
        assert_eq!(resolve_motion(&topology, &node(1), &inside, motion), None);
    }
    assert_eq!(
        resolve_motion(
            &topology,
            &node(1),
            &Position::new("a", 500.0, 500.0),
            Point::new(0.0, 0.0)
        ),
        None
    );
    assert_eq!(
        resolve_motion(
            &topology,
            &node(1),
            &Position::new("a", 500.0, 500.0),
            Point::new(10.0, 10.0)
        ),
        None
    );
}

#[test]
fn missing_members_or_unplugged_displays_do_not_resolve() {
    let topology = fixture(true);
    for (owner, position) in [
        (node(99), Position::new("a", 1200.0, 560.0)),
        (node(1), Position::new("unplugged", 1200.0, 560.0)),
    ] {
        assert_eq!(resolve_transition(&topology, &owner, &position), None);
        assert_eq!(
            resolve_motion(&topology, &owner, &position, Point::new(100.0, 0.0)),
            None
        );
    }
    for remove_member in [false, true] {
        let mut topology = fixture(true);
        if remove_member {
            topology.nodes.pop();
        } else {
            topology.nodes[1].displays.clear();
        }
        assert_eq!(
            resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)),
            None
        );
        assert_eq!(
            resolve_motion(
                &topology,
                &node(1),
                &Position::new("a", 1100.0, 560.0),
                Point::new(100.0, 0.0)
            ),
            None
        );
    }
}

#[test]
fn advisory_offline_flag_does_not_change_pure_geometry_resolution() {
    let mut topology = fixture(true);
    for member in &mut topology.nodes {
        member.online = false;
    }
    let transition =
        resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)).unwrap();
    assert_eq!(transition.target_position, Position::new("b", 2.0, 400.0));
}

#[test]
fn target_native_is_explicit_scaled_and_requires_matching_known_display() {
    let mut topology = fixture(true);
    topology.nodes[0].displays[0].scale = Scale::uniform(2.0);
    topology.nodes[1].displays[0].scale = Scale { x: 1.5, y: 1.25 };
    let transition =
        resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)).unwrap();
    assert_eq!(transition.target_position, Position::new("b", 2.0, 400.0));
    let mut target = topology.nodes[1].displays[0].clone();
    assert_eq!(
        transition.target_native(&target),
        Some(Position::new("b", 3.0, 500.0))
    );
    target.id = "wrong".into();
    assert_eq!(transition.target_native(&target), None);
    assert_eq!(
        transition.target_native(&topology.nodes[0].displays[0]),
        None
    );
    target.id = "b".into();
    target.native_bounds = None;
    assert_eq!(transition.target_native(&target), None);
    topology.nodes[1].displays[0] = target;
    let logical_only =
        resolve_transition(&topology, &node(1), &Position::new("a", 1200.0, 560.0)).unwrap();
    assert_eq!(logical_only.target_position, Position::new("b", 2.0, 400.0));
}

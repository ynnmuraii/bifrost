use geometry::{
    validate_topology, Corner, Display, DisplayDefect, DisplayRef, Edge, EdgeRef, Endpoint,
    EndpointSide, NodeDisplays, NodeId, Rect, Rotation, Scale, Segment, Topology, TopologyError,
    TransitionLink,
};

fn node(n: u8) -> NodeId {
    NodeId::new([n; 32])
}

fn display(bounds: Rect) -> Display {
    Display {
        id: "screen".into(),
        logical_bounds: bounds,
        native_bounds: None,
        rotation: Rotation::Deg0,
        scale: Scale::ONE,
        primary: true,
    }
}

fn endpoint(n: u8, edge: Edge, start: f64, end: f64) -> Endpoint {
    Endpoint::new(node(n), "screen", edge, Segment::new(start, end))
}

fn fixture() -> Topology {
    Topology::new(
        7,
        node(1),
        (1..=3)
            .map(|n| {
                NodeDisplays::new(
                    node(n),
                    vec![display(Rect::new(0.0, 0.0, 100.0, 100.0))],
                    true,
                )
            })
            .collect(),
        vec![TransitionLink::new(
            "first",
            endpoint(1, Edge::Right, 10.0, 90.0),
            endpoint(2, Edge::Left, 10.0, 90.0),
            false,
        )],
    )
}

fn endpoint_mut(topology: &mut Topology, side: EndpointSide) -> &mut Endpoint {
    match side {
        EndpointSide::Source => &mut topology.links[0].source,
        EndpointSide::Target => &mut topology.links[0].target,
    }
}

fn errors(topology: &Topology) -> Vec<TopologyError> {
    validate_topology(topology)
        .expect_err("fixture must be rejected")
        .into_errors()
}

fn assert_defect(screen: Display, defect: DisplayDefect) {
    let topology = Topology::new(
        7,
        node(1),
        vec![NodeDisplays::new(node(1), vec![screen], true)],
        vec![],
    );
    assert_eq!(
        errors(&topology),
        vec![TopologyError::InvalidDisplay {
            node: node(1),
            display: "screen".into(),
            defect,
        }]
    );
}

#[test]
fn nonpositive_logical_dimensions_report_nonpositive_size() {
    for bounds in [
        Rect::new(0.0, 0.0, 0.0, 100.0),
        Rect::new(0.0, 0.0, -1.0, 100.0),
        Rect::new(0.0, 0.0, 100.0, 0.0),
        Rect::new(0.0, 0.0, 100.0, -1.0),
    ] {
        assert_defect(display(bounds), DisplayDefect::NonPositiveSize);
    }
}

#[test]
fn nonfinite_logical_and_native_bounds_report_nonfinite_bounds() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for bounds in [
            Rect::new(value, 0.0, 100.0, 100.0),
            Rect::new(0.0, value, 100.0, 100.0),
            Rect::new(0.0, 0.0, value, 100.0),
            Rect::new(0.0, 0.0, 100.0, value),
        ] {
            assert_defect(display(bounds), DisplayDefect::NonFiniteBounds);
            let mut screen = display(Rect::new(0.0, 0.0, 100.0, 100.0));
            screen.native_bounds = Some(bounds);
            assert_defect(screen, DisplayDefect::NonFiniteBounds);
        }
    }
}

#[test]
fn nonpositive_and_nonfinite_scale_on_either_axis_reports_invalid_scale() {
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for scale in [Scale { x: value, y: 1.0 }, Scale { x: 1.0, y: value }] {
            let mut screen = display(Rect::new(0.0, 0.0, 100.0, 100.0));
            screen.scale = scale;
            assert_defect(screen, DisplayDefect::InvalidScale);
        }
    }
}

#[test]
fn invalid_segments_are_reported_on_both_endpoints_of_every_link_kind() {
    for bidirectional in [false, true] {
        for side in [EndpointSide::Source, EndpointSide::Target] {
            for segment in [
                Segment::new(-1.0, 20.0),
                Segment::new(20.0, 20.0),
                Segment::new(30.0, 20.0),
                Segment::new(10.0, 101.0),
                Segment::new(f64::NAN, 20.0),
                Segment::new(10.0, f64::NAN),
                Segment::new(f64::NEG_INFINITY, 20.0),
                Segment::new(10.0, f64::NEG_INFINITY),
                Segment::new(10.0, f64::INFINITY),
            ] {
                let mut topology = fixture();
                topology.links[0].bidirectional = bidirectional;
                endpoint_mut(&mut topology, side).segment = segment;
                let found = errors(&topology);
                // NaN cannot be compared via PartialEq, so check its payload explicitly.
                assert_eq!(found.len(), 1, "{found:?}");
                let TopologyError::InvalidSegment {
                    link,
                    side: actual_side,
                    segment: actual,
                    edge_length,
                } = &found[0]
                else {
                    panic!("expected InvalidSegment, got {found:?}");
                };
                assert_eq!((*link, *actual_side, *edge_length), (0, side, 100.0));
                assert_eq!(actual.start.to_bits(), segment.start.to_bits());
                assert_eq!(actual.end.to_bits(), segment.end.to_bits());
            }
        }
    }
}

#[test]
fn full_edge_segments_and_unknown_native_bounds_are_valid() {
    let mut topology = fixture();
    topology.links[0].source.segment = Segment::new(0.0, 100.0);
    topology.links[0].target.segment = Segment::new(0.0, 100.0);
    for bidirectional in [false, true] {
        topology.links[0].bidirectional = bidirectional;
        assert_eq!(validate_topology(&topology), Ok(()));
    }
}

#[test]
fn shipped_config_full_edge_chain_is_valid() {
    let mut topology = fixture();
    topology.nodes[0].displays[0].logical_bounds = Rect::new(0.0, 0.0, 2560.0, 1440.0);
    for member in &mut topology.nodes[1..] {
        member.displays[0].logical_bounds = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    }
    topology.links = vec![
        TransitionLink::new(
            "win-right-to-a-left",
            endpoint(1, Edge::Right, 200.0, 1240.0),
            endpoint(2, Edge::Left, 0.0, 1080.0),
            true,
        ),
        TransitionLink::new(
            "a-right-to-b-left",
            endpoint(2, Edge::Right, 0.0, 1080.0),
            endpoint(3, Edge::Left, 0.0, 1080.0),
            true,
        ),
    ];
    assert_eq!(validate_topology(&topology), Ok(()));
}

#[test]
fn segments_are_closed_but_touching_segments_do_not_overlap() {
    let first = Segment::new(10.0, 40.0);
    let second = Segment::new(40.0, 70.0);
    assert!(first.contains(10.0));
    assert!(first.contains(40.0));
    assert!(!first.contains(9.0));
    assert!(!first.contains(41.0));
    assert_eq!(first.overlap(&second), None);
    assert_eq!(second.overlap(&first), None);
    assert_eq!(
        first.overlap(&Segment::new(30.0, 60.0)),
        Some(Segment::new(30.0, 40.0))
    );
}

#[test]
fn unknown_node_and_unknown_display_are_distinguished_on_both_sides() {
    for bidirectional in [false, true] {
        for side in [EndpointSide::Source, EndpointSide::Target] {
            let mut topology = fixture();
            topology.links[0].bidirectional = bidirectional;
            endpoint_mut(&mut topology, side).node = node(99);
            assert_eq!(
                errors(&topology),
                vec![TopologyError::UnknownNode {
                    link: 0,
                    side,
                    node: node(99)
                }]
            );

            let mut topology = fixture();
            topology.links[0].bidirectional = bidirectional;
            let endpoint = endpoint_mut(&mut topology, side);
            let owner = endpoint.node;
            endpoint.display = "unplugged".into();
            assert_eq!(
                errors(&topology),
                vec![TopologyError::UnknownDisplay {
                    link: 0,
                    side,
                    node: owner,
                    display: "unplugged".into(),
                }]
            );
        }
    }
}

#[test]
fn same_node_link_is_rejected_even_between_distinct_displays() {
    let mut topology = fixture();
    let mut sibling = display(Rect::new(-200.0, 0.0, 100.0, 100.0));
    sibling.id = "sibling".into();
    topology.nodes[0].displays.push(sibling);
    topology.links[0].target.node = node(1);
    topology.links[0].target.display = "sibling".into();
    assert_eq!(
        errors(&topology),
        vec![TopologyError::SameNode {
            link: 0,
            node: node(1)
        }]
    );
}

#[test]
fn duplicate_nodes_and_member_local_display_ids_are_rejected() {
    let mut topology = fixture();
    topology.links.clear();
    topology.nodes.push(topology.nodes[0].clone());
    assert_eq!(
        errors(&topology),
        vec![TopologyError::DuplicateNode { node: node(1) }]
    );

    let mut topology = fixture();
    topology.links.clear();
    // Reusing "screen" across different nodes is valid; within one node it is not.
    assert_eq!(validate_topology(&topology), Ok(()));
    let mut duplicate = topology.nodes[0].displays[0].clone();
    duplicate.logical_bounds.x = 200.0;
    topology.nodes[0].displays.push(duplicate);
    assert_eq!(
        errors(&topology),
        vec![TopologyError::DuplicateDisplay {
            node: node(1),
            display: "screen".into()
        }]
    );
}

#[test]
fn duplicate_link_ids_report_original_and_repeated_document_indices() {
    let mut topology = fixture();
    topology.links[0].source.segment = Segment::new(0.0, 20.0);
    topology.links[0].target.segment = Segment::new(0.0, 20.0);
    for (id, start, end) in [
        ("different", 20.0, 40.0),
        ("first", 40.0, 60.0),
        ("first", 60.0, 80.0),
    ] {
        topology.links.push(TransitionLink::new(
            id,
            endpoint(1, Edge::Right, start, end),
            endpoint(2, Edge::Left, start, end),
            false,
        ));
    }
    assert_eq!(
        errors(&topology),
        vec![
            TopologyError::DuplicateLink {
                id: "first".into(),
                first_link: 0,
                second_link: 2
            },
            TopologyError::DuplicateLink {
                id: "first".into(),
                first_link: 0,
                second_link: 3
            },
        ]
    );
}

#[test]
fn same_link_source_and_target_overlap_is_reported_alongside_same_node() {
    let mut topology = fixture();
    topology.links[0].target = endpoint(1, Edge::Right, 30.0, 100.0);
    assert_eq!(
        errors(&topology),
        vec![
            TopologyError::SameNode {
                link: 0,
                node: node(1)
            },
            TopologyError::SegmentOverlap {
                edge: EdgeRef {
                    node: node(1),
                    display: "screen".into(),
                    edge: Edge::Right
                },
                first_link: 0,
                second_link: 0,
                overlap: Segment::new(30.0, 90.0),
            },
        ]
    );
}

// Put the shared edge at either end of each authored link. Remote endpoints use
// different nodes so only the shared edge can cause a conflict.
fn two_links(
    first: Endpoint,
    second: Endpoint,
    first_target: bool,
    second_target: bool,
    bidirectional: bool,
) -> Topology {
    let mut topology = fixture();
    topology.links = vec![
        TransitionLink::new(
            "first",
            first,
            endpoint(2, Edge::Left, 10.0, 90.0),
            bidirectional,
        ),
        TransitionLink::new(
            "second",
            second,
            endpoint(3, Edge::Left, 10.0, 90.0),
            bidirectional,
        ),
    ];
    if first_target {
        topology.links[0] = topology.links[0].reversed();
    }
    if second_target {
        topology.links[1] = topology.links[1].reversed();
    }
    topology
}

#[test]
fn overlapping_sources_targets_and_mixed_endpoints_are_rejected() {
    for bidirectional in [false, true] {
        for first_target in [false, true] {
            for second_target in [false, true] {
                let topology = two_links(
                    endpoint(1, Edge::Right, 10.0, 60.0),
                    endpoint(1, Edge::Right, 40.0, 90.0),
                    first_target,
                    second_target,
                    bidirectional,
                );
                assert_eq!(
                    errors(&topology),
                    vec![TopologyError::SegmentOverlap {
                        edge: EdgeRef {
                            node: node(1),
                            display: "screen".into(),
                            edge: Edge::Right
                        },
                        first_link: 0,
                        second_link: 1,
                        overlap: Segment::new(40.0, 60.0),
                    }]
                );
            }
        }
    }
}

#[test]
fn merely_touching_exit_segments_are_valid_for_all_endpoint_roles() {
    for first_target in [false, true] {
        for second_target in [false, true] {
            let topology = two_links(
                endpoint(1, Edge::Right, 10.0, 50.0),
                endpoint(1, Edge::Right, 50.0, 90.0),
                first_target,
                second_target,
                true,
            );
            assert_eq!(validate_topology(&topology), Ok(()));
        }
    }
}

fn sibling_topology(edge: Edge, side: EndpointSide, gap: f64, bidirectional: bool) -> Topology {
    let mut topology = fixture();
    topology.links[0].bidirectional = bidirectional;
    let endpoint = endpoint_mut(&mut topology, side);
    endpoint.edge = edge;
    endpoint.segment = Segment::new(40.0, 90.0);
    let owner = endpoint.node;
    let member = topology
        .nodes
        .iter_mut()
        .find(|member| member.node == owner)
        .unwrap();
    member.displays[0].logical_bounds = Rect::new(-200.0, -150.0, 100.0, 100.0);
    let bounds = match edge {
        Edge::Right => Rect::new(-100.0 + gap, -125.0, 40.0, 50.0),
        Edge::Left => Rect::new(-240.0 - gap, -125.0, 40.0, 50.0),
        Edge::Bottom => Rect::new(-175.0, -50.0 + gap, 50.0, 40.0),
        Edge::Top => Rect::new(-175.0, -190.0 - gap, 50.0, 40.0),
    };
    let mut sibling = display(bounds);
    sibling.id = "sibling".into();
    member.displays.push(sibling);
    topology
}

#[test]
fn internal_edges_report_the_covered_subsegment_on_both_endpoint_sides() {
    for bidirectional in [false, true] {
        for side in [EndpointSide::Source, EndpointSide::Target] {
            for edge in Edge::ALL {
                let topology = sibling_topology(edge, side, 0.0, bidirectional);
                assert_eq!(
                    errors(&topology),
                    vec![TopologyError::SegmentNotOuter {
                        link: 0,
                        side,
                        covered: Segment::new(40.0, 75.0),
                    }],
                    "{edge:?} {side:?}"
                );
            }
        }
    }
}

#[test]
fn positive_gaps_facing_a_sibling_are_holes_on_both_endpoint_sides() {
    for bidirectional in [false, true] {
        for side in [EndpointSide::Source, EndpointSide::Target] {
            for edge in Edge::ALL {
                let topology = sibling_topology(edge, side, 15.0, bidirectional);
                assert_eq!(
                    errors(&topology),
                    vec![TopologyError::SegmentInHole { link: 0, side }],
                    "{edge:?} {side:?}"
                );
            }
        }
    }
}

#[test]
fn diagonal_sibling_at_a_corner_does_not_cover_an_edge_or_form_a_hole() {
    for gap in [0.0, 15.0] {
        let mut topology = fixture();
        topology.links[0].source.segment = Segment::new(0.0, 100.0);
        let mut sibling = display(Rect::new(100.0 + gap, 100.0, 50.0, 50.0));
        sibling.id = "diagonal".into();
        topology.nodes[0].displays.push(sibling);
        assert_eq!(validate_topology(&topology), Ok(()));
    }
}

#[test]
fn uncovered_part_of_an_edge_remains_a_valid_exit() {
    for gap in [0.0, 15.0] {
        for side in [EndpointSide::Source, EndpointSide::Target] {
            for edge in Edge::ALL {
                let mut topology = sibling_topology(edge, side, gap, false);
                endpoint_mut(&mut topology, side).segment = Segment::new(75.0, 100.0);
                assert_eq!(validate_topology(&topology), Ok(()));
            }
        }
    }
}

#[test]
fn one_segment_reports_both_internal_coverage_and_a_hole() {
    for side in [EndpointSide::Source, EndpointSide::Target] {
        let mut topology = fixture();
        let endpoint = endpoint_mut(&mut topology, side);
        endpoint.edge = Edge::Right;
        let owner = endpoint.node;
        let member = topology
            .nodes
            .iter_mut()
            .find(|member| member.node == owner)
            .unwrap();
        for (id, bounds) in [
            ("adjacent", Rect::new(100.0, 0.0, 50.0, 35.0)),
            ("beyond-gap", Rect::new(120.0, 60.0, 50.0, 40.0)),
        ] {
            let mut sibling = display(bounds);
            sibling.id = id.into();
            member.displays.push(sibling);
        }
        assert_eq!(
            errors(&topology),
            vec![
                TopologyError::SegmentNotOuter {
                    link: 0,
                    side,
                    covered: Segment::new(10.0, 35.0)
                },
                TopologyError::SegmentInHole { link: 0, side },
            ]
        );
    }
}

fn corner_error(corner: Corner, edges: (Edge, Edge)) -> TopologyError {
    TopologyError::AmbiguousCorner {
        display: DisplayRef {
            node: node(1),
            display: "screen".into(),
        },
        corner,
        edges,
    }
}

#[test]
fn full_right_and_bottom_edges_are_ambiguous_in_all_endpoint_roles() {
    for bidirectional in [false, true] {
        for first_target in [false, true] {
            for second_target in [false, true] {
                let topology = two_links(
                    endpoint(1, Edge::Right, 0.0, 100.0),
                    endpoint(1, Edge::Bottom, 0.0, 100.0),
                    first_target,
                    second_target,
                    bidirectional,
                );
                assert_eq!(
                    errors(&topology),
                    vec![corner_error(
                        Corner::BottomRight,
                        (Edge::Right, Edge::Bottom)
                    )]
                );
            }
        }
    }
}

#[test]
fn opposite_edges_and_segments_missing_the_shared_corner_are_valid() {
    for (first, second) in [
        (
            endpoint(1, Edge::Right, 0.0, 100.0),
            endpoint(1, Edge::Left, 0.0, 100.0),
        ),
        (
            endpoint(1, Edge::Top, 0.0, 100.0),
            endpoint(1, Edge::Bottom, 0.0, 100.0),
        ),
        (
            endpoint(1, Edge::Right, 0.0, 80.0),
            endpoint(1, Edge::Bottom, 0.0, 100.0),
        ),
        (
            endpoint(1, Edge::Right, 0.0, 100.0),
            endpoint(1, Edge::Bottom, 0.0, 80.0),
        ),
    ] {
        assert_eq!(
            validate_topology(&two_links(first, second, false, true, false)),
            Ok(())
        );
    }
}

#[test]
fn short_segments_meeting_at_each_corner_are_ambiguous() {
    // Vertical edges start at the TOP; horizontal edges start at the LEFT.
    let cases = [
        (Edge::Left, 0.0, 20.0, Edge::Top, 0.0, 20.0, Corner::TopLeft),
        (
            Edge::Right,
            0.0,
            20.0,
            Edge::Top,
            80.0,
            100.0,
            Corner::TopRight,
        ),
        (
            Edge::Right,
            80.0,
            100.0,
            Edge::Bottom,
            80.0,
            100.0,
            Corner::BottomRight,
        ),
        (
            Edge::Left,
            80.0,
            100.0,
            Edge::Bottom,
            0.0,
            20.0,
            Corner::BottomLeft,
        ),
    ];
    let actual: Vec<_> = cases
        .iter()
        .map(|&(a, a0, a1, b, b0, b1, _)| {
            validate_topology(&two_links(
                endpoint(1, a, a0, a1),
                endpoint(1, b, b0, b1),
                false,
                true,
                false,
            ))
            .err()
            .map(|errors| errors.into_errors())
            .unwrap_or_default()
        })
        .collect();
    let expected: Vec<_> = cases
        .iter()
        .map(|&(a, _, _, b, _, _, corner)| vec![corner_error(corner, (a, b))])
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn disjoint_segments_near_top_right_and_bottom_left_are_not_ambiguous() {
    let cases = [
        (
            endpoint(1, Edge::Right, 80.0, 100.0),
            endpoint(1, Edge::Top, 80.0, 100.0),
        ),
        (
            endpoint(1, Edge::Left, 0.0, 20.0),
            endpoint(1, Edge::Bottom, 0.0, 20.0),
        ),
    ];
    let actual: Vec<_> = cases
        .into_iter()
        .map(|(first, second)| validate_topology(&two_links(first, second, true, false, false)))
        .collect();
    assert_eq!(actual, vec![Ok(()), Ok(())]);
}

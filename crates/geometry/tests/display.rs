use geometry::{Display, Edge, NodeDisplays, NodeId, Point, Rect, Rotation, Scale, Size};

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

#[test]
fn negative_origins_preserve_node_space_edge_coordinates() {
    let screen = display("left", Rect::new(-1920.0, -1200.0, 1080.0, 1000.0));
    assert!(screen.contains_local(Point::new(-1500.0, -500.0)));
    assert!(!screen.contains_local(Point::new(0.0, 0.0)));
    for (edge, expected) in [
        (Edge::Left, Point::new(-1920.0, -950.0)),
        (Edge::Right, Point::new(-840.0, -950.0)),
        (Edge::Top, Point::new(-1670.0, -1200.0)),
        (Edge::Bottom, Point::new(-1670.0, -200.0)),
    ] {
        assert_eq!(screen.point_on_edge(edge, 250.0), expected);
        assert_eq!(screen.edge_distance_of(edge, expected), 250.0);
    }
}

#[test]
fn member_bounds_preserve_negative_minima_and_include_a_gap() {
    let member = NodeDisplays::new(
        NodeId::new([1; 32]),
        vec![
            display("left", Rect::new(-1920.0, -1200.0, 1080.0, 1000.0)),
            display("right", Rect::new(100.0, 50.0, 800.0, 600.0)),
        ],
        true,
    );
    assert_eq!(
        member.bounds(),
        Some(Rect::new(-1920.0, -1200.0, 2820.0, 1850.0))
    );
    let gap = Point::new(-400.0, -100.0);
    assert!(member.bounds().unwrap().contains(gap));
    assert!(member
        .displays
        .iter()
        .all(|screen| !screen.contains_local(gap)));
}

#[test]
fn l_shaped_desktop_does_not_fill_its_bounding_box() {
    let member = NodeDisplays::new(
        NodeId::new([1; 32]),
        vec![
            display("top", Rect::new(0.0, 0.0, 200.0, 100.0)),
            display("bottom", Rect::new(0.0, 100.0, 100.0, 100.0)),
        ],
        true,
    );
    assert_eq!(member.bounds(), Some(Rect::new(0.0, 0.0, 200.0, 200.0)));
    let empty_quadrant = Point::new(150.0, 150.0);
    assert!(member.bounds().unwrap().contains(empty_quadrant));
    assert!(member
        .displays
        .iter()
        .all(|screen| !screen.contains_local(empty_quadrant)));
}

#[test]
fn member_without_displays_has_no_bounds() {
    assert_eq!(
        NodeDisplays::new(NodeId::new([1; 32]), vec![], false).bounds(),
        None
    );
}

#[test]
fn rectangle_containment_is_half_open_and_touching_is_not_intersection() {
    let rect = Rect::new(-20.0, -10.0, 40.0, 30.0);
    for point in [Point::new(-20.0, -10.0), Point::new(19.5, 19.5)] {
        assert!(rect.contains(point));
    }
    for point in [
        Point::new(20.0, 0.0),
        Point::new(0.0, 20.0),
        Point::new(-20.5, 0.0),
        Point::new(0.0, -10.5),
    ] {
        assert!(!rect.contains(point));
    }
    for touching in [
        Rect::new(20.0, -10.0, 10.0, 30.0),
        Rect::new(-30.0, -10.0, 10.0, 30.0),
        Rect::new(-20.0, 20.0, 40.0, 10.0),
        Rect::new(-20.0, -20.0, 40.0, 10.0),
        Rect::new(20.0, 20.0, 10.0, 10.0),
    ] {
        assert!(!rect.intersects(&touching));
        assert!(!touching.intersects(&rect));
    }
    let overlapping = Rect::new(19.0, 19.0, 10.0, 10.0);
    assert!(rect.intersects(&overlapping));
    assert!(overlapping.intersects(&rect));
}

#[test]
fn rotation_uses_oriented_bounds_for_edges_and_unrotated_native_mode() {
    for (rotation, bounds) in [
        (Rotation::Deg0, Rect::new(0.0, 0.0, 1920.0, 1080.0)),
        (Rotation::Deg90, Rect::new(0.0, 0.0, 1080.0, 1920.0)),
        (Rotation::Deg180, Rect::new(0.0, 0.0, 1920.0, 1080.0)),
        (Rotation::Deg270, Rect::new(0.0, 0.0, 1080.0, 1920.0)),
    ] {
        let mut screen = display("panel", bounds);
        screen.rotation = rotation;
        assert_eq!(
            screen.logical_size(),
            Size::new(bounds.width, bounds.height)
        );
        assert_eq!(
            screen.native_size(),
            Some(Size::new(bounds.width, bounds.height))
        );
        assert_eq!(screen.native_mode_size(), Some(Size::new(1920.0, 1080.0)));
        for edge in [Edge::Left, Edge::Right] {
            assert_eq!(screen.edge_length(edge), bounds.height);
        }
        for edge in [Edge::Top, Edge::Bottom] {
            assert_eq!(screen.edge_length(edge), bounds.width);
        }
    }
}

#[test]
fn rotation_wire_values_accept_only_quarter_turns() {
    for (degrees, rotation) in [
        (0, Rotation::Deg0),
        (90, Rotation::Deg90),
        (180, Rotation::Deg180),
        (270, Rotation::Deg270),
    ] {
        assert_eq!(Rotation::from_degrees(degrees), Some(rotation));
        assert_eq!(rotation.degrees(), degrees);
    }
    for degrees in [45, 360, 65535] {
        assert_eq!(Rotation::from_degrees(degrees), None);
    }
}

#[test]
fn fractional_and_per_axis_scale_round_trip_display_local_coordinates() {
    let mut screen = display("panel", Rect::new(-800.0, -600.0, 640.0, 480.0));
    screen.native_bounds = Some(Rect::new(-1600.0, -1200.0, 1280.0, 960.0));
    // Fixed samples exercise all rotations and unequal ratios without randomness.
    // Bounds origins and panel rotation must not enter display-local conversion.
    for rotation in [
        Rotation::Deg0,
        Rotation::Deg90,
        Rotation::Deg180,
        Rotation::Deg270,
    ] {
        screen.rotation = rotation;
        for (scale, expected_native) in [
            (Scale::uniform(1.25), Point::new(20.0, 30.0)),
            (Scale::uniform(1.5), Point::new(24.0, 36.0)),
            (Scale::uniform(2.0), Point::new(32.0, 48.0)),
            (Scale { x: 1.25, y: 2.0 }, Point::new(20.0, 48.0)),
        ] {
            screen.scale = scale;
            assert_eq!(
                screen.logical_to_native(Point::new(16.0, 24.0)),
                Some(expected_native)
            );
            assert_eq!(
                screen.native_to_logical(expected_native),
                Some(Point::new(16.0, 24.0))
            );
            for logical in [
                Point::new(0.0, 0.0),
                Point::new(16.0, 24.0),
                Point::new(127.5, 255.25),
                Point::new(639.5, 479.5),
            ] {
                let native = screen.logical_to_native(logical).unwrap();
                let restored = screen.native_to_logical(native).unwrap();
                assert!((restored.x - logical.x).abs() < 1e-10);
                assert!((restored.y - logical.y).abs() < 1e-10);
            }
        }
    }
}

#[test]
fn unknown_native_bounds_disable_all_native_queries() {
    let mut screen = display("portal", Rect::new(-640.0, 0.0, 640.0, 480.0));
    screen.native_bounds = None;
    screen.scale = Scale::uniform(1.5);
    assert_eq!(screen.native_size(), None);
    assert_eq!(screen.native_mode_size(), None);
    assert_eq!(screen.logical_to_native(Point::new(10.0, 20.0)), None);
    assert_eq!(screen.native_to_logical(Point::new(15.0, 30.0)), None);
    assert_eq!(screen.edge_length(Edge::Right), 480.0);
}

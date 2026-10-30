//! Pointer barriers for the Wayland capture session: pure geometry, no portal calls.
//!
//! `docs/03-architecture.md` («Захват»): the portal hands out zones (`GetZones`:
//! `width,height,x,y` in logical coordinates plus a `zone_set`) and «принимает барьеры
//! только на внешних границах зон; барьер целиком лежит в одной зоне, диагонали не
//! поддерживаются», so a barrier on an internal boundary between two displays of the
//! same node would be rejected by the compositor.
//!
//! The core does not send its transition links to the agent (there is no message for
//! them in `docs/ipc.proto`), so the barriers are the outer edges of the zone union —
//! exactly one activation trigger per reachable desktop border. When a link-aware
//! layout reaches the agent (KVM-072), this function is the single place that has to
//! learn about it.
//!
//! The module is deliberately platform-neutral: it deals in plain logical coordinates
//! and its unit tests therefore run on any host, not only on a Linux one.

/// One zone as `GetZones` reports it, in logical coordinates.
///
/// The origin and the sign of the coordinates are whatever the compositor uses; they
/// are carried through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in logical pixels.
    pub width: i32,
    /// Height in logical pixels.
    pub height: i32,
}

impl Zone {
    /// A zone; a non-positive extent makes it unusable and it contributes no barriers.
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Zone {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether the zone can carry barriers at all.
    pub const fn is_usable(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Horizontal half-open interval `[start, end)`.
    const fn x_interval(&self) -> Interval {
        Interval {
            start: self.x,
            end: self.x + self.width,
        }
    }

    /// Vertical half-open interval `[start, end)`.
    const fn y_interval(&self) -> Interval {
        Interval {
            start: self.y,
            end: self.y + self.height,
        }
    }
}

/// Which border of a zone a segment lies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// Left border.
    Left,
    /// Right border.
    Right,
    /// Top border.
    Top,
    /// Bottom border.
    Bottom,
}

impl Edge {
    /// The name used in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
}

/// One barrier: a straight segment on a zone border, with the id the portal echoes
/// back in `Activated { barrier_id }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarrierSegment {
    /// Portal barrier id: non-zero, unique inside one derivation, stable for the same
    /// zone list.
    pub id: u32,
    /// Index of the zone this barrier lives in.
    pub zone: usize,
    /// Which border of that zone.
    pub edge: Edge,
    /// First endpoint, inclusive.
    pub x1: i32,
    /// First endpoint, inclusive.
    pub y1: i32,
    /// Second endpoint, inclusive along the zone span.
    pub x2: i32,
    /// Second endpoint, inclusive along the zone span.
    pub y2: i32,
}

/// A half-open interval `[start, end)`; empty when `end <= start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Interval {
    start: i32,
    end: i32,
}

impl Interval {
    const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Whether the two intervals share at least one unit.
    const fn overlaps(&self, other: &Interval) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// `self` minus `blockers`, in order, as closed-left/open-right pieces.
    fn subtract(self, blockers: &[Interval]) -> Vec<Interval> {
        let mut pieces = vec![self];
        for blocker in blockers.iter().filter(|b| !b.is_empty()) {
            let mut next = Vec::new();
            for piece in pieces {
                if !piece.overlaps(blocker) {
                    next.push(piece);
                    continue;
                }
                if piece.start < blocker.start {
                    next.push(Interval {
                        start: piece.start,
                        end: blocker.start,
                    });
                }
                if blocker.end < piece.end {
                    next.push(Interval {
                        start: blocker.end,
                        end: piece.end,
                    });
                }
            }
            pieces = next;
        }
        pieces
    }
}

/// The barriers a zone set needs: the outer edges of the union, split so that every
/// segment lies inside exactly one zone.
///
/// Barriers are generated zone by zone in the given order and, inside a zone, in
/// [`Edge::ALL`] order, so the ids (`1..=n`) are stable for the same input. A zone with
/// a non-positive extent contributes nothing.
pub fn derive(zones: &[Zone]) -> Vec<BarrierSegment> {
    let mut barriers = Vec::new();
    let mut next_id: u32 = 1;

    for (index, zone) in zones.iter().enumerate() {
        if !zone.is_usable() {
            continue;
        }
        let x = zone.x_interval();
        let y = zone.y_interval();
        // XDG InputCapture positions the barrier on the half-open zone's outer edge:
        // right/bottom coordinates are the exclusive extent, not the last pixel.
        let borders: [(Edge, i32, Interval, bool); 4] = [
            (Edge::Left, zone.x, y, false),
            (Edge::Right, zone.x + zone.width, y, false),
            (Edge::Top, zone.y, x, true),
            (Edge::Bottom, zone.y + zone.height, x, true),
        ];

        for (edge, fixed, along, horizontal) in borders {
            // A neighbour covers part of this border when its opposite border lies on
            // the same line and the two stretches overlap.
            let blockers: Vec<Interval> = zones
                .iter()
                .enumerate()
                .filter(|(other, candidate)| *other != index && candidate.is_usable())
                .filter(|(_, candidate)| match edge {
                    Edge::Left => candidate.x_interval().end == zone.x,
                    Edge::Right => candidate.x_interval().start == zone.x + zone.width,
                    Edge::Top => candidate.y_interval().end == zone.y,
                    Edge::Bottom => candidate.y_interval().start == zone.y + zone.height,
                })
                .map(|(_, candidate)| {
                    if horizontal {
                        candidate.x_interval()
                    } else {
                        candidate.y_interval()
                    }
                })
                .collect();

            for piece in along.subtract(&blockers) {
                if piece.is_empty() {
                    continue;
                }
                debug_assert!(!piece.is_empty());
                let mut segment = BarrierSegment {
                    id: next_id,
                    zone: index,
                    edge,
                    x1: 0,
                    y1: 0,
                    x2: 0,
                    y2: 0,
                };
                if horizontal {
                    segment.x1 = piece.start;
                    segment.x2 = piece.end - 1;
                    segment.y1 = fixed;
                    segment.y2 = fixed;
                } else {
                    segment.y1 = piece.start;
                    segment.y2 = piece.end - 1;
                    segment.x1 = fixed;
                    segment.x2 = fixed;
                }
                barriers.push(segment);
                next_id += 1;
            }
        }
    }

    barriers
}

/// Which zone and edge a barrier id from `Activated` belongs to.
pub fn locate(barriers: &[BarrierSegment], id: u32) -> Option<(usize, Edge)> {
    barriers
        .iter()
        .find(|barrier| barrier.id == id)
        .map(|barrier| (barrier.zone, barrier.edge))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(
        id: u32,
        zone: usize,
        edge: Edge,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    ) -> BarrierSegment {
        BarrierSegment {
            id,
            zone,
            edge,
            x1,
            y1,
            x2,
            y2,
        }
    }

    #[test]
    fn one_zone_gets_all_four_borders() {
        let barriers = derive(&[Zone::new(0, 0, 100, 50)]);
        assert_eq!(
            barriers,
            vec![
                segment(1, 0, Edge::Left, 0, 0, 0, 49),
                segment(2, 0, Edge::Right, 100, 0, 100, 49),
                segment(3, 0, Edge::Top, 0, 0, 99, 0),
                segment(4, 0, Edge::Bottom, 0, 50, 99, 50),
            ]
        );
    }

    #[test]
    fn a_shared_border_between_two_zones_is_not_a_barrier() {
        // Side by side: the touching vertical borders are internal, the four
        // horizontal ones are outer.
        let barriers = derive(&[Zone::new(0, 0, 100, 50), Zone::new(100, 0, 100, 50)]);
        let edges: Vec<(usize, Edge)> = barriers.iter().map(|b| (b.zone, b.edge)).collect();
        assert_eq!(
            edges,
            vec![
                (0, Edge::Left),
                (0, Edge::Top),
                (0, Edge::Bottom),
                (1, Edge::Right),
                (1, Edge::Top),
                (1, Edge::Bottom),
            ]
        );
        // Each segment stays inside one zone: the two top borders are separate
        // segments even though they are collinear.
        assert_eq!(barriers[1], segment(2, 0, Edge::Top, 0, 0, 99, 0));
        assert_eq!(barriers[4], segment(5, 1, Edge::Top, 100, 0, 199, 0));

        // Stacked: the touching horizontal borders are internal.
        let stacked = derive(&[Zone::new(0, 0, 100, 50), Zone::new(0, 50, 100, 50)]);
        let edges: Vec<(usize, Edge)> = stacked.iter().map(|b| (b.zone, b.edge)).collect();
        assert_eq!(
            edges,
            vec![
                (0, Edge::Left),
                (0, Edge::Right),
                (0, Edge::Top),
                (1, Edge::Left),
                (1, Edge::Right),
                (1, Edge::Bottom),
            ]
        );
    }

    #[test]
    fn a_partially_overlapping_neighbour_clips_the_border() {
        // B sits against A's right border but only for y in [20, 70), so the border
        // survives above and below it.
        let barriers = derive(&[Zone::new(0, 0, 100, 100), Zone::new(100, 20, 50, 50)]);
        let right: Vec<BarrierSegment> = barriers
            .iter()
            .filter(|b| b.zone == 0 && b.edge == Edge::Right)
            .copied()
            .collect();
        assert_eq!(
            right,
            vec![
                segment(2, 0, Edge::Right, 100, 0, 100, 19),
                segment(3, 0, Edge::Right, 100, 70, 100, 99),
            ]
        );

        // B's left border is fully covered and contributes nothing.
        assert!(!barriers.iter().any(|b| b.zone == 1 && b.edge == Edge::Left));
    }

    #[test]
    fn a_neighbour_that_only_touches_a_corner_covers_nothing() {
        let barriers = derive(&[Zone::new(0, 0, 100, 100), Zone::new(100, 100, 50, 50)]);
        // A's right border is not covered: the neighbour starts at y = 100, which is
        // outside A's half-open y range.
        assert!(barriers
            .iter()
            .any(|b| b.zone == 0 && b.edge == Edge::Right));
        // B's left border is not covered either.
        assert!(barriers.iter().any(|b| b.zone == 1 && b.edge == Edge::Left));
    }

    #[test]
    fn ids_are_unique_non_zero_and_stable() {
        let zones = [
            Zone::new(0, 0, 100, 100),
            Zone::new(100, 0, 100, 100),
            Zone::new(0, 100, 200, 80),
        ];
        let first = derive(&zones);
        let second = derive(&zones);
        assert_eq!(first, second, "the same zones must give the same ids");

        let mut ids: Vec<u32> = first.iter().map(|b| b.id).collect();
        assert!(ids.iter().all(|id| *id != 0));
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), first.len(), "ids must be unique");
        assert_eq!(ids, (1..=first.len() as u32).collect::<Vec<u32>>());

        // Every id can be traced back to the zone and edge the compositor names in
        // `Activated`.
        for barrier in &first {
            assert_eq!(
                locate(&first, barrier.id),
                Some((barrier.zone, barrier.edge))
            );
        }
        assert_eq!(locate(&first, 0), None);
        assert_eq!(locate(&first, first.len() as u32 + 1), None);
    }

    #[test]
    fn degenerate_and_negative_geometry_is_handled() {
        assert!(derive(&[]).is_empty());
        // A zero-width or zero-height zone cannot carry a barrier.
        assert!(derive(&[Zone::new(0, 0, 0, 50), Zone::new(0, 0, 50, 0)]).is_empty());
        // Negative origins are carried through unchanged.
        let barriers = derive(&[Zone::new(-200, -100, 100, 50)]);
        assert_eq!(
            barriers,
            vec![
                segment(1, 0, Edge::Left, -200, -100, -200, -51),
                segment(2, 0, Edge::Right, -100, -100, -100, -51),
                segment(3, 0, Edge::Top, -200, -100, -101, -100),
                segment(4, 0, Edge::Bottom, -200, -50, -101, -50),
            ]
        );
        // A one-pixel zone still has four single-point borders.
        assert_eq!(derive(&[Zone::new(5, 5, 1, 1)]).len(), 4);
    }

    #[test]
    fn interval_subtraction_keeps_the_pieces_in_order() {
        let base = Interval { start: 0, end: 10 };
        assert_eq!(base.subtract(&[]), vec![base]);
        assert_eq!(
            base.subtract(&[Interval { start: 3, end: 5 }]),
            vec![
                Interval { start: 0, end: 3 },
                Interval { start: 5, end: 10 },
            ]
        );
        assert_eq!(
            base.subtract(&[Interval { start: 2, end: 4 }, Interval { start: 6, end: 8 },]),
            vec![
                Interval { start: 0, end: 2 },
                Interval { start: 4, end: 6 },
                Interval { start: 8, end: 10 },
            ]
        );
        assert!(base.subtract(&[Interval { start: -5, end: 10 }]).is_empty());
        // An empty blocker changes nothing.
        assert_eq!(base.subtract(&[Interval { start: 5, end: 5 }]), vec![base]);
    }
}

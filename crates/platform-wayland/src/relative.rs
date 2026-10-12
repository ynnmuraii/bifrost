//! Pure cursor estimation and capability selection for EIS pointer injection.

use crate::WaylandCapabilities;
use input_model::Position;

/// The best pointer path the connected EIS device supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerMode {
    /// `ei_pointer_absolute` is present and the compositor supplied a valid region.
    Absolute,
    /// Only `ei_pointer` relative motion is usable.
    Relative,
    /// Neither pointer interface can move the cursor.
    Unavailable,
}

/// Selects absolute when valid; otherwise falls back to relative if available.
#[must_use]
pub const fn select_pointer_mode(capabilities: WaylandCapabilities) -> PointerMode {
    if capabilities.has_absolute_injection {
        PointerMode::Absolute
    } else if capabilities.has_relative_injection {
        PointerMode::Relative
    } else {
        PointerMode::Unavailable
    }
}

/// The local logical bounds used to clamp a receiver-owned cursor estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelativeBounds {
    /// X coordinate of the logical left edge.
    pub x: i32,
    /// Y coordinate of the logical top edge.
    pub y: i32,
    /// Logical width, greater than zero.
    pub width: u32,
    /// Logical height, greater than zero.
    pub height: u32,
}

/// One integral relative motion passed to EIS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeMotion {
    /// Horizontal delta in logical pixels.
    pub dx: i32,
    /// Vertical delta in logical pixels.
    pub dy: i32,
}

/// Tracks a cursor estimate when the compositor offers no absolute EIS interface.
///
/// At entry, a large negative motion relies on compositor edge clamping to establish
/// the top-left corner; subsequent deltas are exact relative to that estimate. This
/// cannot observe external cursor movement or guarantee exact placement if the
/// compositor does not clamp to the advertised logical bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelativeCursor {
    bounds: RelativeBounds,
    position: Position,
}

impl RelativeCursor {
    /// Starts the estimate at the logical top-left corner.
    #[must_use]
    pub fn enter(bounds: RelativeBounds) -> (Self, RelativeMotion) {
        (
            Self {
                bounds,
                position: Position::new(f64::from(bounds.x), f64::from(bounds.y)),
            },
            RelativeMotion {
                dx: -32768,
                dy: -32768,
            },
        )
    }

    /// Moves the estimate toward `target`, clamped to the local logical bounds.
    pub fn move_to(&mut self, target: Position) -> RelativeMotion {
        let max_x = f64::from(self.bounds.x) + f64::from(self.bounds.width.saturating_sub(1));
        let max_y = f64::from(self.bounds.y) + f64::from(self.bounds.height.saturating_sub(1));
        let next = Position::new(
            target.x.clamp(f64::from(self.bounds.x), max_x),
            target.y.clamp(f64::from(self.bounds.y), max_y),
        );
        let motion = RelativeMotion {
            dx: (next.x - self.position.x).round() as i32,
            dy: (next.y - self.position.y).round() as i32,
        };
        self.position = next;
        motion
    }

    /// Current estimated cursor position.
    #[must_use]
    pub const fn position(&self) -> Position {
        self.position
    }
}

#[cfg(test)]
mod tests {
    use super::{select_pointer_mode, PointerMode, RelativeBounds, RelativeCursor, RelativeMotion};
    use crate::WaylandCapabilities;
    use input_model::Position;

    #[test]
    fn pointer_mode_prefers_valid_absolute_then_relative_without_gating_other_devices() {
        assert_eq!(
            select_pointer_mode(WaylandCapabilities {
                has_absolute_injection: true,
                has_relative_injection: true,
                has_keyboard: true,
                ..WaylandCapabilities::default()
            }),
            PointerMode::Absolute
        );
        assert_eq!(
            select_pointer_mode(WaylandCapabilities {
                has_relative_injection: true,
                has_keyboard: true,
                ..WaylandCapabilities::default()
            }),
            PointerMode::Relative
        );
        assert_eq!(
            select_pointer_mode(WaylandCapabilities {
                has_keyboard: true,
                has_buttons: true,
                has_scroll: true,
                ..WaylandCapabilities::default()
            }),
            PointerMode::Unavailable
        );
    }

    #[test]
    fn entry_resets_to_corner_and_clamped_moves_track_estimate() {
        let bounds = RelativeBounds {
            x: 100,
            y: 50,
            width: 800,
            height: 600,
        };
        let (mut cursor, reset) = RelativeCursor::enter(bounds);
        assert_eq!(
            reset,
            RelativeMotion {
                dx: -32768,
                dy: -32768
            }
        );
        assert_eq!(cursor.position(), Position::new(100.0, 50.0));
        assert_eq!(
            cursor.move_to(Position::new(300.0, 220.0)),
            RelativeMotion { dx: 200, dy: 170 }
        );
        assert_eq!(
            cursor.move_to(Position::new(10_000.0, -1.0)),
            RelativeMotion { dx: 599, dy: -170 }
        );
        assert_eq!(cursor.position(), Position::new(899.0, 50.0));
    }
}

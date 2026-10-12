//! Scroll direction between macOS and the wire.
//!
//! The wire follows evdev / Wayland: positive values scroll down (axis 0)
//! or right (axis 1). `CGEvent` scroll wheel deltas are positive for up
//! (wheel 1) and left (wheel 2). Capture and emulation both convert here, so
//! mac -> other, other -> mac and mac -> mac all keep their direction.

use crate::PointerEvent;

/// One wheel line is one third of a 120 step.
const V120_PER_LINE: i32 = 40;

const VERTICAL: u8 = 0;
const HORIZONTAL: u8 = 1;

/// Wheel deltas of a macOS scroll event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacosScroll {
    /// line based (mouse wheel) instead of pixel based (trackpad)
    pub lines: bool,
    /// wheel 1, positive = up
    pub vertical: i32,
    /// wheel 2, positive = left
    pub horizontal: i32,
}

impl MacosScroll {
    /// Wire events for this macOS scroll event (one per non-zero axis).
    pub fn to_wire(self) -> impl Iterator<Item = PointerEvent> {
        [(VERTICAL, self.vertical), (HORIZONTAL, self.horizontal)]
            .into_iter()
            .filter(|&(_, delta)| delta != 0)
            .map(move |(axis, delta)| {
                if self.lines {
                    PointerEvent::AxisDiscrete120 {
                        axis,
                        value: -delta * V120_PER_LINE,
                    }
                } else {
                    PointerEvent::Axis {
                        time: 0,
                        axis,
                        value: -delta as f64,
                    }
                }
            })
    }

    /// The macOS scroll event for a wire scroll event; `None` for other
    /// events and unknown axes.
    pub fn from_wire(event: PointerEvent) -> Option<Self> {
        let (lines, axis, delta) = match event {
            PointerEvent::Axis { axis, value, .. } => (false, axis, -(value as i32)),
            PointerEvent::AxisDiscrete120 { axis, value } => (true, axis, -value / V120_PER_LINE),
            _ => return None,
        };
        let (vertical, horizontal) = match axis {
            VERTICAL => (delta, 0),
            HORIZONTAL => (0, delta),
            _ => return None,
        };
        Some(Self {
            lines,
            vertical,
            horizontal,
        })
    }

    /// Number of wheels to post: 1 for vertical only, 2 when wheel 2 is used.
    pub fn wheel_count(&self) -> u32 {
        if self.horizontal != 0 { 2 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_wheel_up_scrolls_up_on_the_wire() {
        let up = MacosScroll {
            lines: true,
            vertical: 1,
            horizontal: 0,
        };
        assert_eq!(
            up.to_wire().collect::<Vec<_>>(),
            [PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: -40
            }]
        );
        let right = MacosScroll {
            lines: false,
            vertical: 0,
            horizontal: -7,
        };
        assert_eq!(
            right.to_wire().collect::<Vec<_>>(),
            [PointerEvent::Axis {
                time: 0,
                axis: 1,
                value: 7.0
            }]
        );
    }

    #[test]
    fn wire_scroll_down_scrolls_down_on_mac() {
        // a Linux / Windows wheel step down is +120 on axis 0
        let down = MacosScroll::from_wire(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: 120,
        });
        assert_eq!(
            down,
            Some(MacosScroll {
                lines: true,
                vertical: -3,
                horizontal: 0
            })
        );
        let right = MacosScroll::from_wire(PointerEvent::Axis {
            time: 0,
            axis: 1,
            value: 12.0,
        })
        .unwrap();
        assert_eq!((right.vertical, right.horizontal), (0, -12));
        assert_eq!(right.wheel_count(), 2);
        assert_eq!(
            MacosScroll::from_wire(PointerEvent::Axis {
                time: 0,
                axis: 2,
                value: 1.0
            }),
            None
        );
    }

    #[test]
    fn mac_to_mac_keeps_direction() {
        for scroll in [
            MacosScroll {
                lines: true,
                vertical: 2,
                horizontal: -1,
            },
            MacosScroll {
                lines: false,
                vertical: -15,
                horizontal: 4,
            },
        ] {
            let (mut vertical, mut horizontal) = (0, 0);
            for event in scroll.to_wire() {
                let received = MacosScroll::from_wire(event).unwrap();
                assert_eq!(received.lines, scroll.lines);
                vertical += received.vertical;
                horizontal += received.horizontal;
            }
            assert_eq!((vertical, horizontal), (scroll.vertical, scroll.horizontal));
        }
    }
}

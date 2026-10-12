//! Where the pointer appears when a remote cursor enters this desktop.

use std::fmt::{self, Display};

/// Edge of the local desktop the remote cursor enters through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        };
        f.write_str(s)
    }
}

/// A display in desktop coordinates, in the unit the backend positions the
/// pointer with (physical pixels on Windows, logical pixels on Wayland and
/// macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Distance kept from the entered edge: the local capture for the side the
/// cursor came from sits on the outermost pixel (layer-shell surface, macOS
/// edge test), so landing on it would send the cursor straight back.
const EDGE_INSET: i32 = 1;

impl Rect {
    /// Range covered along the edge (y for left/right, x for top/bottom).
    fn along(&self, edge: Edge) -> (i32, i32) {
        match edge {
            Edge::Left | Edge::Right => (self.y, self.y + self.height),
            Edge::Top | Edge::Bottom => (self.x, self.x + self.width),
        }
    }

    /// How far out this display reaches towards `edge`; larger is further.
    fn reach(&self, edge: Edge) -> i32 {
        match edge {
            Edge::Left => -self.x,
            Edge::Right => self.x + self.width,
            Edge::Top => -self.y,
            Edge::Bottom => self.y + self.height,
        }
    }

    /// Coordinate across the edge, `EDGE_INSET` inside this display.
    fn inside(&self, edge: Edge) -> i32 {
        let (start, len) = match edge {
            Edge::Left | Edge::Right => (self.x, self.width),
            Edge::Top | Edge::Bottom => (self.y, self.height),
        };
        let inset = EDGE_INSET.min(len - 1);
        match edge {
            Edge::Left | Edge::Top => start + inset,
            Edge::Right | Edge::Bottom => start + len - 1 - inset,
        }
    }
}

/// The point at `fraction` (0 = top / left end, `u16::MAX` = the other end)
/// along `edge` of the desktop formed by `displays`.
///
/// The fraction is taken over the bounding box of all displays. The point
/// lands on the outermost display at that height (left/right) or column
/// (top/bottom); where no display covers it, on the nearest display, clamped
/// onto it. `None` without a usable display.
pub(crate) fn entry_point(displays: &[Rect], edge: Edge, fraction: u16) -> Option<(i32, i32)> {
    let displays = || displays.iter().filter(|d| d.width > 0 && d.height > 0);
    let lo = displays().map(|d| d.along(edge).0).min()?;
    let hi = displays().map(|d| d.along(edge).1).max()?;
    let fraction = f64::from(fraction) / f64::from(u16::MAX);
    let target = lo + (fraction * f64::from(hi - 1 - lo)).round() as i32;

    // distance along the edge from `target` to the display (0 if covered)
    let distance = |d: &Rect| {
        let (start, end) = d.along(edge);
        (start - target).max(target - (end - 1)).max(0)
    };
    let display = displays().min_by_key(|d| (distance(d), std::cmp::Reverse(d.reach(edge))))?;
    let (start, end) = display.along(edge);
    let along = target.clamp(start, end - 1);
    let across = display.inside(edge);
    Some(match edge {
        Edge::Left | Edge::Right => (across, along),
        Edge::Top | Edge::Bottom => (along, across),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    const HALF: u16 = u16::MAX / 2 + 1;

    #[test]
    fn single_display_maps_fraction_along_each_edge() {
        let d = [rect(0, 0, 1920, 1080)];
        assert_eq!(entry_point(&d, Edge::Left, 0), Some((1, 0)));
        assert_eq!(entry_point(&d, Edge::Left, u16::MAX), Some((1, 1079)));
        assert_eq!(entry_point(&d, Edge::Right, HALF), Some((1918, 540)));
        assert_eq!(entry_point(&d, Edge::Top, HALF), Some((960, 1)));
        assert_eq!(entry_point(&d, Edge::Bottom, u16::MAX), Some((1919, 1078)));
    }

    #[test]
    fn negative_origins() {
        // Windows: secondary monitor left of the primary
        let d = [rect(-1280, -200, 1280, 1024), rect(0, 0, 1920, 1080)];
        // left edge belongs to the secondary monitor where it exists
        assert_eq!(entry_point(&d, Edge::Left, 0), Some((-1279, -200)));
        // only the secondary monitor exists at the top of the box, so the
        // right edge at that height is its right edge
        assert_eq!(entry_point(&d, Edge::Right, 0), Some((-2, -200)));
        assert_eq!(entry_point(&d, Edge::Right, u16::MAX), Some((1918, 1079)));
    }

    #[test]
    fn outermost_display_wins_at_that_height() {
        // small monitor left of a tall one, aligned at the top
        let d = [rect(0, 0, 1280, 720), rect(1280, 0, 1920, 1200)];
        // upper part of the left edge is on the small monitor
        assert_eq!(entry_point(&d, Edge::Left, 0x4000), Some((1, 300)));
        // below the small monitor the left edge is the tall monitor's
        let (x, y) = entry_point(&d, Edge::Left, 0xF000).unwrap();
        assert_eq!(x, 1281);
        assert!((1100..1130).contains(&y), "{y}");
        // the right edge spans the full height of the tall monitor
        assert_eq!(entry_point(&d, Edge::Right, u16::MAX), Some((3198, 1199)));
    }

    #[test]
    fn gaps_clamp_onto_the_nearest_display() {
        // two monitors stacked side by side with a vertical gap between
        // their ranges: y 0..500 and y 600..1100
        let d = [rect(0, 0, 800, 500), rect(800, 600, 800, 500)];
        // y = 550 is covered by neither; nearest along the edge is the
        // first (distance 51) vs the second (distance 50)
        let (x, y) = entry_point(&d, Edge::Left, HALF).unwrap();
        assert_eq!((x, y), (801, 600));
        // every result lies on a real display
        for fraction in (0..=u16::MAX).step_by(997) {
            for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                let (x, y) = entry_point(&d, edge, fraction).unwrap();
                assert!(
                    d.iter()
                        .any(|r| (r.x..r.x + r.width).contains(&x)
                            && (r.y..r.y + r.height).contains(&y)),
                    "{edge} {fraction}: ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn degenerate_displays() {
        assert_eq!(entry_point(&[], Edge::Left, 0), None);
        assert_eq!(entry_point(&[rect(0, 0, 0, 1080)], Edge::Left, 0), None);
        // a 1 px wide display cannot keep the inset
        assert_eq!(
            entry_point(&[rect(5, 0, 1, 100)], Edge::Left, u16::MAX),
            Some((5, 99))
        );
    }
}

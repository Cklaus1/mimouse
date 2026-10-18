//! Where the pointer appears when a remote cursor enters this desktop, and
//! where an agent's desktop pixels are in the backend's units.

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
/// pointer with (physical pixels on Windows, logical pixels on Wayland,
/// points on macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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

/// The bounding box of the displays with an area, `None` without one.
pub(crate) fn bounds(displays: &[Rect]) -> Option<Rect> {
    let displays = || displays.iter().filter(|d| d.width > 0 && d.height > 0);
    let left = displays().map(|d| d.x).min()?;
    let top = displays().map(|d| d.y).min()?;
    let right = displays().map(|d| d.x + d.width).max()?;
    let bottom = displays().map(|d| d.y + d.height).max()?;
    Some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

/// The point at `(x, y)` of a desktop `size` (W, H) pixels large, mapped
/// uniformly onto the bounding box B of `displays`:
/// `B.x + x * B.width / W`, `B.y + y * B.height / H`, clamped inside B.
/// `None` without a display with an area or for an empty desktop.
pub(crate) fn desktop_to_backend(
    displays: &[Rect],
    x: i32,
    y: i32,
    size: (u32, u32),
) -> Option<(i32, i32)> {
    let (w, h) = size;
    if w == 0 || h == 0 {
        return None;
    }
    let b = bounds(displays)?;
    let map = |at: i32, len: u32, start: i32, extent: i32| {
        let offset = (i64::from(at) * i64::from(extent)).div_euclid(i64::from(len));
        let offset = offset.clamp(0, i64::from(extent) - 1);
        // within 0..extent, so in i32 range
        start + offset as i32
    };
    Some((map(x, w, b.x, b.width), map(y, h, b.y, b.height)))
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

    #[test]
    fn equal_sizes_map_to_the_shifted_identity() {
        // Windows: physical pixels both ways, virtual screen left of and
        // above 0
        let d = [rect(-1280, -200, 1280, 1024), rect(0, 0, 1920, 1080)];
        let size = (3200, 1280);
        assert_eq!(desktop_to_backend(&d, 0, 0, size), Some((-1280, -200)));
        assert_eq!(desktop_to_backend(&d, 1280, 200, size), Some((0, 0)));
        assert_eq!(desktop_to_backend(&d, 3199, 1279, size), Some((1919, 1079)));
    }

    #[test]
    fn scaled_desktops_map_uniformly() {
        // macOS retina: a 2x screenshot of a desktop measured in points
        let d = [rect(0, 0, 1440, 900)];
        assert_eq!(
            desktop_to_backend(&d, 2879, 1799, (2880, 1800)),
            Some((1439, 899))
        );
        assert_eq!(
            desktop_to_backend(&d, 1440, 900, (2880, 1800)),
            Some((720, 450))
        );
        assert_eq!(desktop_to_backend(&d, 3, 3, (2880, 1800)), Some((1, 1)));
        // GNOME: logical layout at 1.5x, origin of the second monitor
        let d = [rect(0, 0, 1280, 720), rect(1280, 0, 1280, 720)];
        assert_eq!(
            desktop_to_backend(&d, 1920, 0, (3840, 1080)),
            Some((1280, 0))
        );
    }

    #[test]
    fn points_outside_the_desktop_clamp_into_the_box() {
        let d = [rect(100, 50, 800, 600)];
        let size = (800, 600);
        assert_eq!(desktop_to_backend(&d, -5, -1, size), Some((100, 50)));
        assert_eq!(desktop_to_backend(&d, 800, 600, size), Some((899, 649)));
        assert_eq!(
            desktop_to_backend(&d, i32::MAX, i32::MIN, size),
            Some((899, 50))
        );
        // a gap between monitors is inside the box: not moved onto one
        let d = [rect(0, 0, 100, 100), rect(200, 0, 100, 100)];
        assert_eq!(desktop_to_backend(&d, 150, 50, (300, 100)), Some((150, 50)));
    }

    #[test]
    fn empty_inputs_map_nowhere() {
        let d = [rect(0, 0, 1920, 1080)];
        assert_eq!(desktop_to_backend(&d, 0, 0, (0, 1080)), None);
        assert_eq!(desktop_to_backend(&d, 0, 0, (1920, 0)), None);
        assert_eq!(desktop_to_backend(&[], 0, 0, (1920, 1080)), None);
        // displays without an area do not count
        let d = [rect(-500, 0, 0, 0), rect(0, 0, 10, 10)];
        assert_eq!(bounds(&d), Some(rect(0, 0, 10, 10)));
        assert_eq!(
            desktop_to_backend(&[rect(0, 0, 0, 1080)], 0, 0, (1, 1)),
            None
        );
    }
}

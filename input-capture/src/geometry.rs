//! Where along an edge of the desktop the cursor crossed, and which part of
//! an edge activates a client.

use crate::{EdgeRange, Position};

/// A display in desktop coordinates (any unit, as long as all displays and
/// points of one backend share it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Rect {
    /// Range covered along edges at `pos` (y for left/right, x for top/bottom).
    fn along(&self, pos: Position) -> (f64, f64) {
        match pos {
            Position::Left | Position::Right => (self.y, self.y + self.height),
            Position::Top | Position::Bottom => (self.x, self.x + self.width),
        }
    }
}

/// Extent of the desktop's edge at `pos`: its bounding box along that side
/// (y for left/right, x for top/bottom), end exclusive.
pub(crate) fn edge_extent(displays: &[Rect], pos: Position) -> Option<(f64, f64)> {
    displays
        .iter()
        .filter(|d| d.width > 0. && d.height > 0.)
        .map(|d| d.along(pos))
        .reduce(|(lo, hi), (start, end)| (lo.min(start), hi.max(end)))
}

/// Coordinate of `point` along edges at `pos`.
pub(crate) fn along(point: (f64, f64), pos: Position) -> f64 {
    match pos {
        Position::Left | Position::Right => point.1,
        Position::Top | Position::Bottom => point.0,
    }
}

/// The segment `range` covers of an edge spanning `extent`, end exclusive.
pub(crate) fn range_segment(extent: (f64, f64), range: EdgeRange) -> (f64, f64) {
    let (lo, hi) = extent;
    let at = |percent: u8| lo + (hi - lo) * f64::from(percent) / 100.;
    (at(range.start()), at(range.end()))
}

/// Whether `point`, a coordinate along an edge spanning `extent`, lies in
/// `range` of it; `None` is the whole edge.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn in_range(extent: (f64, f64), point: f64, range: Option<EdgeRange>) -> bool {
    let Some(range) = range else {
        return true;
    };
    let (start, end) = range_segment(extent, range);
    start <= point && point < end
}

/// The barrier on `display`'s `pos` side for `range` of the edge formed by
/// `displays`: the whole pixels `[first, end)` along that side inside the
/// range (`in_range` holds for exactly these). `None` when the range misses
/// the display.
#[cfg(any(libei, layer_shell, test))]
pub(crate) fn barrier_segment(
    displays: &[Rect],
    display: &Rect,
    pos: Position,
    range: Option<EdgeRange>,
) -> Option<(f64, f64)> {
    let side = display.along(pos);
    let Some(range) = range else {
        return Some(side);
    };
    let (start, end) = range_segment(edge_extent(displays, pos)?, range);
    let first = side.0.max(start).ceil();
    let end = side.1.min(end).ceil();
    (first < end).then_some((first, end))
}

/// Where `point` lies along the `pos` edge of the bounding box of
/// `displays`: 0 at the top / left pixel, `u16::MAX` at the bottom / right
/// one. The receiver maps it back the same way (input-emulation
/// `geometry::entry_point`). `None` without a usable display.
pub(crate) fn edge_fraction(displays: &[Rect], pos: Position, point: (f64, f64)) -> Option<u16> {
    let (lo, hi) = edge_extent(displays, pos)?;
    let last = hi - 1.;
    if last <= lo {
        return Some(0);
    }
    let fraction = ((along(point, pos) - lo) / (last - lo)).clamp(0., 1.);
    Some((fraction * f64::from(u16::MAX)).round() as u16)
}

/// Where capture starts without a crossing (a hotkey or `cli focus`): the
/// middle of `range` (`None`: the whole edge) on the `pos` edge, on the
/// outermost pixel of the display there, as if the cursor had crossed
/// there. `None` without a usable display.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn focus_point(
    displays: &[Rect],
    pos: Position,
    range: Option<EdgeRange>,
) -> Option<(f64, f64)> {
    let extent = edge_extent(displays, pos)?;
    let (start, end) = match range {
        Some(range) => range_segment(extent, range),
        None => extent,
    };
    let mid = ((start + end) / 2.).floor();
    // how far out the display's `pos` side lies
    let outward = |d: &Rect| match pos {
        Position::Left => -d.x,
        Position::Right => d.x + d.width,
        Position::Top => -d.y,
        Position::Bottom => d.y + d.height,
    };
    // how far `mid` lies outside the display along the edge
    let distance = |d: &Rect| {
        let (lo, hi) = d.along(pos);
        (lo - mid).max(mid - (hi - 1.)).max(0.)
    };
    let usable = || displays.iter().filter(|d| d.width > 0. && d.height > 0.);
    let display = usable()
        .filter(|d| distance(d) == 0.)
        .max_by(|a, b| outward(a).total_cmp(&outward(b)))
        .or_else(|| usable().min_by(|a, b| distance(a).total_cmp(&distance(b))))?;
    let (lo, hi) = display.along(pos);
    let along = mid.clamp(lo, hi - 1.);
    Some(match pos {
        Position::Left => (display.x, along),
        Position::Right => (display.x + display.width - 1., along),
        Position::Top => (along, display.y),
        Position::Bottom => (along, display.y + display.height - 1.),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    const HALF: u16 = u16::MAX / 2 + 1;

    #[test]
    fn single_display_ends_and_middle() {
        let d = [rect(0., 0., 1920., 1080.)];
        assert_eq!(edge_fraction(&d, Position::Left, (0., 0.)), Some(0));
        assert_eq!(
            edge_fraction(&d, Position::Right, (1919., 1079.)),
            Some(u16::MAX)
        );
        assert_eq!(edge_fraction(&d, Position::Top, (959.5, 0.)), Some(HALF));
        // megapc crossing to the left at y = 600
        assert_eq!(edge_fraction(&d, Position::Left, (0., 600.)), Some(36442));
    }

    #[test]
    fn multi_monitor_uses_the_bounding_box() {
        // 1080p primary, 1440p secondary to the right and 200 px higher
        let d = [rect(0., 0., 1920., 1080.), rect(1920., -200., 2560., 1440.)];
        // the right edge spans y = -200..1240 over both displays
        assert_eq!(edge_fraction(&d, Position::Right, (4479., -200.)), Some(0));
        assert_eq!(
            edge_fraction(&d, Position::Right, (4479., 1239.)),
            Some(u16::MAX)
        );
        // the primary's top row lies 200 px into that span
        let f = edge_fraction(&d, Position::Left, (0., 0.)).unwrap();
        assert_eq!(f, (200. / 1439. * 65535f64).round() as u16);
        // top / bottom span x = 0..4480
        assert_eq!(
            edge_fraction(&d, Position::Bottom, (2239.5, 1239.)),
            Some(HALF)
        );
    }

    #[test]
    fn negative_origins_and_points_outside_are_clamped() {
        let d = [
            rect(-1280., -200., 1280., 1024.),
            rect(0., 0., 1920., 1080.),
        ];
        assert_eq!(edge_fraction(&d, Position::Top, (-1280., -200.)), Some(0));
        assert_eq!(edge_fraction(&d, Position::Top, (-5000., 0.)), Some(0));
        assert_eq!(
            edge_fraction(&d, Position::Left, (0., 5000.)),
            Some(u16::MAX)
        );
    }

    #[test]
    fn no_usable_display() {
        assert_eq!(edge_fraction(&[], Position::Left, (0., 0.)), None);
        let empty = [rect(0., 0., 0., 1080.)];
        assert_eq!(edge_fraction(&empty, Position::Left, (0., 0.)), None);
    }

    fn range(start: u8, end: u8) -> Option<EdgeRange> {
        Some(EdgeRange::new(start, end).expect("valid range"))
    }

    #[test]
    fn range_bounds_are_validated() {
        assert!(EdgeRange::new(0, 100).is_some());
        assert!(EdgeRange::new(99, 100).is_some());
        assert_eq!(EdgeRange::new(50, 50), None);
        assert_eq!(EdgeRange::new(60, 40), None);
        assert_eq!(EdgeRange::new(0, 101), None);
    }

    #[test]
    fn range_check_boundaries() {
        let edge = (0., 1080.);
        // the whole edge, with or without an explicit range
        for r in [None, range(0, 100)] {
            assert!(in_range(edge, 0., r));
            assert!(in_range(edge, 1079., r));
        }
        // start inclusive, end exclusive
        assert!(in_range(edge, 0., range(0, 50)));
        assert!(in_range(edge, 539., range(0, 50)));
        assert!(!in_range(edge, 540., range(0, 50)));
        assert!(in_range(edge, 540., range(50, 100)));
        assert!(in_range(edge, 1079., range(50, 100)));
        // the megapc smoke: the top 40 % excludes a crossing at y = 600
        assert!(!in_range(edge, 600., range(0, 40)));
        // on a desktop that starts above 0
        assert!(in_range((-200., 1240.), -200., range(0, 10)));
        assert!(!in_range((-200., 1240.), -56., range(0, 10)));
    }

    #[test]
    fn barrier_segments_over_stacked_monitors() {
        let top = rect(0., 0., 1920., 1080.);
        let bottom = rect(0., 1080., 1920., 1080.);
        let d = [top, bottom];
        // the left edge spans y = 0..2160
        let seg = |display: &Rect, r| barrier_segment(&d, display, Position::Left, r);
        assert_eq!(seg(&top, None), Some((0., 1080.)));
        assert_eq!(seg(&top, range(0, 100)), Some((0., 1080.)));
        assert_eq!(seg(&bottom, range(0, 100)), Some((1080., 2160.)));
        // the middle half spans the seam
        assert_eq!(seg(&top, range(25, 75)), Some((540., 1080.)));
        assert_eq!(seg(&bottom, range(25, 75)), Some((1080., 1620.)));
        // a range that ends exactly at the seam misses the lower display
        assert_eq!(seg(&top, range(0, 50)), Some((0., 1080.)));
        assert_eq!(seg(&bottom, range(0, 50)), None);
        assert_eq!(seg(&top, range(50, 100)), None);
        // fractional ends round to the pixels `in_range` accepts
        let (first, end) = seg(&top, range(0, 33)).unwrap();
        assert_eq!((first, end), (0., 713.));
        assert!(in_range((0., 2160.), 712., range(0, 33)));
        assert!(!in_range((0., 2160.), 713., range(0, 33)));
    }

    #[test]
    fn barrier_segments_side_by_side_with_offset() {
        let primary = rect(0., 0., 1920., 1080.);
        let secondary = rect(1920., -200., 2560., 1440.);
        let d = [primary, secondary];
        // the top edge spans x = 0..4480; the left half covers the primary
        // and 320 px of the secondary
        let seg = |display: &Rect| barrier_segment(&d, display, Position::Top, range(0, 50));
        assert_eq!(seg(&primary), Some((0., 1920.)));
        assert_eq!(seg(&secondary), Some((1920., 2240.)));
        // the right edge spans y = -200..1240: 10..20 % is y = -56..88
        let seg = |display: &Rect| barrier_segment(&d, display, Position::Right, range(10, 20));
        assert_eq!(seg(&secondary), Some((-56., 88.)));
        assert_eq!(seg(&primary), Some((0., 88.)));
        assert_eq!(
            barrier_segment(&[], &primary, Position::Left, range(0, 50)),
            None
        );
    }

    #[test]
    fn focus_point_is_mid_range_on_the_outer_edge() {
        let d = [rect(0., 0., 1920., 1080.)];
        assert_eq!(focus_point(&d, Position::Left, None), Some((0., 540.)));
        assert_eq!(
            focus_point(&d, Position::Right, range(0, 40)),
            Some((1919., 216.))
        );
        assert_eq!(
            focus_point(&d, Position::Bottom, range(50, 100)),
            Some((1440., 1079.))
        );
        // the right edge of a secondary display 200 px higher: the point
        // lies on the display that reaches furthest right
        let d = [rect(0., 0., 1920., 1080.), rect(1920., -200., 2560., 1440.)];
        assert_eq!(focus_point(&d, Position::Right, None), Some((4479., 520.)));
        assert_eq!(focus_point(&d, Position::Left, None), Some((0., 520.)));
        // the middle of the top edge is on the secondary display
        assert_eq!(focus_point(&d, Position::Top, None), Some((2240., -200.)));
        // a gap along the edge: the nearest display (y = 750 is 250 px
        // from the lower one, 251 from the upper one's last row)
        let d = [rect(0., 0., 1000., 500.), rect(0., 1000., 1000., 500.)];
        assert_eq!(
            focus_point(&d, Position::Left, range(40, 60)),
            Some((0., 1000.))
        );
        assert_eq!(focus_point(&[], Position::Left, None), None);
    }
}

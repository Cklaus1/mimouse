//! Where along an edge of the desktop the cursor crossed.

use crate::Position;

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

/// Range of the desktop's bounding box along edges at `pos`.
fn span(displays: &[Rect], pos: Position) -> Option<(f64, f64)> {
    displays
        .iter()
        .filter(|d| d.width > 0. && d.height > 0.)
        .map(|d| d.along(pos))
        .reduce(|(lo, hi), (start, end)| (lo.min(start), hi.max(end)))
}

/// Coordinate of `point` along edges at `pos`.
fn along(point: (f64, f64), pos: Position) -> f64 {
    match pos {
        Position::Left | Position::Right => point.1,
        Position::Top | Position::Bottom => point.0,
    }
}

/// Where `point` lies along the `pos` edge of the bounding box of
/// `displays`: 0 at the top / left pixel, `u16::MAX` at the bottom / right
/// one. The receiver maps it back the same way (input-emulation
/// `geometry::entry_point`). `None` without a usable display.
pub(crate) fn edge_fraction(displays: &[Rect], pos: Position, point: (f64, f64)) -> Option<u16> {
    let (lo, hi) = span(displays, pos)?;
    let last = hi - 1.;
    if last <= lo {
        return Some(0);
    }
    let fraction = ((along(point, pos) - lo) / (last - lo)).clamp(0., 1.);
    Some((fraction * f64::from(u16::MAX)).round() as u16)
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
}

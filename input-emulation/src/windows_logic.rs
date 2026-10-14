//! Platform-independent part of the Windows emulation, tested on every
//! platform.

/// Which `SendInput` wheel a scroll event turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wheel {
    /// `MOUSEEVENTF_WHEEL`
    Vertical,
    /// `MOUSEEVENTF_HWHEEL`
    Horizontal,
}

/// The wheel and its `mouseData` for a wire scroll `value` on `axis`. The
/// wire is positive for down (axis 0) and right (axis 1); `WHEEL` is
/// positive for up, `HWHEEL` positive for right, so only the vertical sign
/// flips (the capture side reads `WM_MOUSEHWHEEL` unflipped too).
pub(crate) fn wheel(axis: u8, value: i32) -> Option<(Wheel, i32)> {
    match axis {
        0 => Some((Wheel::Vertical, value.saturating_neg())),
        1 => Some((Wheel::Horizontal, value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_vertical_wheel_flips() {
        // one notch down / right on the wire
        assert_eq!(wheel(0, 120), Some((Wheel::Vertical, -120)));
        assert_eq!(wheel(1, 120), Some((Wheel::Horizontal, 120)));
        assert_eq!(wheel(1, -15), Some((Wheel::Horizontal, -15)));
        assert_eq!(wheel(2, 120), None);
    }
}

//! The one place peer input is checked before a backend emulates it: values
//! outside the protocol are dropped, held keys and buttons are tracked per
//! peer (so they can be released when it goes away), and motion or scroll
//! finer than a backend can emulate is kept per peer until it adds up.

use std::collections::HashSet;

use input_event::{Event, KeyboardEvent, PointerEvent};

/// Smallest steps a backend emulates; finer input accumulates per peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Steps {
    /// motion and smooth scroll (`Axis`) only in whole pixels
    pub(crate) whole_pixels: bool,
    /// discrete scroll (`AxisDiscrete120`) only in multiples of this many
    /// 120ths of a wheel notch
    pub(crate) v120: i32,
}

impl Steps {
    /// the backend takes fractional values as they are
    pub(crate) const EXACT: Steps = Steps {
        whole_pixels: false,
        v120: 1,
    };
}

/// Input state of one peer (emulation handle).
#[derive(Debug, Default)]
pub(crate) struct Peer {
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
    /// motion not emulated yet (less than a pixel)
    motion: (f64, f64),
    /// smooth scroll not emulated yet, per axis
    scroll: [f64; 2],
    /// discrete scroll not emulated yet, per axis
    v120: [i32; 2],
}

impl Peer {
    /// What the backend gets for `event`: `None` for values outside the
    /// protocol, a key or button already in that state, or motion and
    /// scroll below one step. A press counts as held from here on; a release
    /// only once [`Peer::emulated`] confirms it reached the backend, so an
    /// interrupted release is repeated by [`Peer::release`].
    pub(crate) fn plan(&mut self, event: Event, steps: Steps) -> Option<Event> {
        match event {
            Event::Pointer(PointerEvent::Motion { time, dx, dy }) => {
                if !(dx.is_finite() && dy.is_finite()) {
                    return None;
                }
                if !steps.whole_pixels {
                    return Some(event);
                }
                let dx = whole(&mut self.motion.0, dx);
                let dy = whole(&mut self.motion.1, dy);
                (dx != 0. || dy != 0.).then_some(Event::Pointer(PointerEvent::Motion {
                    time,
                    dx,
                    dy,
                }))
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                transition(&mut self.buttons, button, pressed(state)?).then_some(event)
            }
            Event::Pointer(PointerEvent::Axis { time, axis, value }) => {
                let remainder = self.scroll.get_mut(usize::from(axis))?;
                if !value.is_finite() {
                    return None;
                }
                if !steps.whole_pixels {
                    return Some(event);
                }
                let value = whole(remainder, value);
                (value != 0.).then_some(Event::Pointer(PointerEvent::Axis { time, axis, value }))
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                let remainder = self.v120.get_mut(usize::from(axis))?;
                let total = remainder.saturating_add(value);
                let value = total / steps.v120 * steps.v120;
                *remainder = total - value;
                (value != 0).then_some(Event::Pointer(PointerEvent::AxisDiscrete120 {
                    axis,
                    value,
                }))
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                transition(&mut self.keys, key, pressed(state.into())?).then_some(event)
            }
            Event::Keyboard(KeyboardEvent::Modifiers { .. }) => Some(event),
        }
    }

    /// `event` (from [`Peer::plan`]) reached the backend.
    pub(crate) fn emulated(&mut self, event: Event) {
        match event {
            Event::Pointer(PointerEvent::Button {
                button, state: 0, ..
            }) => {
                self.buttons.remove(&button);
            }
            Event::Keyboard(KeyboardEvent::Key { key, state: 0, .. }) => {
                self.keys.remove(&key);
            }
            _ => {}
        }
    }

    /// Releases for every held key and button, then a modifier reset (the
    /// peer's XKB state is separate from held keys, e.g. a latched Shift).
    /// Each release counts as emulated once [`Peer::emulated`] says so.
    pub(crate) fn release(&self) -> Vec<Event> {
        let keys = self.keys.iter().map(|&key| {
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state: 0,
            })
        });
        let buttons = self.buttons.iter().map(|&button| {
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            })
        });
        let modifiers = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        keys.chain(buttons).chain([modifiers]).collect()
    }

    /// number of held keys and buttons
    pub(crate) fn held(&self) -> (usize, usize) {
        (self.keys.len(), self.buttons.len())
    }
}

/// Whether a press (`pressed`) or release changes the state of `code` in
/// `held`; a press is recorded right away, a release by [`Peer::emulated`].
fn transition(held: &mut HashSet<u32>, code: u32, pressed: bool) -> bool {
    if pressed {
        held.insert(code)
    } else {
        held.contains(&code)
    }
}

/// a wire key or button state: 0 = released, 1 = pressed, else invalid
fn pressed(state: u32) -> Option<bool> {
    match state {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// The whole part of `remainder + value`, keeping the fraction in
/// `remainder`.
fn whole(remainder: &mut f64, value: f64) -> f64 {
    let total = *remainder + value;
    let whole = total.trunc();
    *remainder = total - whole;
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: Steps = Steps {
        whole_pixels: true,
        v120: 40,
    };

    fn motion(dx: f64, dy: f64) -> Event {
        Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
    }

    fn button(button: u32, state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state,
        })
    }

    fn key(key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state,
        })
    }

    fn axis(axis: u8, value: f64) -> Event {
        Event::Pointer(PointerEvent::Axis {
            time: 0,
            axis,
            value,
        })
    }

    fn v120(axis: u8, value: i32) -> Event {
        Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value })
    }

    /// plans and emulates `events`, returning what the backend got
    fn run(peer: &mut Peer, steps: Steps, events: &[Event]) -> Vec<Event> {
        let mut out = Vec::new();
        for &event in events {
            if let Some(event) = peer.plan(event, steps) {
                peer.emulated(event);
                out.push(event);
            }
        }
        out
    }

    #[test]
    fn values_outside_the_protocol_are_dropped() {
        let mut peer = Peer::default();
        for event in [
            key(30, 2),
            button(0x110, 2),
            axis(2, 1.),
            v120(2, 120),
            motion(f64::NAN, 1.),
            motion(1., f64::INFINITY),
            axis(0, f64::NAN),
        ] {
            assert_eq!(peer.plan(event, Steps::EXACT), None, "{event:?}");
            assert_eq!(peer.plan(event, WHOLE), None, "{event:?}");
        }
        assert_eq!(peer.held(), (0, 0));
    }

    #[test]
    fn buttons_are_tracked_and_released() {
        let mut peer = Peer::default();
        let sent = run(
            &mut peer,
            Steps::EXACT,
            &[
                button(0x110, 1),
                button(0x110, 1),
                key(30, 1),
                button(0x111, 1),
                button(0x111, 0),
            ],
        );
        // the repeated press is dropped
        assert_eq!(
            sent,
            [
                button(0x110, 1),
                key(30, 1),
                button(0x111, 1),
                button(0x111, 0)
            ]
        );
        let release = peer.release();
        assert_eq!(release.len(), 3);
        assert!(release.contains(&button(0x110, 0)));
        assert!(release.contains(&key(30, 0)));
        assert!(matches!(
            release.last(),
            Some(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                ..
            }))
        ));
        for event in release {
            peer.emulated(event);
        }
        assert_eq!(peer.held(), (0, 0));
        assert_eq!(peer.release().len(), 1);
    }

    #[test]
    fn a_release_counts_once_it_was_emulated() {
        let mut peer = Peer::default();
        run(&mut peer, Steps::EXACT, &[key(30, 1)]);
        // the key-up was planned but never reached the backend
        assert_eq!(peer.plan(key(30, 0), Steps::EXACT), Some(key(30, 0)));
        assert_eq!(peer.release()[0], key(30, 0));
        // a release without a press is dropped
        assert_eq!(peer.plan(key(31, 0), Steps::EXACT), None);
        assert_eq!(peer.plan(button(0x110, 0), Steps::EXACT), None);
    }

    #[test]
    fn sub_pixel_motion_adds_up() {
        let mut peer = Peer::default();
        let sent = run(&mut peer, WHOLE, &[motion(0.4, -0.3); 5]);
        // 2.0 / -1.5 in total: two whole pixels right, one up
        let (dx, dy) = sent.iter().fold((0., 0.), |(x, y), e| match e {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => (x + dx, y + dy),
            _ => unreachable!(),
        });
        assert_eq!((dx, dy), (2., -1.));
        for event in &sent {
            let Event::Pointer(PointerEvent::Motion { dx, dy, .. }) = event else {
                unreachable!()
            };
            assert_eq!((dx.fract(), dy.fract()), (0., 0.));
        }
        // exact backends get the fractions as they are
        assert_eq!(
            run(&mut Peer::default(), Steps::EXACT, &[motion(0.4, -0.3)]),
            [motion(0.4, -0.3)]
        );
    }

    #[test]
    fn small_scroll_steps_add_up_per_axis() {
        let mut peer = Peer::default();
        // precision wheel: 15/120 per event, the backend scrolls in 40s
        let sent = run(&mut peer, WHOLE, &[v120(0, 15); 8]);
        assert_eq!(sent, [v120(0, 40), v120(0, 40), v120(0, 40)]);
        // the other axis keeps its own remainder
        assert_eq!(run(&mut peer, WHOLE, &[v120(1, -30), v120(0, 10)]), []);
        assert_eq!(run(&mut peer, WHOLE, &[v120(1, -10)]), [v120(1, -40)]);
        assert_eq!(
            run(
                &mut peer,
                WHOLE,
                &[axis(0, 0.6), axis(0, 0.6), axis(1, -2.5)]
            ),
            [axis(0, 1.), axis(1, -2.)]
        );
        // hostile values cannot overflow the remainder
        let sent = run(
            &mut Peer::default(),
            WHOLE,
            &[
                v120(0, 39),
                v120(0, i32::MAX),
                v120(1, -39),
                v120(1, i32::MIN),
            ],
        );
        assert_eq!(sent.len(), 2);
    }
}

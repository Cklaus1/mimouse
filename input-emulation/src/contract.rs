//! The contract every emulation backend keeps under [`InputEmulation`] (T6),
//! checked on each backend that runs without a desktop, with wire traces
//! recorded from peers (`tests/fixtures/emulation/*.trace`). Real backends
//! replay the same traces in the two-machine smoke test.
//!
//! Trace format: one wire event per line, `#` starts a comment.
//! `motion <dx> <dy>`, `button <code> <state>`, `axis <axis> <value>`,
//! `v120 <axis> <value>`, `key <code> <state>`,
//! `mods <depressed> <latched> <locked> <group>`.

use std::collections::{HashMap, HashSet};

use futures::FutureExt;
use input_event::{BTN_LEFT, Event, KeyboardEvent, PointerEvent};

use crate::dummy::DummyEmulation;
use crate::normalize::Steps;
use crate::testing::{Call, Calls, Fake, Recorder};
use crate::{Edge, Emulation, EmulationHandle, InputEmulation};

const TRACES: &[(&str, &str)] = &[
    (
        "drag",
        include_str!("../../tests/fixtures/emulation/drag.trace"),
    ),
    (
        "typing",
        include_str!("../../tests/fixtures/emulation/typing.trace"),
    ),
    (
        "scroll",
        include_str!("../../tests/fixtures/emulation/scroll.trace"),
    ),
    (
        "malformed",
        include_str!("../../tests/fixtures/emulation/malformed.trace"),
    ),
    (
        "extreme",
        include_str!("../../tests/fixtures/emulation/extreme.trace"),
    ),
];

/// the events of a trace
fn parse(trace: &str) -> Vec<Event> {
    trace
        .lines()
        .map(|line| line.split('#').next().unwrap().trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let num = |i: usize| words[i].parse::<f64>().unwrap();
            let int = |i: usize| words[i].parse::<i64>().unwrap();
            match words[0] {
                "motion" => Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx: num(1),
                    dy: num(2),
                }),
                "button" => Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: int(1) as u32,
                    state: int(2) as u32,
                }),
                "axis" => Event::Pointer(PointerEvent::Axis {
                    time: 0,
                    axis: int(1) as u8,
                    value: num(2),
                }),
                "v120" => Event::Pointer(PointerEvent::AxisDiscrete120 {
                    axis: int(1) as u8,
                    value: int(2) as i32,
                }),
                "key" => Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: int(1) as u32,
                    state: int(2) as u8,
                }),
                "mods" => Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: int(1) as u32,
                    latched: int(2) as u32,
                    locked: int(3) as u32,
                    group: int(4) as u32,
                }),
                other => panic!("unknown trace event {other:?}"),
            }
        })
        .collect()
}

/// A backend the suite runs on.
struct Subject {
    name: &'static str,
    /// whether it can place the pointer where a cursor enters
    places_pointer: bool,
    make: fn() -> Box<dyn Emulation>,
}

fn fake(steps: Steps, places_pointer: bool) -> Box<dyn Emulation> {
    Box::new(Fake {
        steps,
        places_pointer,
        stall_at: None,
    })
}

const SUBJECTS: &[Subject] = &[
    Subject {
        name: "dummy",
        places_pointer: false,
        make: || Box::new(DummyEmulation::new()),
    },
    Subject {
        name: "fake, exact steps",
        places_pointer: true,
        make: || fake(Steps::EXACT, true),
    },
    // as X11: whole pixels, whole wheel notches
    Subject {
        name: "fake, notch steps",
        places_pointer: false,
        make: || {
            let steps = Steps {
                whole_pixels: true,
                v120: 120,
            };
            fake(steps, false)
        },
    },
    // as macOS: whole pixels, whole wheel lines
    Subject {
        name: "fake, line steps",
        places_pointer: true,
        make: || {
            let steps = Steps {
                whole_pixels: true,
                v120: input_event::macos_scroll::V120_PER_LINE,
            };
            fake(steps, true)
        },
    },
    // a compositor without outputs: nowhere to place the pointer
    #[cfg(wlroots)]
    Subject {
        name: "wlroots",
        places_pointer: false,
        make: || Box::new(crate::wlroots::tests::on_fake_compositor()),
    },
];

impl Subject {
    /// the backend under the layer, the calls that reach it and its steps
    fn start(&self) -> (InputEmulation, Calls, Steps) {
        let backend = (self.make)();
        let steps = backend.steps();
        let (emulation, calls) = Recorder::wrap(backend);
        (emulation, calls, steps)
    }
}

/// consumes every event for each of `handles` in turn
async fn feed(emulation: &mut InputEmulation, events: &[Event], handles: &[EmulationHandle]) {
    for &event in events {
        for &handle in handles {
            emulation.consume(event, handle).await.unwrap();
        }
    }
}

/// a value the protocol allows, in a step the backend emulates
fn conforms(event: Event, steps: Steps) -> bool {
    let pixels = |v: f64| v.is_finite() && (!steps.whole_pixels || v.fract() == 0.);
    match event {
        Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => pixels(dx) && pixels(dy),
        Event::Pointer(PointerEvent::Button { state, .. }) => state <= 1,
        Event::Pointer(PointerEvent::Axis { axis, value, .. }) => axis <= 1 && pixels(value),
        Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
            axis <= 1 && value % steps.v120 == 0
        }
        Event::Keyboard(KeyboardEvent::Key { state, .. }) => state <= 1,
        Event::Keyboard(KeyboardEvent::Modifiers { .. }) => true,
    }
}

/// Checks the calls that reached a backend: every event conforms, a
/// handle is destroyed (and the backend terminated) only with nothing held,
/// and nothing follows `Terminate`.
fn assert_contract(context: &str, calls: &[Call], steps: Steps) {
    let mut held: HashMap<EmulationHandle, HashSet<(&str, u32)>> = HashMap::new();
    let mut terminated = false;
    for call in calls {
        assert!(!terminated, "{context}: {call:?} after terminate");
        match *call {
            Call::Consume(handle, event) => {
                assert!(
                    conforms(event, steps),
                    "{context}: {event:?} reached the backend"
                );
                let held = held.entry(handle).or_default();
                let (code, state) = match event {
                    Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                        (("button", button), state)
                    }
                    Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                        (("key", key), state.into())
                    }
                    _ => continue,
                };
                if state == 1 {
                    held.insert(code);
                } else {
                    held.remove(&code);
                }
            }
            Call::Destroy(handle) => {
                let left = held.remove(&handle).unwrap_or_default();
                assert!(left.is_empty(), "{context}: {handle} left {left:?} held");
            }
            Call::Terminate => {
                assert!(
                    held.values().all(HashSet::is_empty),
                    "{context}: terminated with {held:?} held"
                );
                terminated = true;
            }
            Call::Create(_) | Call::EnterAt(_) => {}
        }
    }
}

#[tokio::test]
async fn destroy_releases_everything_a_trace_left_held() {
    for subject in SUBJECTS {
        for &(trace, events) in TRACES {
            let context = format!("{} / {trace}", subject.name);
            let (mut emulation, mut calls, steps) = subject.start();
            emulation.create(1).await;
            emulation.create(2).await;
            feed(&mut emulation, &parse(events), &[1, 2]).await;
            emulation.destroy(1).await;
            emulation.destroy(2).await;
            let destroyed = calls.take();
            // a second destroy has nothing left to do
            emulation.destroy(1).await;
            assert_eq!(calls.take(), [], "{context}");
            assert_contract(&context, &destroyed, steps);
            assert!(destroyed.contains(&Call::Destroy(1)), "{context}");
            assert!(destroyed.contains(&Call::Destroy(2)), "{context}");
        }
    }
}

#[tokio::test]
async fn terminate_releases_everything_once_and_ends_input() {
    for subject in SUBJECTS {
        for &(trace, events) in TRACES {
            let context = format!("{} / {trace}", subject.name);
            let (mut emulation, mut calls, steps) = subject.start();
            let events = parse(events);
            emulation.create(1).await;
            emulation.create(2).await;
            feed(&mut emulation, &events, &[1, 2]).await;
            emulation.terminate().await;
            emulation.terminate().await;
            // nothing reaches the backend any more
            assert!(!emulation.create(3).await, "{context}");
            feed(&mut emulation, &events, &[1, 2, 3]).await;
            emulation.enter_at(1, Edge::Left, 0).unwrap();
            assert!(!emulation.push_through(1, Edge::Left, 0).await.unwrap());
            emulation.destroy(1).await;
            let calls = calls.take();
            assert_contract(&context, &calls, steps);
            let terminates = calls.iter().filter(|&c| *c == Call::Terminate).count();
            assert_eq!(terminates, 1, "{context}");
        }
    }
}

#[tokio::test]
async fn malformed_datagrams_never_reach_a_backend() {
    let (_, malformed) = TRACES.iter().find(|(t, _)| *t == "malformed").unwrap();
    for subject in SUBJECTS {
        let (mut emulation, mut calls, _) = subject.start();
        emulation.create(1).await;
        calls.take();
        feed(&mut emulation, &parse(malformed), &[1]).await;
        assert_eq!(calls.take(), [], "{}", subject.name);
    }
}

#[tokio::test]
async fn the_pointer_is_pushed_only_where_it_can_be_placed() {
    for subject in SUBJECTS {
        let (mut emulation, mut calls, steps) = subject.start();
        emulation.create(1).await;
        calls.take();
        let pushed = emulation.push_through(1, Edge::Right, 0).await.unwrap();
        assert_eq!(pushed, subject.places_pointer, "{}", subject.name);
        let calls = calls.take();
        assert_eq!(calls[0], Call::EnterAt(1), "{}", subject.name);
        // motion through the edge only from a placed pointer
        let moved = calls[1..].iter().any(|c| matches!(c, Call::Consume(1, _)));
        assert_eq!(moved, pushed, "{}", subject.name);
        assert_contract(subject.name, &calls, steps);
    }
}

/// A layer on a fake backend whose `consume` number `stall_at` never
/// completes, so the caller drops it.
fn stalling(stall_at: usize) -> (InputEmulation, Calls) {
    Recorder::wrap(Box::new(Fake {
        steps: Steps::EXACT,
        places_pointer: true,
        stall_at: Some(stall_at),
    }))
}

#[tokio::test]
async fn a_dropped_consume_leaves_nothing_held() {
    for &(trace, events) in TRACES {
        let events = parse(events);
        for stall_at in 0..events.len() {
            let context = format!("{trace}, consume {stall_at} dropped");
            let (mut emulation, mut calls) = stalling(stall_at);
            emulation.create(1).await;
            for &event in &events {
                // a stalled call is dropped unfinished
                let _ = emulation.consume(event, 1).now_or_never();
            }
            // the stalled call may be a release instead: destroy again then
            if emulation.destroy(1).now_or_never().is_none() {
                emulation.destroy(1).await;
            }
            assert_contract(&context, &calls.take(), Steps::EXACT);
        }
    }
}

#[tokio::test]
async fn a_dropped_release_is_repeated() {
    let press = Event::Pointer(PointerEvent::Button {
        time: 0,
        button: BTN_LEFT,
        state: 1,
    });
    let release = Event::Pointer(PointerEvent::Button {
        time: 0,
        button: BTN_LEFT,
        state: 0,
    });
    // the press completes, the release on destroy stalls
    let (mut emulation, mut calls) = stalling(1);
    emulation.create(1).await;
    emulation.consume(press, 1).await.unwrap();
    assert!(emulation.destroy(1).now_or_never().is_none());
    emulation.destroy(1).await;
    let calls = calls.take();
    let releases = calls
        .iter()
        .filter(|&c| *c == Call::Consume(1, release))
        .count();
    assert_eq!(releases, 2);
    assert_contract("dropped release", &calls, Steps::EXACT);
}

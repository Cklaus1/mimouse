//! The contract every capture backend keeps under [`InputCapture`], checked
//! on each backend that runs without a desktop: Dummy, layer-shell on a fake
//! compositor, and a scripted [`Fake`] standing in for the OS backends
//! (Windows and macOS: `focus` starts capture, events come through the OS
//! callback queue with its limit; libei: `focus` needs a pointer push,
//! nothing is dropped). libei needs the portal on D-Bus, Windows and macOS
//! their OS, and X11 capture is not implemented, so those real backends run
//! the contract only in the two-machine smoke test. A backend joins the
//! suite through `SUBJECTS`.
//!
//! 1. A crossing (`Begin`) or `End` reported at a position reaches every id
//!    there, in creation order; input reaches only the ids steering there:
//!    enter-only ids only while no Default id shares the position.
//! 2. The barrier at a position covers the ranges of the ids steering there
//!    (an enter-only id alone keeps the whole edge); the backend hears of a
//!    barrier only when it changes and loses it with its last id.
//! 3. A destroyed id gets nothing more, copies queued before included.
//!    Destroying an unknown id does nothing.
//! 4. `release()` drops the input queued for every id and releases the
//!    backend. Input the backend still yields from the ended capture (an
//!    OS queue, a compositor socket) is dropped until the next `Begin`.
//! 5. A report at a position without ids is skipped; the next one arrives.
//! 6. A focus starts capture as if the cursor crossed within the id's own
//!    range; the next `Begin` at its position is for that id alone, once.
//!    Backends that cannot grab on their own answer `FocusByPointerPush`
//!    and the `Begin` of the pushed pointer is the focused one.
//! 7. A focus that cannot begin is forgotten: an unknown id (the backend is
//!    not asked), a failed focus, `clear_focus`, the focused id destroyed.
//! 8. Hotkeys reach the backend by position, for ids with a barrier only,
//!    and follow the barriers as ids come and go.
//! 9. An overflowing OS callback queue never loses a key or button release,
//!    a modifier update or an `End`, for any id.
//! 10. A backend error or the end of its stream comes after the queued
//!     copies of earlier reports.
//! 11. The keys reported down and not up are handed to the caller once.
//! 12. After `terminate` the stream ends, queued copies included.
//! 13. While an enter-only id is returning (`set_returning`), a `Begin` at
//!     its position is for it alone; input still reaches the steering ids.
//!     Only enter-only ids can be returning; unmarking (or destroying) the
//!     id makes crossings everyone's again.

use std::collections::HashSet;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use input_event::scancode::Linux::{Key1, Key2, KeyA, KeyB, KeyLeftCtrl};
use input_event::{BTN_LEFT, Event, KeyboardEvent, PointerEvent};
use tokio::task::unconstrained;
use tokio::time::timeout;

use crate::dummy::DummyInputCapture;
use crate::testing::{Call, Calls, Fake, Focus, Recorder, Script};
use crate::{
    Capture, CaptureError, CaptureEvent, CaptureHandle, Chord, EdgeRange, InputCapture, Position,
};
use CaptureEvent::{Begin, End, Input};
use Position::{Bottom, Left, Right, Top};

/// the OS callback queue limits (windows.rs, macos.rs)
const WINDOWS_QUEUE: usize = 10;
const MACOS_QUEUE: usize = 32;

/// A backend the suite runs on.
struct Subject {
    name: &'static str,
    /// `focus` starts capture itself; otherwise it answers
    /// `FocusByPointerPush`
    focuses: bool,
    /// it can report where no barrier is (layer-shell: only a barrier's
    /// surface gets input)
    reports_anywhere: bool,
    start: fn() -> (Box<dyn Capture + Send>, Driver),
}

/// How a test makes a subject report events.
enum Driver {
    /// Dummy: reports `Begin`, then motion, at the left edge by itself
    Itself,
    Script(Script),
    #[cfg(layer_shell)]
    Compositor(crate::layer_shell::tests::Compositor),
}

impl Driver {
    /// makes the backend report `events` at `pos`; false if it reports only
    /// by itself
    fn report(&mut self, pos: Position, events: &[CaptureEvent]) -> bool {
        match self {
            Driver::Itself => return false,
            Driver::Script(script) => {
                for &event in events {
                    // a full queue may drop it, as the OS backends' do
                    script.report(pos, event);
                }
            }
            #[cfg(layer_shell)]
            Driver::Compositor(compositor) => {
                for &event in events {
                    compositor.report(pos, event);
                }
            }
        }
        true
    }
}

fn fake(focus: Focus, limit: usize) -> (Box<dyn Capture + Send>, Driver) {
    let (fake, script) = Fake::new(focus, limit);
    (Box::new(fake), Driver::Script(script))
}

const SUBJECTS: &[Subject] = &[
    Subject {
        name: "dummy",
        focuses: false,
        reports_anywhere: true,
        start: || (Box::new(DummyInputCapture::new()), Driver::Itself),
    },
    Subject {
        name: "fake as windows",
        focuses: true,
        reports_anywhere: true,
        start: || fake(Focus::Starts, WINDOWS_QUEUE),
    },
    Subject {
        name: "fake as macos",
        focuses: true,
        reports_anywhere: true,
        start: || fake(Focus::Starts, MACOS_QUEUE),
    },
    // libei's channel applies backpressure: nothing is dropped
    Subject {
        name: "fake as libei",
        focuses: false,
        reports_anywhere: true,
        start: || fake(Focus::ByPointerPush, usize::MAX),
    },
    #[cfg(layer_shell)]
    Subject {
        name: "layer-shell",
        focuses: false,
        reports_anywhere: false,
        start: || {
            let (capture, compositor) = crate::layer_shell::tests::on_fake_compositor();
            (Box::new(capture), Driver::Compositor(compositor))
        },
    },
];

impl Subject {
    fn start(&self) -> Run {
        let (backend, driver) = (self.start)();
        Run::new(self.name, backend, driver)
    }
}

/// A backend under the layer, and what passed between them.
struct Run {
    name: &'static str,
    capture: InputCapture,
    calls: Calls,
    driver: Driver,
    /// every call and report so far
    log: Vec<Call>,
    /// the live ids in creation order: id, position, enter-only
    ids: Vec<(CaptureHandle, Position, bool)>,
}

impl Run {
    fn new(name: &'static str, backend: Box<dyn Capture + Send>, driver: Driver) -> Self {
        let (capture, calls) = Recorder::wrap(backend);
        Self {
            name,
            capture,
            calls,
            driver,
            log: Vec::new(),
            ids: Vec::new(),
        }
    }

    async fn create(
        &mut self,
        id: CaptureHandle,
        pos: Position,
        range: Option<EdgeRange>,
        enter_only: bool,
    ) {
        self.capture
            .create(id, pos, range, enter_only)
            .await
            .unwrap();
        self.ids.push((id, pos, enter_only));
    }

    async fn destroy(&mut self, id: CaptureHandle) {
        self.capture.destroy(id).await.unwrap();
        self.ids.retain(|&(i, ..)| i != id);
    }

    fn report(&mut self, pos: Position, events: &[CaptureEvent]) -> bool {
        self.driver.report(pos, events)
    }

    /// marks the log: [`Self::since`] the calls from here on
    fn mark(&mut self) -> usize {
        self.log.extend(self.calls.take());
        self.log.len()
    }

    fn since(&mut self, mark: usize) -> &[Call] {
        self.log.extend(self.calls.take());
        &self.log[mark..]
    }

    fn reports_since(&mut self, mark: usize) -> Vec<(Position, CaptureEvent)> {
        let reports = self.since(mark).iter().filter_map(|call| match *call {
            Call::Report(pos, event) => Some((pos, event)),
            _ => None,
        });
        reports.collect()
    }

    /// the barriers created and destroyed since `mark`
    fn barriers_since(&mut self, mark: usize) -> Vec<Call> {
        let barriers = self
            .since(mark)
            .iter()
            .filter(|call| matches!(call, Call::Create(..) | Call::Destroy(_)));
        barriers.cloned().collect()
    }

    /// the hotkeys the backend has now
    fn hotkeys(&mut self) -> Vec<(Chord, Position)> {
        let calls = self.since(0).iter().rev();
        let mut hotkeys = calls.filter_map(|call| match call {
            Call::SetHotkeys(hotkeys) => Some(hotkeys.clone()),
            _ => None,
        });
        hotkeys.next().unwrap_or_default()
    }

    async fn next(&mut self) -> (CaptureHandle, CaptureEvent) {
        let next = timeout(Duration::from_secs(5), self.capture.next()).await;
        let name = self.name;
        let next = next.unwrap_or_else(|_| panic!("{name}: no event"));
        let next = next.unwrap_or_else(|| panic!("{name}: the stream ended"));
        next.unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// Yields until the reports since `mark`, at least `reports` of them,
    /// reached their ids, checking each reached exactly those (rules 1, 4).
    async fn deliver(&mut self, mark: usize, reports: usize) -> Vec<(CaptureHandle, CaptureEvent)> {
        let mut yielded = Vec::new();
        loop {
            let calls = self.since(mark).to_vec();
            let reported = calls
                .iter()
                .filter(|c| matches!(c, Call::Report(..)))
                .count();
            let expected = delivered(&calls, &self.ids);
            assert!(
                expected.starts_with(&yielded),
                "{}: yielded {yielded:?} for {calls:?}",
                self.name
            );
            if reported >= reports && yielded.len() == expected.len() {
                return yielded;
            }
            yielded.push(self.next().await);
        }
    }
}

/// What the caller gets for the reports among `calls` while `ids` (id,
/// position, enter-only; in creation order) live: a crossing or end reaches
/// every id at its position, input the ids steering there, that is
/// enter-only ids only while no Default id is there; input reported after a
/// release and before the next crossing reaches nobody.
fn delivered(
    calls: &[Call],
    ids: &[(CaptureHandle, Position, bool)],
) -> Vec<(CaptureHandle, CaptureEvent)> {
    let mut delivered = Vec::new();
    let mut released = false;
    for call in calls {
        let (pos, event) = match *call {
            Call::Report(pos, event) => (pos, event),
            Call::Release => {
                released = true;
                continue;
            }
            _ => continue,
        };
        match event {
            Begin(_) => released = false,
            Input(_) if released => continue,
            _ => {}
        }
        let here = ids.iter().filter(|&&(_, p, _)| p == pos);
        let shared = here.clone().any(|&(.., enter_only)| !enter_only);
        for &(id, _, enter_only) in here {
            if !(matches!(event, Input(_)) && shared && enter_only) {
                delivered.push((id, event));
            }
        }
    }
    delivered
}

const fn key(key: input_event::scancode::Linux, state: u8) -> CaptureEvent {
    Input(Event::Keyboard(KeyboardEvent::Key {
        time: 0,
        key: key as u32,
        state,
    }))
}

const fn button(state: u32) -> CaptureEvent {
    Input(Event::Pointer(PointerEvent::Button {
        time: 0,
        button: BTN_LEFT,
        state,
    }))
}

/// a capture from crossing to end
const CROSSING: [CaptureEvent; 4] = [Begin(None), key(KeyA, 1), key(KeyA, 0), End];

/// rule 1
#[tokio::test]
async fn each_report_reaches_the_ids_at_its_position() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, EdgeRange::new(0, 40), false).await;
        run.create(2, Left, None, true).await;
        run.create(3, Right, None, false).await;
        // beside a Default id, then alone (the Default id gone)
        for steering in [1, 2] {
            if steering == 2 {
                run.destroy(1).await;
            }
            let mark = run.mark();
            run.report(Left, &CROSSING);
            let yielded = run.deliver(mark, CROSSING.len()).await;
            let input = yielded.iter().filter(|(_, e)| matches!(e, Input(_)));
            let steered: HashSet<_> = input.map(|&(id, _)| id).collect();
            assert_eq!(steered, HashSet::from([steering]), "{}", subject.name);
        }
    }
}

/// rule 2
#[tokio::test]
async fn a_barrier_covers_the_ids_steering_there() {
    let range = EdgeRange::new;
    enum Op {
        Create(CaptureHandle, Position, Option<EdgeRange>, bool),
        Destroy(CaptureHandle),
    }
    let steps = [
        (
            Op::Create(1, Left, range(0, 40), false),
            vec![Call::Create(Left, range(0, 40))],
        ),
        // an enter-only id beside a Default one leaves it alone
        (Op::Create(2, Left, None, true), vec![]),
        // it covers every Default id there
        (
            Op::Create(3, Left, range(30, 70), false),
            vec![Call::Create(Left, range(0, 70))],
        ),
        (Op::Create(4, Left, range(10, 20), false), vec![]),
        (
            Op::Create(5, Right, None, false),
            vec![Call::Create(Right, None)],
        ),
        (Op::Destroy(4), vec![]),
        (Op::Destroy(1), vec![Call::Create(Left, range(30, 70))]),
        // an enter-only id alone keeps the whole edge
        (Op::Destroy(3), vec![Call::Create(Left, None)]),
        (Op::Destroy(2), vec![Call::Destroy(Left)]),
        (Op::Destroy(5), vec![Call::Destroy(Right)]),
    ];
    for subject in SUBJECTS {
        let mut run = subject.start();
        for (i, (op, expected)) in steps.iter().enumerate() {
            let mark = run.mark();
            match *op {
                Op::Create(id, pos, range, enter_only) => {
                    run.create(id, pos, range, enter_only).await
                }
                Op::Destroy(id) => run.destroy(id).await,
            }
            assert_eq!(
                &run.barriers_since(mark),
                expected,
                "{} step {i}",
                subject.name
            );
        }
    }
}

/// rule 3
#[tokio::test]
async fn a_destroyed_id_gets_nothing_more() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, false).await;
        // Dummy: its own `Begin`, then motion
        run.report(Left, &[Begin(None), key(KeyA, 1), key(KeyA, 0)]);
        // id 1 gets the crossing, id 2's copy is queued
        let (id, event) = run.next().await;
        assert!(id == 1 && matches!(event, Begin(_)), "{}", subject.name);
        run.destroy(2).await;
        let mark = run.mark();
        // what follows is for id 1 alone
        run.deliver(mark, 2).await;
    }
}

/// rule 4
#[tokio::test]
async fn release_drops_the_input_of_the_ended_capture() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, false).await;
        run.report(Left, &[Begin(None), key(KeyA, 1)]);
        for expected in [1, 2, 1] {
            assert_eq!(run.next().await.0, expected, "{}", subject.name);
        }
        // id 2's copy of the input is queued
        let mark = run.mark();
        run.capture.release().await.unwrap();
        assert_eq!(run.since(mark), [Call::Release], "{}", subject.name);
        // the backend still holds input of the ended capture
        let reports = [key(KeyA, 0), Begin(None), key(KeyB, 1)];
        if !run.report(Left, &reports) {
            // Dummy's motion goes on without a new crossing
            let none = timeout(Duration::from_millis(100), run.capture.next()).await;
            assert!(none.is_err(), "{}: {none:?}", subject.name);
            continue;
        }
        let mut yielded = Vec::new();
        for _ in 0..4 {
            // layer-shell reports where the pointer entered
            yielded.push(match run.next().await {
                (id, Begin(_)) => (id, Begin(None)),
                next => next,
            });
        }
        let expected = [
            (1, Begin(None)),
            (2, Begin(None)),
            (1, key(KeyB, 1)),
            (2, key(KeyB, 1)),
        ];
        assert_eq!(yielded, expected, "{}", subject.name);
    }
}

/// rule 5
#[tokio::test(start_paused = true)]
async fn a_report_where_no_id_is_skipped() {
    for subject in SUBJECTS.iter().filter(|s| s.reports_anywhere) {
        let mut run = subject.start();
        run.create(1, Right, None, false).await;
        let mark = run.mark();
        if run.report(Top, &[Begin(None), End]) {
            run.report(Right, &[Begin(None)]);
            assert_eq!(run.next().await, (1, Begin(None)), "{}", subject.name);
            assert_eq!(run.reports_since(mark).len(), 3, "{}", subject.name);
        } else {
            // Dummy reports at the left edge only
            let none = timeout(Duration::from_millis(100), run.capture.next()).await;
            assert!(none.is_err(), "{}: {none:?}", subject.name);
            assert!(!run.reports_since(mark).is_empty(), "{}", subject.name);
            run.create(2, Left, None, false).await;
            assert_eq!(run.next().await.0, 2, "{}", subject.name);
        }
    }
}

/// rule 6
#[tokio::test]
async fn a_focus_begins_for_its_id_alone() {
    let range = EdgeRange::new(50, 100);
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, range, false).await;
        run.create(3, Right, None, false).await;
        let mark = run.mark();
        let focused = run.capture.focus(2).await;
        if subject.focuses {
            focused.unwrap();
        } else {
            let push = matches!(focused, Err(CaptureError::FocusByPointerPush));
            assert!(push, "{}: {focused:?}", subject.name);
            // the pushed pointer crosses (Dummy: its first `Begin`)
            run.report(Left, &[Begin(None)]);
        }
        assert!(
            run.since(mark).contains(&Call::Focus(Left, range)),
            "{}",
            subject.name
        );
        let (id, event) = run.next().await;
        assert!(
            id == 2 && matches!(event, Begin(_)),
            "{}: {id} {event}",
            subject.name
        );
        // once: the next crossing is everyone's
        let mark = run.mark();
        run.report(Left, &CROSSING);
        run.report(Left, &[Begin(None)]);
        run.deliver(mark, CROSSING.len() + 1).await;
    }
}

/// rule 6: a crossing elsewhere is not the focused one
#[tokio::test]
async fn a_focus_waits_for_its_own_position() {
    for subject in SUBJECTS.iter().filter(|s| !s.focuses) {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, false).await;
        run.create(3, Right, None, false).await;
        let focused = run.capture.focus(2).await;
        assert!(matches!(focused, Err(CaptureError::FocusByPointerPush)));
        let mark = run.mark();
        if !run.report(Right, &[Begin(None), End]) {
            // Dummy crosses at the left edge only
            continue;
        }
        run.report(Left, &[Begin(None)]);
        let yielded = [run.next().await, run.next().await, run.next().await];
        let reports = run.reports_since(mark);
        let expected = [(3, reports[0].1), (3, reports[1].1), (2, reports[2].1)];
        assert_eq!(yielded, expected, "{}", subject.name);
    }
}

/// rule 7
#[tokio::test]
async fn a_focus_that_cannot_begin_is_forgotten() {
    let start = |focus| {
        let (backend, driver) = fake(focus, usize::MAX);
        Run::new("fake", backend, driver)
    };
    let mut run = start(Focus::Starts);
    run.create(1, Left, None, false).await;
    let mark = run.mark();
    let focused = run.capture.focus(9).await;
    assert!(
        matches!(focused, Err(CaptureError::NoSuchClient(9))),
        "{focused:?}"
    );
    assert!(!run.since(mark).iter().any(|c| matches!(c, Call::Focus(..))));

    for case in ["failed", "cleared", "destroyed"] {
        let focus = match case {
            "failed" => Focus::Fails,
            _ => Focus::ByPointerPush,
        };
        let mut run = start(focus);
        for id in 1..=3 {
            run.create(id, Left, None, false).await;
        }
        let focused = run.capture.focus(2).await;
        assert!(focused.is_err(), "{case}");
        match case {
            "cleared" => run.capture.clear_focus(),
            // a new client under the old id is not the focused one
            "destroyed" => {
                run.destroy(2).await;
                run.create(2, Left, None, false).await;
            }
            _ => {}
        }
        // the next crossing is everyone's
        let mark = run.mark();
        run.report(Left, &[Begin(None)]);
        let yielded = run.deliver(mark, 1).await;
        assert!(yielded.len() >= 2, "{case}: {yielded:?}");
    }
}

/// rule 8
#[tokio::test]
async fn hotkeys_follow_the_barriers() {
    let chord = |key| Chord::new([KeyLeftCtrl, key]).unwrap();
    let (a, b) = (chord(Key1), chord(Key2));
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        // id 5 has no barrier yet
        let hotkeys = vec![(a.clone(), 1), (b.clone(), 5)];
        run.capture.set_hotkeys(hotkeys).await.unwrap();
        assert_eq!(run.hotkeys(), [(a.clone(), Left)], "{}", subject.name);
        run.create(5, Bottom, None, false).await;
        let both = [(a.clone(), Left), (b.clone(), Bottom)];
        assert_eq!(run.hotkeys(), both, "{}", subject.name);
        run.destroy(1).await;
        assert_eq!(run.hotkeys(), [(b.clone(), Bottom)], "{}", subject.name);
    }
}

/// rule 9: the OS callback sends while the capture task is busy
#[tokio::test]
async fn a_flooded_os_queue_keeps_releases_and_ends() {
    let motion = Input(Event::Pointer(PointerEvent::Motion {
        time: 0,
        dx: 1.,
        dy: 0.,
    }));
    let modifiers = Input(Event::Keyboard(KeyboardEvent::Modifiers {
        depressed: 0,
        latched: 0,
        locked: 0,
        group: 0,
    }));
    let releases = [key(KeyA, 0), button(0), modifiers, End];
    let mut sent = vec![Begin(None), key(KeyA, 1), button(1)];
    sent.extend([motion; 100]);
    sent.extend(releases);
    for subject in SUBJECTS.iter().filter(|s| s.name.starts_with("fake as")) {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, true).await;
        run.report(Left, &sent);
        let mut yielded = Vec::new();
        // unconstrained: tokio's coop budget would end the drain early
        while let Some(next) = unconstrained(run.capture.next()).now_or_never() {
            yielded.push(next.unwrap().unwrap());
        }
        let got = |id| -> Vec<_> {
            let events = yielded.iter().filter(|&&(i, _)| i == id);
            events.map(|&(_, e)| e).collect()
        };
        let mut rest = sent.iter();
        let in_order = got(1).iter().all(|e| rest.any(|s| s == e));
        assert!(in_order, "{}: {:?}", subject.name, got(1));
        assert!(
            got(1).ends_with(&releases),
            "{}: {:?}",
            subject.name,
            got(1)
        );
        assert_eq!(got(2), [Begin(None), End], "{}", subject.name);
        let pressed = run.capture.take_pressed_keys();
        assert!(pressed.is_empty(), "{}: {pressed:?}", subject.name);
        // non-vacuous only if the queue overflowed
        if subject.name != "fake as libei" {
            assert!(got(1).len() < sent.len(), "{}", subject.name);
        }
    }
}

/// rule 10
#[tokio::test]
async fn queued_copies_precede_a_backend_error_or_end() {
    for fails in [true, false] {
        let (fake, script) = Fake::new(Focus::ByPointerPush, usize::MAX);
        let mut capture = InputCapture::with_backend(Box::new(fake));
        for id in [1, 2] {
            capture.create(id, Left, None, false).await.unwrap();
        }
        script.report(Left, End);
        let next = capture.next().await.unwrap().unwrap();
        assert_eq!(next, (1, End));
        if fails {
            script.fail(CaptureError::EndOfStream);
        } else {
            drop(script);
        }
        let next = capture.next().await.unwrap().unwrap();
        assert_eq!(next, (2, End));
        let last = capture.next().await;
        match fails {
            true => assert!(matches!(last, Some(Err(CaptureError::EndOfStream)))),
            false => assert!(last.is_none()),
        }
    }
}

/// rule 11
#[tokio::test]
async fn keys_left_down_are_handed_over_once() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, true).await;
        let mark = run.mark();
        let typed = [Begin(None), key(KeyA, 1), key(KeyB, 1), key(KeyB, 0)];
        if !run.report(Left, &typed) {
            // Dummy reports no keys
            continue;
        }
        run.deliver(mark, typed.len()).await;
        let pressed = run.capture.take_pressed_keys();
        assert_eq!(pressed, HashSet::from([KeyA]), "{}", subject.name);
        let pressed = run.capture.take_pressed_keys();
        assert!(pressed.is_empty(), "{}: {pressed:?}", subject.name);
    }
}

/// rule 12
#[tokio::test]
async fn terminate_ends_the_stream() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, false).await;
        // Dummy reports by itself
        run.report(Left, &[Begin(None), key(KeyA, 1)]);
        // id 2's copy is queued
        assert_eq!(run.next().await.0, 1, "{}", subject.name);
        let mark = run.mark();
        run.capture.terminate().await.unwrap();
        assert_eq!(run.since(mark), [Call::Terminate], "{}", subject.name);
        let next = timeout(Duration::from_secs(5), run.capture.next()).await;
        assert!(matches!(next, Ok(None)), "{}: {next:?}", subject.name);
    }
}

/// rule 3
#[tokio::test]
async fn destroying_an_unknown_id_does_nothing() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        let mark = run.mark();
        run.destroy(2).await;
        run.destroy(2).await;
        let calls = run.since(mark);
        assert!(calls.is_empty(), "{}: {calls:?}", subject.name);
        run.destroy(1).await;
        run.destroy(1).await;
        assert_eq!(
            run.barriers_since(mark),
            [Call::Destroy(Left)],
            "{}",
            subject.name
        );
    }
}

/// rule 13
#[tokio::test]
async fn a_crossing_back_to_the_controlling_peer_is_its_alone() {
    for subject in SUBJECTS {
        let mut run = subject.start();
        run.create(1, Left, None, false).await;
        run.create(2, Left, None, true).await;
        // a Default id cannot be returning
        run.capture.set_returning(1, true);
        run.capture.set_returning(2, true);
        // Dummy reports by itself
        let scripted = run.report(Left, &[Begin(None), key(KeyA, 1)]);
        let (id, event) = run.next().await;
        assert!(
            id == 2 && matches!(event, Begin(_)),
            "{}: {id} {event}",
            subject.name
        );
        if !scripted {
            continue;
        }
        // no copy of the crossing for id 1, which still steers
        assert_eq!(run.next().await, (1, key(KeyA, 1)), "{}", subject.name);
        run.capture.set_returning(2, false);
        let mark = run.mark();
        run.report(Left, &CROSSING);
        run.deliver(mark, CROSSING.len()).await;
    }
}

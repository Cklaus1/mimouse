use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    future::Future,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, scancode};

pub use chord::{Chord, ChordMatcher};
pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod chord;
pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;
#[cfg(any(libei, layer_shell, windows, target_os = "macos", test))]
mod geometry;

pub type CaptureHandle = u64;

/// `dwExtraInfo` of the zero-length mouse move that mimouse injects on
/// Windows every 30 s while capturing, to reset the idle timer. The capture
/// hook passes these moves through instead of swallowing them.
pub const IDLE_KEEPALIVE_TAG: usize = 0x4D69_4D6F; // "MiMo"

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active; where the cursor
    /// crossed the edge (0 = top / left end, `u16::MAX` = the other end),
    /// if the backend knows
    Begin(Option<u16>),
    /// input event coming from capture handle
    Input(Event),
    /// the compositor or OS ended an active capture without a release
    /// request (e.g. GNOME deactivated the session)
    End,
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin(None) => write!(f, "begin capture"),
            CaptureEvent::Begin(Some(at)) => write!(f, "begin capture at {at}"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
            CaptureEvent::End => write!(f, "capture ended"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// The part of an edge that switches to a client, in percent of the edge's
/// length (the desktop's bounding box along that side): `start < end <= 100`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EdgeRange {
    start: u8,
    end: u8,
}

impl EdgeRange {
    /// `None` unless `start < end <= 100`
    pub fn new(start: u8, end: u8) -> Option<Self> {
        (start < end && end <= 100).then_some(Self { start, end })
    }

    pub fn start(&self) -> u8 {
        self.start
    }

    pub fn end(&self) -> u8 {
        self.end
    }

    /// The smallest range covering both; `None` (the whole edge) covers all.
    /// Clients sharing an edge share one barrier, which covers all of them.
    fn hull(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        let (a, b) = (a?, b?);
        Some(Self {
            start: a.start.min(b.start),
            end: a.end.max(b.end),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

/// the backend an [`InputCapture`] runs on and how it was chosen
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selected {
    pub backend: Backend,
    /// tried in order because none was configured
    pub auto: bool,
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position, range and whether it is enter-only
    id_map: HashMap<CaptureHandle, (Position, Option<EdgeRange>, bool)>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
    /// client hotkeys the backend starts capture with while input is local
    hotkeys: Vec<(Chord, CaptureHandle)>,
    /// the client a requested focus goes to: the next `Begin` at its
    /// position is for it alone
    focus: Option<CaptureHandle>,
    /// `None`: a backend injected through [`InputCapture::with_backend`]
    selected: Option<Selected>,
}

impl InputCapture {
    /// create a new client with the given id; `range`: the part of the edge
    /// that activates it (`None`: all of it); `enter_only`: it only wants
    /// to see crossings (an inbound connection), so a normal client at the
    /// same position alone shapes the barrier and gets the input
    pub async fn create(
        &mut self,
        id: CaptureHandle,
        pos: Position,
        range: Option<EdgeRange>,
        enter_only: bool,
    ) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        let old = self
            .position_map
            .contains_key(&pos)
            .then(|| self.barrier_range(pos));
        self.id_map.insert(id, (pos, range, enter_only));
        self.position_map.entry(pos).or_default().push(id);
        let new = self.barrier_range(pos);
        if old != Some(new) {
            self.capture.create(pos, new).await?;
        }
        self.update_hotkeys().await
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let (pos, ..) = *self.id_map.get(&id).expect("no position for this handle");
        let old = self.barrier_range(pos);
        self.id_map.remove(&id);
        if self.focus == Some(id) {
            self.focus = None;
        }
        self.retain_live_pending();

        log::debug!("destroying capture {id} @ {pos}");
        let remaining = self.position_map.get_mut(&pos).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        } else {
            let new = self.barrier_range(pos);
            if new != old {
                self.capture.create(pos, new).await?;
            }
        }
        self.update_hotkeys().await
    }

    /// Starts capture for `id` without a crossing (a hotkey or `cli focus`);
    /// the backend reports `Begin` for it. Backends that cannot start
    /// capture themselves return [`CaptureError::FocusByPointerPush`]: the
    /// caller then moves the pointer through the barrier, and the `Begin`
    /// that follows is for `id`.
    pub async fn focus(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(&(pos, range, _)) = self.id_map.get(&id) else {
            return Err(CaptureError::NoSuchClient(id));
        };
        self.focus = Some(id);
        let result = self.capture.focus(pos, range).await;
        if let Err(e) = &result {
            if !matches!(e, CaptureError::FocusByPointerPush) {
                self.focus = None;
            }
        }
        result
    }

    /// forget a requested focus that did not start capture
    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    /// Hotkeys that start capture for a client while input is local, on
    /// backends that see local keys (Windows, macOS). Clients without a
    /// barrier are ignored until they get one.
    pub async fn set_hotkeys(
        &mut self,
        hotkeys: Vec<(Chord, CaptureHandle)>,
    ) -> Result<(), CaptureError> {
        self.hotkeys = hotkeys;
        self.update_hotkeys().await
    }

    async fn update_hotkeys(&mut self) -> Result<(), CaptureError> {
        let hotkeys = self
            .hotkeys
            .iter()
            .filter_map(|(chord, id)| Some((chord.clone(), self.id_map.get(id)?.0)))
            .collect();
        self.capture.set_hotkeys(hotkeys).await
    }

    /// the range of the barrier at `pos`: covers every steering client there
    fn barrier_range(&self, pos: Position) -> Option<EdgeRange> {
        let mut ranges = ids_at(&self.position_map, &self.id_map, pos, true)
            .map(|id| self.id_map.get(&id).and_then(|&(_, range, _)| range));
        let first = ranges.next()?;
        ranges.fold(first, EdgeRange::hull)
    }

    /// drop queued events of destroyed ids: the caller no longer knows them
    fn retain_live_pending(&mut self) {
        let id_map = &self.id_map;
        self.pending.retain(|(id, _)| id_map.contains_key(id));
    }

    /// release mouse; queued input of the ended capture is dropped, so
    /// nothing reaches a peer after its `Leave`
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        let id_map = &self.id_map;
        self.pending.retain(|(id, event)| {
            id_map.contains_key(id) && !matches!(event, CaptureEvent::Input(_))
        });
        self.capture.release().await
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]; `permission_pending` is `true` while
    /// the backend waits for a permission request (e.g. the portal dialog)
    pub async fn new(
        backend: Option<Backend>,
        permission_pending: tokio::sync::watch::Sender<bool>,
    ) -> Result<Self, CaptureCreationError> {
        let (capture, selected) = select(backend, AUTO_BACKENDS, |backend| {
            create_backend(backend, permission_pending.clone())
        })
        .await?;
        let mut capture = Self::with_backend(capture);
        capture.selected = Some(selected);
        Ok(capture)
    }

    /// an [`InputCapture`] over an injected backend (tests and embedders)
    pub fn with_backend(capture: Box<dyn Capture>) -> Self {
        Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
            hotkeys: Vec::new(),
            focus: None,
            selected: None,
        }
    }

    /// the backend in use and how it was chosen (`None`: injected)
    pub fn selected(&self) -> Option<Selected> {
        self.selected
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        loop {
            let event = ready!(self.capture.poll_next_unpin(cx));

            // stream closed
            let event = match event {
                Some(e) => e,
                None => return Poll::Ready(None),
            };

            // error occurred
            let (pos, event) = match event {
                Ok(e) => e,
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            // handle key presses
            if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) =
                event
            {
                self.update_pressed_keys(key, state);
            }

            // a requested focus: the `Begin` is for the focused client only
            if matches!(event, CaptureEvent::Begin(_)) {
                if let Some(id) = self.focus {
                    if self.id_map.get(&id).is_some_and(|&(p, ..)| p == pos) {
                        self.focus = None;
                        return Poll::Ready(Some(Ok((id, event))));
                    }
                }
            }

            // crossings and ends reach every id, input only the steering ones
            let this = &mut *self;
            let steering = matches!(event, CaptureEvent::Input(_));
            let mut ids = ids_at(&this.position_map, &this.id_map, pos, steering);
            let Some(first) = ids.next() else {
                // no client at this position (destroyed meanwhile): poll
                // again, returning Pending here would lose the wakeup
                continue;
            };
            this.pending.extend(ids.map(|id| (id, event)));
            return Poll::Ready(Some(Ok((first, event))));
        }
    }
}

/// The ids at `pos`; `steering`: only those that shape its barrier and get
/// its input, i.e. enter-only ids only while no other id is there (the way
/// back to the peer that controls this machine).
fn ids_at<'a>(
    position_map: &'a HashMap<Position, Vec<CaptureHandle>>,
    id_map: &'a HashMap<CaptureHandle, (Position, Option<EdgeRange>, bool)>,
    pos: Position,
    steering: bool,
) -> impl Iterator<Item = CaptureHandle> + 'a {
    let ids = position_map.get(&pos).map_or(&[][..], Vec::as_slice);
    let enter_only = |id: &CaptureHandle| id_map.get(id).is_some_and(|&(.., e)| e);
    let skip_enter_only = steering && !ids.iter().all(enter_only);
    ids.iter()
        .copied()
        .filter(move |id| !(skip_enter_only && enter_only(id)))
}

/// Events from an OS input callback (the Windows hooks, the macOS event
/// tap) to the capture task. Sending never blocks the callback: while
/// `limit` events wait, those whose loss is harmless (motion, scrolling,
/// presses, a crossing) are dropped; releases, modifier updates and `End`
/// always queue, in order with the rest, as losing one leaves a key or
/// button held on the peer.
#[cfg(any(windows, target_os = "macos", test))]
mod event_queue {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, ready},
    };

    use input_event::{Event, KeyboardEvent, PointerEvent};
    use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

    use crate::{CaptureEvent, Position};

    pub(crate) fn channel(limit: usize) -> (EventSender, EventReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let sender = EventSender {
            tx,
            queued: queued.clone(),
            limit,
        };
        (sender, EventReceiver { rx, queued })
    }

    #[derive(Clone)]
    pub(crate) struct EventSender {
        tx: UnboundedSender<(Position, CaptureEvent)>,
        /// events sent and not yet received
        queued: Arc<AtomicUsize>,
        limit: usize,
    }

    impl EventSender {
        /// Queues `event`; false (and logged) if it was dropped because the
        /// capture task is not keeping up, or the receiver is gone.
        pub(crate) fn send(&self, pos: Position, event: CaptureEvent) -> bool {
            if self.queued.load(Ordering::Relaxed) >= self.limit && !must_arrive(&event) {
                log::warn!("dropped {event}: the capture task is not keeping up");
                return false;
            }
            // counted before it can be received, so the count never wraps
            self.queued.fetch_add(1, Ordering::Relaxed);
            let sent = self.tx.send((pos, event)).is_ok();
            if !sent {
                log::debug!("dropped {event}: input capture is gone");
            }
            sent
        }
    }

    pub(crate) struct EventReceiver {
        rx: UnboundedReceiver<(Position, CaptureEvent)>,
        queued: Arc<AtomicUsize>,
    }

    impl EventReceiver {
        pub(crate) fn poll_recv(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<Option<(Position, CaptureEvent)>> {
            let event = ready!(self.rx.poll_recv(cx));
            if event.is_some() {
                self.queued.fetch_sub(1, Ordering::Relaxed);
            }
            Poll::Ready(event)
        }
    }

    /// events whose loss leaves something held on the peer
    fn must_arrive(event: &CaptureEvent) -> bool {
        matches!(
            event,
            CaptureEvent::End
                | CaptureEvent::Input(
                    Event::Keyboard(
                        KeyboardEvent::Key { state: 0, .. } | KeyboardEvent::Modifiers { .. }
                    ) | Event::Pointer(PointerEvent::Button { state: 0, .. })
                )
        )
    }

    #[cfg(test)]
    mod tests {
        use std::future::poll_fn;

        use super::*;

        fn key(state: u8) -> CaptureEvent {
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state,
            }))
        }

        const MOTION: CaptureEvent = CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.,
            dy: 1.,
        }));

        fn button(state: u32) -> CaptureEvent {
            CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: input_event::BTN_LEFT,
                state,
            }))
        }

        #[tokio::test]
        async fn full_queue_keeps_releases_in_order() {
            let (tx, mut rx) = channel(2);
            let pos = Position::Left;
            assert!(tx.send(pos, CaptureEvent::Begin(None)));
            assert!(tx.send(pos, key(1)));

            // full: presses, motion and crossings are dropped
            assert!(!tx.send(pos, button(1)));
            assert!(!tx.send(pos, MOTION));
            assert!(!tx.send(pos, CaptureEvent::Begin(None)));
            // releases and the end of capture still arrive
            let modifiers = CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            for event in [key(0), button(0), modifiers, CaptureEvent::End] {
                assert!(tx.send(pos, event), "{event:?}");
            }

            let mut received = Vec::new();
            for _ in 0..6 {
                let (_, event) = poll_fn(|cx| rx.poll_recv(cx)).await.unwrap();
                received.push(event);
            }
            let expected = [
                CaptureEvent::Begin(None),
                key(1),
                key(0),
                button(0),
                modifiers,
                CaptureEvent::End,
            ];
            assert_eq!(received, expected);

            // drained: there is room again
            assert!(tx.send(pos, MOTION));
        }

        #[test]
        fn send_without_receiver_fails_without_panic() {
            let (tx, rx) = channel(2);
            drop(rx);
            assert!(!tx.send(Position::Left, key(0)));
            assert!(!tx.send(Position::Left, CaptureEvent::End));
        }
    }
}

/// An input capture backend: barriers at screen edges and the events
/// captured through them.
#[async_trait]
pub trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a barrier at `pos`, covering `range` of the edge (`None`: all);
    /// replaces the barrier already at `pos`
    async fn create(&mut self, pos: Position, range: Option<EdgeRange>)
    -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// Start capture at `pos` without a crossing, as if the cursor crossed
    /// in the middle of `range`, and report `Begin`. Backends that cannot
    /// grab input on their own return [`CaptureError::FocusByPointerPush`]:
    /// a pointer pushed through the barrier by the local input emulation
    /// starts it.
    #[allow(clippy::double_must_use)] // async_trait's boxed future, as for the methods above
    async fn focus(&mut self, pos: Position, range: Option<EdgeRange>) -> Result<(), CaptureError>;

    /// Chords that start capture at a position while input is local. Only
    /// backends that see local keys watch them (Windows, macOS); elsewhere
    /// input is local exactly when these keys are invisible to mimouse.
    #[allow(clippy::double_must_use)]
    async fn set_hotkeys(&mut self, hotkeys: Vec<(Chord, Position)>) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;
}

async fn create_backend(
    backend: Backend,
    permission_pending: tokio::sync::watch::Sender<bool>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    // only the portal asks for permission per session
    #[cfg(not(libei))]
    drop(permission_pending);
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(
            libei::LibeiInputCapture::new(permission_pending).await?,
        )),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

/// backends tried in order when none is configured; never Dummy, which
/// captures nothing
const AUTO_BACKENDS: &[Backend] = &[
    #[cfg(libei)]
    Backend::InputCapturePortal,
    #[cfg(layer_shell)]
    Backend::LayerShell,
    #[cfg(x11)]
    Backend::X11,
    #[cfg(windows)]
    Backend::Windows,
    #[cfg(target_os = "macos")]
    Backend::MacOs,
];

/// the `configured` backend, else the first of `auto` that `create`
/// succeeds with, stopping early when the user denied a permission request
async fn select<T, F, Fut>(
    configured: Option<Backend>,
    auto: &[Backend],
    mut create: F,
) -> Result<(T, Selected), CaptureCreationError>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = Result<T, CaptureCreationError>>,
{
    if let Some(backend) = configured {
        let capture = create(backend).await?;
        log::info!("using capture backend: {backend} (configured)");
        return Ok((
            capture,
            Selected {
                backend,
                auto: false,
            },
        ));
    }
    for &backend in auto {
        match create(backend).await {
            Ok(capture) => {
                log::info!("using capture backend: {backend} (auto)");
                return Ok((
                    capture,
                    Selected {
                        backend,
                        auto: true,
                    },
                ));
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use input_event::PointerEvent;

    use super::*;

    /// what the test backend saw and will report
    #[derive(Default)]
    struct Shared {
        events: VecDeque<(Position, CaptureEvent)>,
        barriers: HashMap<Position, Option<EdgeRange>>,
    }

    struct TestCapture(Arc<Mutex<Shared>>);

    #[async_trait]
    impl Capture for TestCapture {
        async fn create(
            &mut self,
            pos: Position,
            range: Option<EdgeRange>,
        ) -> Result<(), CaptureError> {
            self.0.lock().unwrap().barriers.insert(pos, range);
            Ok(())
        }

        async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
            self.0.lock().unwrap().barriers.remove(&pos);
            Ok(())
        }

        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }

        async fn focus(&mut self, _: Position, _: Option<EdgeRange>) -> Result<(), CaptureError> {
            Ok(())
        }

        async fn set_hotkeys(&mut self, _: Vec<(Chord, Position)>) -> Result<(), CaptureError> {
            Ok(())
        }

        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    impl Stream for TestCapture {
        type Item = Result<(Position, CaptureEvent), CaptureError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.lock().unwrap().events.pop_front().map(Ok))
        }
    }

    fn input_capture() -> (InputCapture, Arc<Mutex<Shared>>) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let capture = InputCapture::with_backend(Box::new(TestCapture(shared.clone())));
        (capture, shared)
    }

    const MOTION: CaptureEvent = CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
        time: 0,
        dx: 1.,
        dy: 1.,
    }));

    #[tokio::test]
    async fn destroyed_id_never_yields_queued_event() {
        let (mut capture, shared) = input_capture();
        capture
            .create(0, Position::Left, None, false)
            .await
            .unwrap();
        capture
            .create(1, Position::Left, None, false)
            .await
            .unwrap();
        shared
            .lock()
            .unwrap()
            .events
            .push_back((Position::Left, MOTION));

        // one id gets the event now, the other's copy is queued
        let (first, _) = capture.next().await.unwrap().unwrap();
        let other = 1 - first;
        capture.destroy(other).await.unwrap();

        let mut yielded = Vec::new();
        while let Some(event) = capture.next().await {
            yielded.push(event.unwrap().0);
        }
        assert!(!yielded.contains(&other), "{yielded:?}");

        // the caller recovers: the next event reaches the live id
        shared
            .lock()
            .unwrap()
            .events
            .push_back((Position::Left, MOTION));
        assert_eq!(capture.next().await.unwrap().unwrap(), (first, MOTION));
    }

    #[tokio::test]
    async fn enter_only_keeps_default_range_and_input() {
        let (mut capture, shared) = input_capture();
        let range = EdgeRange::new(0, 40);
        capture
            .create(0, Position::Left, range, false)
            .await
            .unwrap();
        capture.create(1, Position::Left, None, true).await.unwrap();
        assert_eq!(shared.lock().unwrap().barriers[&Position::Left], range);

        // the crossing reaches both, the input only the Default id
        let begin = CaptureEvent::Begin(None);
        shared
            .lock()
            .unwrap()
            .events
            .extend([(Position::Left, begin), (Position::Left, MOTION)]);
        let mut yielded = Vec::new();
        while let Some(event) = capture.next().await {
            yielded.push(event.unwrap());
        }
        assert_eq!(yielded, [(0, begin), (1, begin), (0, MOTION)]);

        // the Default id gone, the enter-only id gets the whole edge back
        capture.destroy(0).await.unwrap();
        assert_eq!(shared.lock().unwrap().barriers[&Position::Left], None);
    }

    #[tokio::test]
    async fn enter_only_alone_keeps_whole_edge() {
        let (mut capture, shared) = input_capture();
        capture.create(1, Position::Left, None, true).await.unwrap();
        assert_eq!(shared.lock().unwrap().barriers[&Position::Left], None);
        shared
            .lock()
            .unwrap()
            .events
            .push_back((Position::Left, MOTION));
        assert_eq!(capture.next().await.unwrap().unwrap(), (1, MOTION));

        // a Default id joining narrows it to its range
        let range = EdgeRange::new(0, 40);
        capture
            .create(0, Position::Left, range, false)
            .await
            .unwrap();
        assert_eq!(shared.lock().unwrap().barriers[&Position::Left], range);
    }

    #[tokio::test]
    async fn release_drops_queued_input() {
        let (mut capture, shared) = input_capture();
        // two enter-only ids: both get the input, one copy is queued
        capture.create(0, Position::Left, None, true).await.unwrap();
        capture.create(1, Position::Left, None, true).await.unwrap();
        shared
            .lock()
            .unwrap()
            .events
            .push_back((Position::Left, MOTION));
        capture.next().await.unwrap().unwrap();

        // input of the released capture never reaches the caller after it
        capture.release().await.unwrap();
        assert_eq!(capture.next().await.map(Result::unwrap), None);
    }

    /// T5: a configured backend is reported as configured and never
    /// replaced; without one, the first auto backend that works is
    /// reported as auto
    #[test]
    fn selection_reports_configured_or_auto() {
        use futures::executor::block_on;
        let configured = Selected {
            backend: Backend::Dummy,
            auto: false,
        };
        let mut tried = 0;
        let result = block_on(select(Some(Backend::Dummy), AUTO_BACKENDS, |_| {
            tried += 1;
            async { Ok::<_, CaptureCreationError>("dummy") }
        }));
        assert_eq!(result.unwrap(), ("dummy", configured));
        assert_eq!(tried, 1);

        // a configured backend that fails is not replaced by an auto one
        let mut tried = 0;
        let result = block_on(select(Some(Backend::Dummy), &[Backend::Dummy], |_| {
            tried += 1;
            async { Err::<(), _>(CaptureCreationError::NoAvailableBackend) }
        }));
        assert!(matches!(
            result,
            Err(CaptureCreationError::NoAvailableBackend)
        ));
        assert_eq!(tried, 1);

        // auto: the backend that worked, after one that did not
        let mut tried = 0;
        let result = block_on(select(None, &[Backend::Dummy, Backend::Dummy], |_| {
            tried += 1;
            let attempt = tried;
            async move {
                match attempt {
                    1 => Err(CaptureCreationError::NoAvailableBackend),
                    n => Ok(n),
                }
            }
        }));
        let auto = Selected {
            backend: Backend::Dummy,
            auto: true,
        };
        assert_eq!(result.unwrap(), (2, auto));
        // auto never picks the dummy, which captures nothing
        assert!(!AUTO_BACKENDS.contains(&Backend::Dummy));
    }
}

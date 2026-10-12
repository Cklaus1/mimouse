use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    mem::swap,
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

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position and range
    id_map: HashMap<CaptureHandle, (Position, Option<EdgeRange>)>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
    /// client hotkeys the backend starts capture with while input is local
    hotkeys: Vec<(Chord, CaptureHandle)>,
    /// the client a requested focus goes to: the next `Begin` at its
    /// position is for it alone
    focus: Option<CaptureHandle>,
}

impl InputCapture {
    /// create a new client with the given id; `range`: the part of the edge
    /// that activates it (`None`: all of it)
    pub async fn create(
        &mut self,
        id: CaptureHandle,
        pos: Position,
        range: Option<EdgeRange>,
    ) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        let old = self
            .position_map
            .contains_key(&pos)
            .then(|| self.barrier_range(pos));
        self.id_map.insert(id, (pos, range));
        self.position_map.entry(pos).or_default().push(id);
        let new = self.barrier_range(pos);
        if old != Some(new) {
            self.capture.create(pos, new).await?;
        }
        self.update_hotkeys().await
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let (pos, _) = *self.id_map.get(&id).expect("no position for this handle");
        let old = self.barrier_range(pos);
        self.id_map.remove(&id);
        if self.focus == Some(id) {
            self.focus = None;
        }

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
        let Some(&(pos, range)) = self.id_map.get(&id) else {
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

    /// the range of the barrier at `pos`: covers every client there
    fn barrier_range(&self, pos: Position) -> Option<EdgeRange> {
        let mut ranges = self
            .position_map
            .get(&pos)
            .into_iter()
            .flatten()
            .map(|id| self.id_map.get(id).and_then(|&(_, range)| range));
        let first = ranges.next()?;
        ranges.fold(first, EdgeRange::hull)
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
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
        let capture = create(backend, permission_pending).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
            hotkeys: Vec::new(),
            focus: None,
        })
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

            let len = self
                .position_map
                .get(&pos)
                .map(|ids| ids.len())
                .unwrap_or(0);

            // a requested focus: the `Begin` is for the focused client only
            if matches!(event, CaptureEvent::Begin(_)) {
                if let Some(id) = self.focus {
                    if self.id_map.get(&id).is_some_and(|&(p, _)| p == pos) {
                        self.focus = None;
                        return Poll::Ready(Some(Ok((id, event))));
                    }
                }
            }

            return match len {
                // no client at this position (destroyed meanwhile): poll
                // again, returning Pending here would lose the wakeup
                0 => continue,
                1 => Poll::Ready(Some(Ok((
                    self.position_map.get(&pos).expect("no id")[0],
                    event,
                )))),
                _ => {
                    let mut position_map = HashMap::new();
                    swap(&mut self.position_map, &mut position_map);
                    {
                        for &id in position_map.get(&pos).expect("position") {
                            self.pending.push_back((id, event));
                        }
                    }
                    swap(&mut self.position_map, &mut position_map);

                    Poll::Ready(Some(Ok(self.pending.pop_front().expect("event"))))
                }
            };
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
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

async fn create(
    backend: Option<Backend>,
    permission_pending: tokio::sync::watch::Sender<bool>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend, permission_pending).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
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
    ] {
        match create_backend(backend, permission_pending.clone()).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

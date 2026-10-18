use async_trait::async_trait;
use std::{
    collections::{HashMap, hash_map::Entry},
    fmt::Display,
    future::Future,
};
use tokio::sync::watch;

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::dummy::{DummyRecord, Received};
pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};
pub use self::geometry::{Edge, Rect};

use self::normalize::{Peer, Steps};
use self::text::Keystroke;

#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod windows_logic;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "macos", test))]
mod macos_logic;

#[cfg(test)]
mod contract;
#[cfg(test)]
mod testing;

/// configured-only emulation that discards events
mod dummy;
mod error;
mod geometry;
#[cfg(any(windows, target_os = "macos", test))]
mod keys;
mod normalize;
mod text;

pub type EmulationHandle = u64;

/// How a backend types text ([`InputEmulation::type_text`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextSupport {
    /// any Unicode text, whatever the keyboard layout
    Exact,
    /// the characters the target's keyboard layout has keys for, with
    /// Shift and AltGr (Level3/Level5) at most
    Layout,
    /// no typing: every character is unmapped
    None,
}

/// What [`InputEmulation::type_text`] could not type.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Typed {
    /// the characters no key types, in order; they were skipped
    pub unmapped: String,
}

/// The longest [`InputEmulation::type_text`] waits between the events of
/// one text (Windows paces Unicode packets), on top of sending them.
pub const MAX_TYPE_PACING: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
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
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

/// the backend an [`InputEmulation`] runs on and how it was chosen
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selected {
    pub backend: Backend,
    /// tried in order because none was configured
    pub auto: bool,
}

/// Emulates peers' input on a backend. Every event passes [`Peer::plan`]
/// first, so backends only see protocol values, real key and button
/// changes, and motion and scroll in steps they can emulate.
pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    selected: Selected,
    steps: Steps,
    peers: HashMap<EmulationHandle, Peer>,
    terminated: bool,
}

impl InputEmulation {
    async fn create_backend(
        backend: Backend,
        permission_pending: &watch::Sender<bool>,
    ) -> Result<Box<dyn Emulation>, EmulationCreationError> {
        // only the portals ask for permission
        #[cfg(not(any(libei, rdp)))]
        let _ = permission_pending;
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new().await?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new(permission_pending).await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => {
                Box::new(xdg_desktop_portal::DesktopPortalEmulation::new(permission_pending).await?)
            }
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(emulation)
    }

    fn with_emulation(emulation: Box<dyn Emulation>, selected: Selected) -> Self {
        Self {
            steps: emulation.steps(),
            emulation,
            selected,
            peers: HashMap::new(),
            terminated: false,
        }
    }

    /// A backend that discards everything (as [`Backend::Dummy`]) and logs
    /// it to `record` instead, for tests of the layers above.
    pub fn dummy(record: DummyRecord) -> Self {
        Self::with_emulation(
            Box::new(dummy::DummyEmulation::recording(record)),
            Selected {
                backend: Backend::Dummy,
                auto: false,
            },
        )
    }

    /// `permission_pending` is `true` while the backend waits for a
    /// permission request (e.g. the portal dialog)
    pub async fn new(
        backend: Option<Backend>,
        permission_pending: watch::Sender<bool>,
    ) -> Result<InputEmulation, EmulationCreationError> {
        let (emulation, selected) = select(backend, AUTO_BACKENDS, |backend| {
            Self::create_backend(backend, &permission_pending)
        })
        .await?;
        Ok(Self::with_emulation(emulation, selected))
    }

    /// the backend in use and how it was chosen
    pub fn selected(&self) -> Selected {
        self.selected
    }

    /// how the backend types text
    pub fn text_support(&self) -> TextSupport {
        self.emulation.text_support()
    }

    /// The displays in the units the backend places the pointer in, `None`
    /// if it cannot place the pointer absolutely.
    pub fn desktop(&self) -> Option<Vec<Rect>> {
        self.emulation.desktop()
    }

    /// Moves the pointer for `handle` to `(x, y)` of a desktop image `size`
    /// pixels large (e.g. a screenshot of all displays), mapped uniformly
    /// onto the bounding box of [`InputEmulation::desktop`] and clamped into
    /// it. `false` if the pointer was not placed: the backend cannot place
    /// it absolutely, there is no display, `size` is empty, `handle` was not
    /// created, or after `terminate`.
    pub fn pointer_to(
        &mut self,
        handle: EmulationHandle,
        x: i32,
        y: i32,
        size: (u32, u32),
    ) -> Result<bool, EmulationError> {
        if self.terminated || !self.peers.contains_key(&handle) {
            return Ok(false);
        }
        let Some((x, y)) = self
            .emulation
            .desktop()
            .and_then(|displays| geometry::desktop_to_backend(&displays, x, y, size))
        else {
            return Ok(false);
        };
        self.emulation.pointer_to(handle, x, y)
    }

    /// Types `text` for `handle`, `"\r\n"` and `'\r'` as a newline, and
    /// returns what no key types (see [`TextSupport`]); those characters are
    /// skipped. With [`TextSupport::Layout`] each character is a press and
    /// release of its key, inside the level modifiers it needs (modifiers
    /// `handle` holds stay held), so typing leaves no key down. Nothing is
    /// typed for a handle that was not created or after `terminate`.
    pub async fn type_text(
        &mut self,
        handle: EmulationHandle,
        text: &str,
    ) -> Result<Typed, EmulationError> {
        if self.terminated || !self.peers.contains_key(&handle) {
            return Ok(Typed {
                unmapped: text::chars(text).collect(),
            });
        }
        if let Some(typed) = self.emulation.type_text(handle, text).await? {
            return Ok(typed);
        }
        let mut unmapped = String::new();
        for c in text::chars(text) {
            match self.emulation.keystroke(c) {
                Some(stroke) => self.keystroke(handle, stroke).await?,
                None => unmapped.push(c),
            }
        }
        Ok(Typed { unmapped })
    }

    /// Presses and releases `stroke` through [`InputEmulation::consume`],
    /// so whatever is down while it types counts as held by `handle`.
    async fn keystroke(
        &mut self,
        handle: EmulationHandle,
        stroke: Keystroke,
    ) -> Result<(), EmulationError> {
        let key = |key, state| {
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state,
            })
        };
        let depressed = |depressed| {
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched: 0,
                locked: 0,
                group: 0,
            })
        };
        let holds = |this: &Self, key| this.peers.get(&handle).is_some_and(|p| p.holds(key));
        let mut pressed = [0; 3];
        for (slot, modifier) in stroke.modifiers().enumerate() {
            if !holds(self, modifier) {
                self.consume_typed(key(modifier, 1), handle).await?;
                pressed[slot] = modifier;
            }
        }
        if stroke.depressed != 0 {
            self.consume_typed(depressed(stroke.depressed), handle)
                .await?;
        }
        // a held key would not type: its press is not a change
        if holds(self, stroke.key) {
            self.consume_typed(key(stroke.key, 0), handle).await?;
        }
        self.consume_typed(key(stroke.key, 1), handle).await?;
        self.consume_typed(key(stroke.key, 0), handle).await?;
        for &modifier in pressed.iter().rev().filter(|&&m| m != 0) {
            self.consume_typed(key(modifier, 0), handle).await?;
        }
        if stroke.depressed != 0 {
            self.consume_typed(depressed(0), handle).await?;
        }
        Ok(())
    }

    /// [`InputEmulation::consume`] for typed text, which waits for the
    /// backend rather than being discarded under backpressure like motion
    async fn consume_typed(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        self.emulation.ready().await?;
        self.consume(event, handle).await
    }

    /// Emulates `event` for `handle` (see [`Peer::plan`]); events for a
    /// handle that was not created, or after `terminate`, are dropped.
    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let steps = self.steps;
        let Some(event) = self
            .peers
            .get_mut(&handle)
            .and_then(|peer| peer.plan(event, steps))
        else {
            return Ok(());
        };
        self.emulate(event, handle).await
    }

    /// hands a planned event to the backend
    async fn emulate(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        self.emulation.consume(event, handle).await?;
        if let Some(peer) = self.peers.get_mut(&handle) {
            peer.emulated(event);
        }
        Ok(())
    }

    /// Places the pointer at `fraction` along `edge` of this desktop, where
    /// a remote cursor enters (0 = top / left end, `u16::MAX` = the other
    /// end). Backends that cannot place the pointer absolutely leave it.
    pub fn enter_at(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<(), EmulationError> {
        if self.terminated {
            return Ok(());
        }
        if let Some((x, y)) = self.emulation.enter_at(handle, edge, fraction)? {
            log::info!("entered at {x},{y} ({edge} edge, fraction {fraction})");
        }
        Ok(())
    }

    /// Moves the pointer just inside `edge` at `fraction` (as `enter_at`),
    /// then relatively out through the edge, the way a user's mouse would
    /// reach it. Compositors check pointer barriers on this motion, so a
    /// capture barrier there activates. Returns `false` when the backend
    /// cannot place the pointer absolutely.
    pub async fn push_through(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<bool, EmulationError> {
        /// a few events, so one lost to rounding or acceleration still lands
        const STEPS: usize = 3;
        /// logical pixels per event: far more than the edge inset
        const STEP: f64 = 16.;
        if self.terminated {
            return Ok(false);
        }
        let Some((x, y)) = self.emulation.enter_at(handle, edge, fraction)? else {
            return Ok(false);
        };
        log::info!("pushing the pointer from {x},{y} through the {edge} edge");
        let (dx, dy) = match edge {
            Edge::Left => (-STEP, 0.),
            Edge::Right => (STEP, 0.),
            Edge::Top => (0., -STEP),
            Edge::Bottom => (0., STEP),
        };
        for _ in 0..STEPS {
            let motion = PointerEvent::Motion { time: 0, dx, dy };
            self.emulation
                .consume(Event::Pointer(motion), handle)
                .await?;
        }
        Ok(true)
    }

    /// Starts emulating for `handle`; `false` if it exists or emulation was
    /// terminated.
    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.terminated {
            return false;
        }
        let Entry::Vacant(entry) = self.peers.entry(handle) else {
            return false;
        };
        entry.insert(Peer::default());
        self.emulation.create(handle).await;
        true
    }

    /// Releases every key and button `handle` holds, then stops emulating
    /// for it.
    pub async fn destroy(&mut self, handle: EmulationHandle) {
        if let Err(e) = self.release(handle).await {
            log::warn!("could not release the input of {handle}: {e}");
        }
        if self.peers.remove(&handle).is_some() {
            self.emulation.destroy(handle).await
        }
    }

    /// Destroys every handle and ends the backend; later calls do nothing.
    pub async fn terminate(&mut self) {
        if std::mem::replace(&mut self.terminated, true) {
            return;
        }
        for handle in self.peers.keys().copied().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    /// key-ups and button-ups for everything `handle` holds, then a
    /// modifier reset
    async fn release(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let Some(peer) = self.peers.get(&handle) else {
            return Ok(());
        };
        let (keys, buttons) = peer.held();
        if keys + buttons > 0 {
            log::warn!("releasing {keys} key(s) and {buttons} button(s) held by {handle}");
        }
        for event in peer.release() {
            self.emulate(event, handle).await?;
        }
        Ok(())
    }
}

/// backends tried in order when none is configured; never Dummy, which would
/// report emulation as enabled while replaying nothing
const AUTO_BACKENDS: &[Backend] = &[
    #[cfg(wlroots)]
    Backend::Wlroots,
    #[cfg(libei)]
    Backend::Libei,
    #[cfg(rdp)]
    Backend::Xdp,
    #[cfg(x11)]
    Backend::X11,
    #[cfg(windows)]
    Backend::Windows,
    #[cfg(target_os = "macos")]
    Backend::MacOs,
];

/// the `configured` backend, else the first of `auto` that `create`
/// succeeds with, stopping early when the user denied a permission request
/// or a backend's desktop runs but is not ready yet (the caller retries it)
async fn select<T, F, Fut>(
    configured: Option<Backend>,
    auto: &[Backend],
    mut create: F,
) -> Result<(T, Selected), EmulationCreationError>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = Result<T, EmulationCreationError>>,
{
    if let Some(backend) = configured {
        let emulation = create(backend).await?;
        log::info!("using emulation backend: {backend} (configured)");
        return Ok((
            emulation,
            Selected {
                backend,
                auto: false,
            },
        ));
    }
    for &backend in auto {
        match create(backend).await {
            Ok(emulation) => {
                log::info!("using emulation backend: {backend} (auto)");
                return Ok((
                    emulation,
                    Selected {
                        backend,
                        auto: true,
                    },
                ));
            }
            Err(e) if e.cancelled_by_user() || e.not_ready() => return Err(e),
            Err(e) => log::warn!("{e}"),
        }
    }
    Err(EmulationCreationError::NoAvailableBackend)
}

/// flags a permission request as pending while alive, also when the request
/// is cancelled
#[cfg(any(libei, rdp))]
struct PermissionPending<'a>(&'a watch::Sender<bool>);

#[cfg(any(libei, rdp))]
impl<'a> PermissionPending<'a> {
    fn new(pending: &'a watch::Sender<bool>) -> Self {
        pending.send_replace(true);
        Self(pending)
    }
}

#[cfg(any(libei, rdp))]
impl Drop for PermissionPending<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

// async_trait returns a boxed future, itself `must_use`, from methods that return `Result`
#[allow(clippy::double_must_use)]
#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    /// Smallest motion and scroll steps the backend emulates; the layer
    /// above keeps anything finer per peer until it adds up.
    fn steps(&self) -> Steps {
        Steps::EXACT
    }
    /// Moves the pointer to [`geometry::entry_point`] of this desktop and
    /// returns the point, `None` if the pointer was not placed.
    fn enter_at(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError>;
    /// how the backend types text
    fn text_support(&self) -> TextSupport {
        TextSupport::None
    }
    /// the displays in the units of [`Emulation::pointer_to`], `None` if
    /// the pointer cannot be placed absolutely
    fn desktop(&self) -> Option<Vec<Rect>> {
        None
    }
    /// Moves the pointer to `(x, y)` of [`Emulation::desktop`]; `false` if
    /// it was not placed.
    fn pointer_to(
        &mut self,
        _handle: EmulationHandle,
        _x: i32,
        _y: i32,
    ) -> Result<bool, EmulationError> {
        Ok(false)
    }
    /// Types `text` itself; `None` to have it typed with
    /// [`Emulation::keystroke`] key events instead.
    async fn type_text(
        &mut self,
        _handle: EmulationHandle,
        _text: &str,
    ) -> Result<Option<Typed>, EmulationError> {
        Ok(None)
    }
    /// the keys that type `c` in the target's layout, `None` if none does
    fn keystroke(&self, _c: char) -> Option<Keystroke> {
        None
    }
    /// Waits until the next event would be sent rather than discarded
    /// under backpressure.
    async fn ready(&mut self) -> Result<(), EmulationError> {
        Ok(())
    }
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Call, Recorder};
    use futures::executor::block_on;
    use input_event::{BTN_LEFT, KeyboardEvent};

    const KEY_A: u32 = 30;

    fn key(key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state,
        })
    }

    fn button(button: u32, state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state,
        })
    }

    fn motion(dx: f64, dy: f64) -> Event {
        Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
    }

    const RESET: Event = Event::Keyboard(KeyboardEvent::Modifiers {
        depressed: 0,
        latched: 0,
        locked: 0,
        group: 0,
    });

    #[test]
    fn destroy_releases_held_keys_and_buttons() {
        let (mut emulation, mut calls) = Recorder::emulation(Steps::EXACT, true);
        block_on(async {
            emulation.create(1).await;
            emulation.consume(button(BTN_LEFT, 1), 1).await.unwrap();
            emulation.consume(key(KEY_A, 1), 1).await.unwrap();
            calls.take();
            emulation.destroy(1).await;
        });
        assert_eq!(
            calls.take(),
            [
                Call::Consume(1, key(KEY_A, 0)),
                Call::Consume(1, button(BTN_LEFT, 0)),
                Call::Consume(1, RESET),
                Call::Destroy(1),
            ]
        );
    }

    fn depressed(depressed: u32) -> Event {
        Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched: 0,
            locked: 0,
            group: 0,
        })
    }

    #[test]
    fn layout_text_is_typed_inside_its_modifiers() {
        use crate::testing::{KEY_E, KEY_LEFTSHIFT as SHIFT, KEY_RIGHTALT as ALTGR};
        let (mut emulation, mut calls) = Recorder::emulation(Steps::EXACT, true);
        let typed = block_on(async {
            emulation.create(1).await;
            calls.take();
            emulation.type_text(1, "A€").await.unwrap()
        });
        assert_eq!(typed.unmapped, "");
        let consumed = [
            key(SHIFT, 1),
            depressed(1),
            key(KEY_A, 1),
            key(KEY_A, 0),
            key(SHIFT, 0),
            depressed(0),
            key(SHIFT, 1),
            key(ALTGR, 1),
            depressed(0x81),
            key(KEY_E, 1),
            key(KEY_E, 0),
            key(ALTGR, 0),
            key(SHIFT, 0),
            depressed(0),
        ];
        let mut expected = vec![Call::TypeText(1, "A€".into())];
        expected.extend(consumed.map(|e| Call::Consume(1, e)));
        assert_eq!(calls.take(), expected);
    }

    #[test]
    fn typing_keeps_what_the_peer_holds() {
        use crate::testing::KEY_LEFTSHIFT as SHIFT;
        let (mut emulation, mut calls) = Recorder::emulation(Steps::EXACT, true);
        block_on(async {
            emulation.create(1).await;
            emulation.consume(key(SHIFT, 1), 1).await.unwrap();
            emulation.consume(key(KEY_A, 1), 1).await.unwrap();
            calls.take();
            emulation.type_text(1, "A").await.unwrap();
        });
        // Shift stays down; the held A is released so its press types
        let consumed = [
            depressed(1),
            key(KEY_A, 0),
            key(KEY_A, 1),
            key(KEY_A, 0),
            depressed(0),
        ];
        let mut expected = vec![Call::TypeText(1, "A".into())];
        expected.extend(consumed.map(|e| Call::Consume(1, e)));
        assert_eq!(calls.take(), expected);
        block_on(emulation.destroy(1));
        assert_eq!(
            calls.take(),
            [
                Call::Consume(1, key(SHIFT, 0)),
                Call::Consume(1, RESET),
                Call::Destroy(1),
            ]
        );
    }

    #[test]
    fn dropped_typing_is_released_on_destroy() {
        use crate::testing::{Fake, KEY_LEFTSHIFT as SHIFT};
        use futures::FutureExt;
        // stalls on the press of A, after Shift and the modifier state
        let fake = Fake {
            steps: Steps::EXACT,
            places_pointer: true,
            stall_at: Some(2),
        };
        let (mut emulation, mut calls) = Recorder::wrap(Box::new(fake));
        block_on(emulation.create(1));
        assert!(emulation.type_text(1, "A").now_or_never().is_none());
        calls.take();
        block_on(emulation.destroy(1));
        let calls = calls.take();
        for released in [key(KEY_A, 0), key(SHIFT, 0)] {
            assert!(calls.contains(&Call::Consume(1, released)), "{calls:?}");
        }
        assert_eq!(calls.last(), Some(&Call::Destroy(1)));
    }

    #[test]
    fn values_outside_the_protocol_never_reach_the_backend() {
        let (mut emulation, mut calls) = Recorder::emulation(Steps::EXACT, true);
        block_on(async {
            emulation.create(1).await;
            emulation.consume(key(KEY_A, 2), 1).await.unwrap();
            emulation.consume(button(BTN_LEFT, 7), 1).await.unwrap();
            let axis = PointerEvent::AxisDiscrete120 {
                axis: 2,
                value: 120,
            };
            emulation.consume(Event::Pointer(axis), 1).await.unwrap();
            // a handle that was never created
            emulation.consume(key(KEY_A, 1), 2).await.unwrap();
        });
        assert_eq!(calls.take(), [Call::Create(1)]);
    }

    #[test]
    fn remainders_are_kept_per_handle() {
        let whole = Steps {
            whole_pixels: true,
            v120: 1,
        };
        let (mut emulation, mut calls) = Recorder::emulation(whole, true);
        block_on(async {
            emulation.create(1).await;
            emulation.create(2).await;
            calls.take();
            emulation.consume(motion(0.6, 0.), 1).await.unwrap();
            // another peer's motion does not complete the first one's pixel
            emulation.consume(motion(0.6, 0.), 2).await.unwrap();
            assert_eq!(calls.take(), []);
            emulation.consume(motion(0.6, 0.), 1).await.unwrap();
        });
        assert_eq!(calls.take(), [Call::Consume(1, motion(1., 0.))]);
    }

    #[test]
    fn auto_selection_without_a_working_backend_fails() {
        let mut tried = Vec::new();
        let result = futures::executor::block_on(select(None, AUTO_BACKENDS, |backend| {
            tried.push(backend);
            async { Err::<(), _>(EmulationCreationError::NoAvailableBackend) }
        }));
        assert!(matches!(
            result,
            Err(EmulationCreationError::NoAvailableBackend)
        ));
        assert_eq!(tried, AUTO_BACKENDS);
        // never silently replays remote input into the log
        assert!(!tried.contains(&Backend::Dummy));
    }

    /// T5: a configured backend is reported as configured and never
    /// replaced; without one, the first auto backend that works is
    /// reported as auto
    #[test]
    fn selection_reports_configured_or_auto() {
        use futures::executor::block_on;
        let mut tried = 0;
        let result = block_on(select(Some(Backend::Dummy), AUTO_BACKENDS, |_| {
            tried += 1;
            async { Ok::<_, EmulationCreationError>("dummy") }
        }));
        let configured = Selected {
            backend: Backend::Dummy,
            auto: false,
        };
        assert_eq!(result.unwrap(), ("dummy", configured));
        assert_eq!(tried, 1);

        // a configured backend that fails is not replaced by an auto one
        let mut tried = 0;
        let result = block_on(select(Some(Backend::Dummy), &[Backend::Dummy], |_| {
            tried += 1;
            async { Err::<(), _>(EmulationCreationError::NoAvailableBackend) }
        }));
        assert!(matches!(
            result,
            Err(EmulationCreationError::NoAvailableBackend)
        ));
        assert_eq!(tried, 1);

        // auto: the backend that worked, after one that did not
        let mut tried = 0;
        let result = block_on(select(None, &[Backend::Dummy, Backend::Dummy], |_| {
            tried += 1;
            let attempt = tried;
            async move {
                match attempt {
                    1 => Err(EmulationCreationError::NoAvailableBackend),
                    n => Ok(n),
                }
            }
        }));
        let auto = Selected {
            backend: Backend::Dummy,
            auto: true,
        };
        assert_eq!(result.unwrap(), (2, auto));
    }

    /// a wlroots compositor whose seat has no keyboard yet is retried (the
    /// error is transient), never replaced by X11, which on Xwayland reaches
    /// only X11 clients; without a compositor, X11 is still selected
    #[cfg(all(wlroots, x11))]
    #[test]
    fn auto_selection_retries_wlroots_until_its_seat_has_a_keyboard() {
        use crate::error::WlrootsEmulationCreationError;
        use std::time::Duration;
        let no_keymap = || {
            EmulationCreationError::Wlroots(WlrootsEmulationCreationError::Timeout {
                awaited: "keymap",
                after: Duration::from_secs(5),
            })
        };
        let x11_or = |wlroots_error: fn() -> EmulationCreationError| {
            move |backend: Backend| async move {
                match backend {
                    Backend::Wlroots => Err(wlroots_error()),
                    _ => Ok(backend),
                }
            }
        };

        let result = block_on(select(
            None,
            &[Backend::Wlroots, Backend::X11],
            x11_or(no_keymap),
        ));
        let error = InputEmulationError::from(result.unwrap_err());
        assert!(error.is_transient(), "{error}");

        // configured: the same error, as before
        let result = block_on(select(
            Some(Backend::Wlroots),
            &[Backend::Wlroots, Backend::X11],
            x11_or(no_keymap),
        ));
        assert!(InputEmulationError::from(result.unwrap_err()).is_transient());

        // no Wayland compositor (an X11 session): X11
        let no_compositor = || {
            EmulationCreationError::Wlroots(WlrootsEmulationCreationError::Connect(
                wayland_client::ConnectError::NoCompositor,
            ))
        };
        let result = block_on(select(
            None,
            &[Backend::Wlroots, Backend::X11],
            x11_or(no_compositor),
        ));
        let auto = Selected {
            backend: Backend::X11,
            auto: true,
        };
        assert_eq!(result.unwrap(), (Backend::X11, auto));
    }
}

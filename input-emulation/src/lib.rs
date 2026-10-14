use async_trait::async_trait;
use std::{
    collections::{HashMap, hash_map::Entry},
    fmt::Display,
    future::Future,
};
use tokio::sync::watch;

use input_event::{Event, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};
pub use self::geometry::Edge;
use self::normalize::{Peer, Steps};

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

pub type EmulationHandle = u64;

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

/// Emulates peers' input on a backend. Every event passes [`Peer::plan`]
/// first, so backends only see protocol values, real key and button
/// changes, and motion and scroll in steps they can emulate.
pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    steps: Steps,
    peers: HashMap<EmulationHandle, Peer>,
    terminated: bool,
}

impl InputEmulation {
    async fn with_backend(
        backend: Backend,
        permission_pending: &watch::Sender<bool>,
    ) -> Result<InputEmulation, EmulationCreationError> {
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
        Ok(Self::with_emulation(emulation))
    }

    fn with_emulation(emulation: Box<dyn Emulation>) -> Self {
        Self {
            steps: emulation.steps(),
            emulation,
            peers: HashMap::new(),
            terminated: false,
        }
    }

    /// `permission_pending` is `true` while the backend waits for a
    /// permission request (e.g. the portal dialog)
    pub async fn new(
        backend: Option<Backend>,
        permission_pending: watch::Sender<bool>,
    ) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend, &permission_pending).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        first_available(AUTO_BACKENDS, |backend| {
            Self::with_backend(backend, &permission_pending)
        })
        .await
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

/// the first of `backends` that `create` succeeds with; stops early when the
/// user denied a permission request
async fn first_available<T, F, Fut>(
    backends: &[Backend],
    mut create: F,
) -> Result<T, EmulationCreationError>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = Result<T, EmulationCreationError>>,
{
    for &backend in backends {
        match create(backend).await {
            Ok(b) => {
                log::info!("using emulation backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
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
        let result = futures::executor::block_on(first_available(AUTO_BACKENDS, |backend| {
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
}

use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    future::Future,
};
use tokio::sync::watch;

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};
pub use self::geometry::Edge;

#[cfg(windows)]
mod windows;

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

/// configured-only emulation that discards events
mod dummy;
mod error;
mod geometry;

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

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
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
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
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
        Ok(Self {
            emulation,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
        })
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

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
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

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        let _ = self.release_keys(handle).await;
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        self.emulation.consume(event, handle).await?;
        Ok(())
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
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

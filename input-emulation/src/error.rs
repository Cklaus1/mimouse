#[derive(Debug, Error)]
pub enum InputEmulationError {
    #[error("error creating input-emulation: `{0}`")]
    Create(#[from] EmulationCreationError),
    #[error("error emulating input: `{0}`")]
    Emulate(#[from] EmulationError),
}

impl InputEmulationError {
    /// the user denied the permission request
    pub fn cancelled_by_user(&self) -> bool {
        matches!(self, Self::Create(e) if e.cancelled_by_user())
    }

    /// the backend may come up by itself: the EIS / portal / compositor
    /// connection ended or is not there yet (login), or no backend works yet
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Create(EmulationCreationError::NoAvailableBackend) => true,
            #[cfg(libei)]
            Self::Create(EmulationCreationError::Libei(LibeiEmulationCreationError::Ashpd(e))) => {
                portal_not_on_bus(e)
            }
            #[cfg(rdp)]
            Self::Create(EmulationCreationError::Xdp(XdpEmulationCreationError::Ashpd(e))) => {
                portal_not_on_bus(e)
            }
            #[cfg(wlroots)]
            Self::Create(EmulationCreationError::Wlroots(e)) => e.is_transient(),
            #[cfg(x11)]
            Self::Create(EmulationCreationError::X11(X11EmulationCreationError::OpenDisplay)) => {
                true
            }
            Self::Emulate(EmulationError::EndOfStream) => true,
            #[cfg(any(rdp, libei))]
            Self::Emulate(EmulationError::Ashpd(e)) => portal_not_on_bus(e),
            #[cfg(wlroots)]
            Self::Emulate(EmulationError::Wayland(e)) => compositor_gone(e),
            _ => false,
        }
    }
}

/// the compositor closed the connection (it exited or restarted)
#[cfg(wlroots)]
fn compositor_gone(e: &WaylandError) -> bool {
    matches!(e, WaylandError::Io(e) if matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
    ))
}

#[cfg(any(libei, rdp))]
fn portal_not_on_bus(e: &ashpd::Error) -> bool {
    use ashpd::zbus::{self, fdo};
    const NOT_ON_BUS: &[&str] = &[
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    match e {
        ashpd::Error::PortalNotFound(_) => true,
        ashpd::Error::Zbus(zbus::Error::FDO(e)) => matches!(
            **e,
            fdo::Error::ServiceUnknown(_) | fdo::Error::NameHasNoOwner(_)
        ),
        ashpd::Error::Zbus(zbus::Error::MethodError(name, ..)) => {
            NOT_ON_BUS.contains(&name.as_str())
        }
        _ => false,
    }
}

#[cfg(any(libei, rdp))]
use ashpd::{Error::Response, desktop::ResponseError};
use std::io;
use thiserror::Error;

#[cfg(wlroots)]
use wayland_client::{
    ConnectError, DispatchError,
    backend::WaylandError,
    globals::{BindError, GlobalError},
};

#[derive(Debug, Error)]
pub enum EmulationError {
    #[error("event stream closed")]
    EndOfStream,
    #[cfg(libei)]
    #[error("libei error: `{0}`")]
    Libei(#[from] reis::Error),
    #[cfg(wlroots)]
    #[error("wayland error: `{0}`")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[cfg(wlroots)]
    #[error("event not representable by the backend: {0:?}")]
    InvalidEvent(input_event::Event),
    #[cfg(any(rdp, libei))]
    #[error("xdg-desktop-portal: `{0}`")]
    Ashpd(#[from] ashpd::Error),
    #[error("io error: `{0}`")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum EmulationCreationError {
    #[cfg(wlroots)]
    #[error("wlroots backend: `{0}`")]
    Wlroots(#[from] WlrootsEmulationCreationError),
    #[cfg(libei)]
    #[error("libei backend: `{0}`")]
    Libei(#[from] LibeiEmulationCreationError),
    #[cfg(rdp)]
    #[error("xdg-desktop-portal: `{0}`")]
    Xdp(#[from] XdpEmulationCreationError),
    #[cfg(x11)]
    #[error("x11: `{0}`")]
    X11(#[from] X11EmulationCreationError),
    #[cfg(target_os = "macos")]
    #[error("macos: `{0}`")]
    MacOs(#[from] MacOSEmulationCreationError),
    #[cfg(windows)]
    #[error("windows: `{0}`")]
    Windows(#[from] WindowsEmulationCreationError),
    #[error("no emulation backend available")]
    NoAvailableBackend,
}

impl EmulationCreationError {
    /// request was intentionally denied by the user
    pub(crate) fn cancelled_by_user(&self) -> bool {
        #[cfg(libei)]
        if matches!(
            self,
            EmulationCreationError::Libei(LibeiEmulationCreationError::Ashpd(Response(
                ResponseError::Cancelled,
            )))
        ) {
            return true;
        }
        #[cfg(rdp)]
        if matches!(
            self,
            EmulationCreationError::Xdp(XdpEmulationCreationError::Ashpd(Response(
                ResponseError::Cancelled,
            )))
        ) {
            return true;
        }
        false
    }

    /// The backend's desktop runs but is not ready yet: a wlroots seat
    /// without a keyboard sends no keymap. Auto selection retries it rather
    /// than fall back to a backend that reaches only part of the desktop
    /// (X11 on Xwayland).
    pub(crate) fn not_ready(&self) -> bool {
        #[cfg(wlroots)]
        if matches!(
            self,
            EmulationCreationError::Wlroots(WlrootsEmulationCreationError::Timeout { .. })
        ) {
            return true;
        }
        false
    }
}

#[cfg(wlroots)]
#[derive(Debug, Error)]
pub enum WlrootsEmulationCreationError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Global(#[from] GlobalError),
    #[error(transparent)]
    Wayland(#[from] WaylandError),
    #[error(transparent)]
    Bind(#[from] WaylandBindError),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// e.g. no keymap because the seat has no keyboard (yet)
    #[error("the compositor sent no {awaited} within {after:?}")]
    Timeout {
        awaited: &'static str,
        after: std::time::Duration,
    },
}

#[cfg(wlroots)]
impl WlrootsEmulationCreationError {
    /// no compositor yet (login), it went away while starting, or it did
    /// not answer in time
    fn is_transient(&self) -> bool {
        match self {
            Self::Connect(ConnectError::NoCompositor) | Self::Timeout { .. } => true,
            Self::Wayland(e)
            | Self::Global(GlobalError::Backend(e))
            | Self::Dispatch(DispatchError::Backend(e)) => compositor_gone(e),
            _ => false,
        }
    }
}

#[cfg(wlroots)]
#[derive(Debug, Error)]
#[error("wayland protocol \"{protocol}\" not supported: {inner}")]
pub struct WaylandBindError {
    inner: BindError,
    protocol: &'static str,
}

#[cfg(wlroots)]
impl WaylandBindError {
    pub(crate) fn new(inner: BindError, protocol: &'static str) -> Self {
        Self { inner, protocol }
    }
}

#[cfg(libei)]
#[derive(Debug, Error)]
pub enum LibeiEmulationCreationError {
    #[error(transparent)]
    Ashpd(#[from] ashpd::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Reis(#[from] reis::Error),
}

#[cfg(rdp)]
#[derive(Debug, Error)]
pub enum XdpEmulationCreationError {
    #[error(transparent)]
    Ashpd(#[from] ashpd::Error),
}

#[cfg(x11)]
#[derive(Debug, Error)]
pub enum X11EmulationCreationError {
    #[error("could not open display")]
    OpenDisplay,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Error)]
pub enum MacOSEmulationCreationError {
    #[error("could not create event source")]
    EventSourceCreation,
    #[error("accessibility permission is required")]
    AccessibilityPermission,
    #[error("input control permission is required")]
    InputControlPermission,
}

#[cfg(windows)]
#[derive(Debug, Error)]
pub enum WindowsEmulationCreationError {}

#[cfg(all(test, libei))]
mod tests {
    use super::*;
    use ashpd::zbus::{self, fdo, names::OwnedInterfaceName};

    fn libei(e: ashpd::Error) -> InputEmulationError {
        EmulationCreationError::Libei(LibeiEmulationCreationError::Ashpd(e)).into()
    }

    #[test]
    fn portal_not_up_yet_is_transient() {
        let missing = OwnedInterfaceName::try_from("org.freedesktop.portal.RemoteDesktop").unwrap();
        assert!(libei(ashpd::Error::PortalNotFound(missing)).is_transient());
        let unknown = zbus::Error::FDO(Box::new(fdo::Error::ServiceUnknown(String::new())));
        assert!(libei(ashpd::Error::Zbus(unknown)).is_transient());
        assert!(
            InputEmulationError::from(EmulationCreationError::NoAvailableBackend).is_transient()
        );
        assert!(InputEmulationError::from(EmulationError::EndOfStream).is_transient());
    }

    #[test]
    fn denied_dialog_is_final() {
        let denied = libei(Response(ResponseError::Cancelled));
        assert!(denied.cancelled_by_user());
        assert!(!denied.is_transient());
        let io = InputEmulationError::from(EmulationError::Io(io::ErrorKind::Other.into()));
        assert!(!io.is_transient());
        assert!(!io.cancelled_by_user());
    }
}

#[cfg(all(test, wlroots, x11))]
mod compositor_tests {
    use super::*;
    use std::time::Duration;

    fn wlroots(e: WlrootsEmulationCreationError) -> InputEmulationError {
        EmulationCreationError::Wlroots(e).into()
    }

    #[test]
    fn compositor_loss_is_transient() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::UnexpectedEof,
        ] {
            let lost = WaylandError::Io(kind.into());
            assert!(
                InputEmulationError::from(EmulationError::Wayland(lost.clone())).is_transient()
            );
            assert!(wlroots(lost.clone().into()).is_transient());
            assert!(wlroots(DispatchError::Backend(lost).into()).is_transient());
        }
        // no compositor yet, e.g. right after login
        assert!(wlroots(ConnectError::NoCompositor.into()).is_transient());
        assert!(
            wlroots(WlrootsEmulationCreationError::Timeout {
                awaited: "keymap",
                after: Duration::from_secs(5),
            })
            .is_transient()
        );
        assert!(
            InputEmulationError::from(EmulationCreationError::X11(
                X11EmulationCreationError::OpenDisplay
            ))
            .is_transient()
        );
    }

    #[test]
    fn unsupported_compositor_is_final() {
        assert!(!wlroots(ConnectError::NoWaylandLib.into()).is_transient());
        let other = WaylandError::Io(io::ErrorKind::PermissionDenied.into());
        assert!(!InputEmulationError::from(EmulationError::Wayland(other)).is_transient());
    }
}

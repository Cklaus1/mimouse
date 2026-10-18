//! Lock and idle state of this desktop.

use std::time::Duration;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use linux::{idle, locked};
#[cfg(target_os = "macos")]
pub(crate) use macos::{idle, locked};
#[cfg(windows)]
pub(crate) use windows::{idle, input_desktop_locked, locked};

#[cfg(not(any(unix, windows)))]
pub(crate) async fn locked() -> Option<bool> {
    None
}

#[cfg(not(any(unix, windows)))]
pub(crate) async fn idle() -> Option<Duration> {
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use super::Duration;
    use ashpd::zbus::{self, Connection};

    pub(crate) async fn locked() -> Option<bool> {
        let conn = session().await?;
        for (service, path) in [
            ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
            (
                "org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
            ),
        ] {
            match call::<(bool,)>(&conn, service, path, service, "GetActive").await {
                Ok((active,)) => return Some(active),
                Err(e) => log::debug!("{service}.GetActive: {e}"),
            }
        }
        None
    }

    pub(crate) async fn idle() -> Option<Duration> {
        let conn = session().await?;
        let interface = "org.gnome.Mutter.IdleMonitor";
        match call::<(u64,)>(
            &conn,
            interface,
            "/org/gnome/Mutter/IdleMonitor/Core",
            interface,
            "GetIdletime",
        )
        .await
        {
            Ok((ms,)) => Some(Duration::from_millis(ms)),
            Err(e) => {
                log::debug!("{interface}.GetIdletime: {e}");
                None
            }
        }
    }

    async fn session() -> Option<Connection> {
        Connection::session()
            .await
            .inspect_err(|e| log::debug!("session bus: {e}"))
            .ok()
    }

    async fn call<T>(
        conn: &Connection,
        service: &str,
        path: &str,
        interface: &str,
        method: &str,
    ) -> zbus::Result<T>
    where
        T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = conn
            .call_method(Some(service), path, Some(interface), method, &())
            .await?;
        reply.body().deserialize()
    }
}

#[cfg(windows)]
mod windows {
    use super::Duration;
    use windows::Win32::{
        Foundation::HANDLE,
        System::{
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
                GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
            },
            SystemInformation::GetTickCount,
        },
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
    };

    pub(crate) async fn locked() -> Option<bool> {
        tokio::task::spawn_blocking(input_desktop_locked)
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn idle() -> Option<Duration> {
        tokio::task::spawn_blocking(|| {
            let mut info = LASTINPUTINFO {
                cbSize: size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            // SAFETY: info is a valid LASTINPUTINFO with cbSize set
            if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
                log::debug!("GetLastInputInfo: {}", std::io::Error::last_os_error());
                return None;
            }
            // both are the 32-bit tick count, so the difference survives its wrap
            // SAFETY: no preconditions
            let now = unsafe { GetTickCount() };
            Some(Duration::from_millis(u64::from(
                now.wrapping_sub(info.dwTime),
            )))
        })
        .await
        .ok()
        .flatten()
    }

    /// whether the desktop receiving input is not the user's `Default` one
    /// (Winlogon: lock screen, UAC, Ctrl+Alt+Del); `None` if unknown
    pub(crate) fn input_desktop_locked() -> Option<bool> {
        // SAFETY: plain call; the handle is closed below
        let desktop =
            match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }
            {
                Ok(desktop) => desktop,
                Err(e) => {
                    // the secure desktop can't be opened from the user's session
                    log::debug!("OpenInputDesktop: {e}");
                    return Some(true);
                }
            };
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        // SAFETY: name is writable for the byte length passed
        let read = unsafe {
            GetUserObjectInformationW(
                HANDLE(desktop.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                size_of_val(&name) as u32,
                Some(&mut needed),
            )
        };
        // SAFETY: desktop came from OpenInputDesktop and is closed once
        if let Err(e) = unsafe { CloseDesktop(desktop) } {
            log::debug!("CloseDesktop: {e}");
        }
        if let Err(e) = read {
            // a name longer than the buffer is not "Default"
            log::debug!("GetUserObjectInformationW(UOI_NAME): {e}");
            return (needed as usize > size_of_val(&name)).then_some(true);
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..len]) != "Default")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Duration;
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        dictionary::{CFDictionary, CFDictionaryRef},
        string::CFString,
    };

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }

    const COMBINED_SESSION_STATE: i32 = 0;
    /// `kCGAnyInputEventType`
    const ANY_INPUT_EVENT: u32 = !0;

    pub(crate) async fn locked() -> Option<bool> {
        tokio::task::spawn_blocking(|| {
            // SAFETY: returns an owned dictionary or NULL outside a GUI session
            let dict = unsafe { CGSessionCopyCurrentDictionary() };
            if dict.is_null() {
                return None;
            }
            // SAFETY: dict is a non-NULL owned CFDictionary (create rule)
            let dict: CFDictionary<CFString, CFType> =
                unsafe { CFDictionary::wrap_under_create_rule(dict) };
            let locked = dict
                .find(CFString::from_static_string("CGSSessionScreenIsLocked"))
                .and_then(|v| v.downcast::<CFBoolean>())
                .is_some_and(bool::from);
            Some(locked)
        })
        .await
        .ok()
        .flatten()
    }

    pub(crate) async fn idle() -> Option<Duration> {
        tokio::task::spawn_blocking(|| {
            // SAFETY: plain query
            let seconds = unsafe {
                CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT)
            };
            Duration::try_from_secs_f64(seconds).ok()
        })
        .await
        .ok()
        .flatten()
    }
}

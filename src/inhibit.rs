//! Keeps this machine from idling or locking while it forwards input: the
//! captured input never reaches the local idle timer.
//!
//! Linux: org.gnome.SessionManager.Inhibit (idle), or the
//! org.freedesktop.portal.Inhibit portal where gnome-session is absent.
//! Windows: `SetThreadExecutionState` plus a tagged zero-length mouse move
//! every 30 s. macOS: an IOPM `PreventUserIdleDisplaySleep` assertion.
//!
//! A worker owns the inhibitor; releases are delayed by [`RELEASE_DELAY`] so
//! crossing back and forth does not churn inhibitors.

use std::time::{Duration, Instant};

/// how long control must stay local before the inhibitor is released
const RELEASE_DELAY: Duration = Duration::from_secs(5);
#[cfg(not(windows))]
const REASON: &str = "MiMouse is controlling another machine";

pub(crate) struct IdleInhibit {
    enabled: bool,
    /// this machine forwards input
    active: bool,
    worker: worker::Worker,
}

impl IdleInhibit {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            active: false,
            worker: worker::Worker::new(),
        }
    }

    /// `active`: this machine is forwarding input to a remote client
    pub(crate) fn set(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if self.enabled {
            self.worker.send(active);
        }
    }

    /// `idle_inhibit` from the config
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if self.active {
            self.worker.send(enabled);
        }
    }
}

/// Turns requested states into acquire/release actions: acquire at once,
/// release only after the request to release has stood for the delay.
struct Debounce {
    delay: Duration,
    held: bool,
    release_at: Option<Instant>,
}

impl Debounce {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            held: false,
            release_at: None,
        }
    }

    /// returns `Some(true)` when the inhibitor must be acquired now
    fn request(&mut self, active: bool, now: Instant) -> Option<bool> {
        if active {
            self.release_at = None;
            if !self.held {
                self.held = true;
                return Some(true);
            }
        } else if self.held && self.release_at.is_none() {
            self.release_at = Some(now + self.delay);
        }
        None
    }

    /// when [`Debounce::poll`] has to run next
    fn deadline(&self) -> Option<Instant> {
        self.release_at
    }

    /// returns `Some(false)` when the inhibitor must be released now
    fn poll(&mut self, now: Instant) -> Option<bool> {
        match self.release_at {
            Some(at) if now >= at => {
                self.release_at = None;
                self.held = false;
                Some(false)
            }
            _ => None,
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod worker {
    use super::{Debounce, REASON, RELEASE_DELAY};
    use ashpd::zbus::{
        self, Connection,
        zvariant::{OwnedObjectPath, Value},
    };
    use std::{collections::HashMap, time::Instant};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// inhibit flag "idle" of gnome-session and the Inhibit portal
    const IDLE: u32 = 8;

    pub(super) struct Worker {
        tx: Option<UnboundedSender<bool>>,
    }

    impl Worker {
        pub(super) fn new() -> Self {
            Self { tx: None }
        }

        pub(super) fn send(&mut self, active: bool) {
            let tx = self.tx.get_or_insert_with(|| {
                let (tx, rx) = unbounded_channel();
                tokio::task::spawn_local(run(rx));
                tx
            });
            let _ = tx.send(active);
        }
    }

    async fn run(mut rx: UnboundedReceiver<bool>) {
        let mut debounce = Debounce::new(RELEASE_DELAY);
        let mut inhibitor = Inhibitor::default();
        loop {
            let action = match debounce.deadline() {
                Some(at) => tokio::select! {
                    r = rx.recv() => match r {
                        Some(active) => debounce.request(active, Instant::now()),
                        None => break,
                    },
                    _ = tokio::time::sleep_until(at.into()) => debounce.poll(Instant::now()),
                },
                None => match rx.recv().await {
                    Some(active) => debounce.request(active, Instant::now()),
                    None => break,
                },
            };
            match action {
                Some(true) => inhibitor.acquire().await,
                Some(false) => inhibitor.release().await,
                None => {}
            }
        }
        inhibitor.release().await;
    }

    #[derive(Debug)]
    enum Held {
        /// gnome-session cookie
        Session(u32),
        /// portal request handle
        Portal(OwnedObjectPath),
    }

    /// Both inhibitors end when our bus connection closes, so a crash never
    /// leaves the session inhibited.
    #[derive(Default)]
    struct Inhibitor {
        conn: Option<Connection>,
        held: Option<Held>,
    }

    impl Inhibitor {
        async fn acquire(&mut self) {
            if self.held.is_some() {
                return;
            }
            match self.inhibit().await {
                Ok(held) => {
                    log::info!("idle inhibited ({held:?})");
                    self.held = Some(held);
                }
                Err(e) => {
                    log::warn!("could not inhibit idle: {e}");
                    self.conn = None;
                }
            }
        }

        async fn inhibit(&mut self) -> zbus::Result<Held> {
            let conn = match &self.conn {
                Some(c) => c.clone(),
                None => self.conn.insert(Connection::session().await?).clone(),
            };
            let session = conn
                .call_method(
                    Some("org.gnome.SessionManager"),
                    "/org/gnome/SessionManager",
                    Some("org.gnome.SessionManager"),
                    "Inhibit",
                    &("mimouse", 0u32, REASON, IDLE),
                )
                .await;
            match session {
                Ok(reply) => Ok(Held::Session(reply.body().deserialize()?)),
                Err(e) => {
                    log::debug!("org.gnome.SessionManager.Inhibit: {e}; trying the portal");
                    let options = HashMap::from([("reason", Value::from(REASON))]);
                    let reply = conn
                        .call_method(
                            Some("org.freedesktop.portal.Desktop"),
                            "/org/freedesktop/portal/desktop",
                            Some("org.freedesktop.portal.Inhibit"),
                            "Inhibit",
                            &("", IDLE, options),
                        )
                        .await?;
                    Ok(Held::Portal(reply.body().deserialize()?))
                }
            }
        }

        async fn release(&mut self) {
            let (Some(held), Some(conn)) = (self.held.take(), self.conn.as_ref()) else {
                return;
            };
            let r = match &held {
                Held::Session(cookie) => {
                    conn.call_method(
                        Some("org.gnome.SessionManager"),
                        "/org/gnome/SessionManager",
                        Some("org.gnome.SessionManager"),
                        "Uninhibit",
                        &(*cookie,),
                    )
                    .await
                }
                Held::Portal(handle) => {
                    conn.call_method(
                        Some("org.freedesktop.portal.Desktop"),
                        handle,
                        Some("org.freedesktop.portal.Request"),
                        "Close",
                        &(),
                    )
                    .await
                }
            };
            match r {
                Ok(_) => log::info!("idle inhibit released"),
                Err(e) => {
                    // dropping the connection ends the inhibitor as well
                    log::warn!("could not release idle inhibit ({held:?}): {e}");
                    self.conn = None;
                }
            }
        }
    }
}

/// a thread that holds the platform assertion
#[cfg(any(windows, target_os = "macos"))]
mod worker {
    use super::{Debounce, RELEASE_DELAY};
    use std::{
        sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel},
        time::Instant,
    };

    pub(super) struct Worker {
        tx: Option<Sender<bool>>,
    }

    impl Worker {
        pub(super) fn new() -> Self {
            Self { tx: None }
        }

        pub(super) fn send(&mut self, active: bool) {
            if self.tx.is_none() {
                let (tx, rx) = channel();
                match std::thread::Builder::new()
                    .name("idle-inhibit".into())
                    .spawn(move || run(rx))
                {
                    Ok(_) => self.tx = Some(tx),
                    Err(e) => {
                        log::warn!("could not start idle inhibit thread: {e}");
                        return;
                    }
                }
            }
            if let Some(tx) = &self.tx {
                let _ = tx.send(active);
            }
        }
    }

    fn run(rx: Receiver<bool>) {
        let mut debounce = Debounce::new(RELEASE_DELAY);
        let mut inhibitor = sys::Inhibitor::default();
        loop {
            let wake = [debounce.deadline(), inhibitor.next_keepalive()]
                .into_iter()
                .flatten()
                .min();
            let action = match wake {
                None => match rx.recv() {
                    Ok(active) => debounce.request(active, Instant::now()),
                    Err(_) => break,
                },
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(active) => debounce.request(active, Instant::now()),
                    Err(RecvTimeoutError::Timeout) => debounce.poll(Instant::now()),
                    Err(RecvTimeoutError::Disconnected) => break,
                },
            };
            match action {
                Some(true) => inhibitor.acquire(),
                Some(false) => inhibitor.release(),
                None => {}
            }
            inhibitor.keepalive(Instant::now());
        }
        inhibitor.release();
    }

    #[cfg(windows)]
    mod sys {
        use std::time::{Duration, Instant};
        use windows::Win32::{
            System::Power::{
                ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
            },
            UI::Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
            },
        };

        /// the capture hook swallows input before the idle timer sees it
        /// (screen saver and lock still run under `ES_DISPLAY_REQUIRED`), so
        /// a zero-length move the hook lets through resets the timer
        const KEEPALIVE: Duration = Duration::from_secs(30);

        #[derive(Default)]
        pub(super) struct Inhibitor {
            next_keepalive: Option<Instant>,
        }

        impl Inhibitor {
            pub(super) fn acquire(&mut self) {
                // the execution state belongs to this thread and lasts until
                // it is reset or the thread exits
                let prev = unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
                    )
                };
                if prev.0 == 0 {
                    log::warn!("SetThreadExecutionState failed");
                } else {
                    log::info!("idle inhibited (SetThreadExecutionState)");
                }
                self.next_keepalive = Some(Instant::now() + KEEPALIVE);
            }

            pub(super) fn release(&mut self) {
                if self.next_keepalive.take().is_some() {
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                    log::info!("idle inhibit released");
                }
            }

            pub(super) fn next_keepalive(&self) -> Option<Instant> {
                self.next_keepalive
            }

            pub(super) fn keepalive(&mut self, now: Instant) {
                match self.next_keepalive {
                    Some(at) if now >= at => {}
                    _ => return,
                }
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE,
                            time: 0,
                            dwExtraInfo: input_capture::IDLE_KEEPALIVE_TAG,
                        },
                    },
                };
                let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
                if sent != 1 {
                    log::debug!("idle keepalive: SendInput failed");
                }
                self.next_keepalive = Some(now + KEEPALIVE);
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod sys {
        use super::super::REASON;
        use std::{
            ffi::{CString, c_char, c_void},
            time::Instant,
        };

        const UTF8: u32 = 0x0800_0100;
        const ASSERTION_LEVEL_ON: u32 = 255;

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFStringCreateWithCString(
                alloc: *const c_void,
                c_str: *const c_char,
                encoding: u32,
            ) -> *const c_void;
            fn CFRelease(cf: *const c_void);
        }

        #[link(name = "IOKit", kind = "framework")]
        extern "C" {
            fn IOPMAssertionCreateWithName(
                assertion_type: *const c_void,
                level: u32,
                name: *const c_void,
                id: *mut u32,
            ) -> i32;
            fn IOPMAssertionRelease(id: u32) -> i32;
        }

        fn cf_string(s: &str) -> Option<*const c_void> {
            let c = CString::new(s).ok()?;
            let cf = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
            (!cf.is_null()).then_some(cf)
        }

        #[derive(Default)]
        pub(super) struct Inhibitor {
            assertion: Option<u32>,
        }

        impl Inhibitor {
            pub(super) fn acquire(&mut self) {
                if self.assertion.is_some() {
                    return;
                }
                let (Some(kind), Some(name)) =
                    (cf_string("PreventUserIdleDisplaySleep"), cf_string(REASON))
                else {
                    log::warn!("could not inhibit idle: CFString creation failed");
                    return;
                };
                let mut id = 0;
                let r =
                    unsafe { IOPMAssertionCreateWithName(kind, ASSERTION_LEVEL_ON, name, &mut id) };
                unsafe {
                    CFRelease(kind);
                    CFRelease(name);
                }
                if r == 0 {
                    log::info!("idle inhibited (IOPMAssertion {id})");
                    self.assertion = Some(id);
                } else {
                    log::warn!("IOPMAssertionCreateWithName failed: {r:#x}");
                }
            }

            pub(super) fn release(&mut self) {
                if let Some(id) = self.assertion.take() {
                    unsafe { IOPMAssertionRelease(id) };
                    log::info!("idle inhibit released");
                }
            }

            /// the assertion needs no refresh
            pub(super) fn next_keepalive(&self) -> Option<Instant> {
                None
            }

            pub(super) fn keepalive(&mut self, _now: Instant) {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the worker loop runs `Debounce` on `RELEASE_DELAY` but reads
    // `std::time::Instant::now()`, which a paused tokio clock does not move,
    // so the release delay is pinned here on the pure state machine
    const D: Duration = RELEASE_DELAY;

    #[test]
    fn acquires_at_once_releases_after_delay() {
        let mut d = Debounce::new(D);
        let t0 = Instant::now();
        assert_eq!(d.request(true, t0), Some(true));
        assert_eq!(d.request(false, t0), None);
        assert_eq!(d.deadline(), Some(t0 + Duration::from_secs(5)));
        assert_eq!(d.poll(t0 + D - Duration::from_millis(1)), None);
        assert_eq!(d.poll(t0 + D), Some(false));
        assert_eq!(d.deadline(), None);
        assert_eq!(d.poll(t0 + D * 2), None);
    }

    #[test]
    fn quick_return_keeps_inhibitor() {
        let mut d = Debounce::new(D);
        let t0 = Instant::now();
        assert_eq!(d.request(true, t0), Some(true));
        assert_eq!(d.request(false, t0 + Duration::from_secs(1)), None);
        // back to the remote before the delay ran out: nothing to do
        assert_eq!(d.request(true, t0 + Duration::from_secs(2)), None);
        assert_eq!(d.deadline(), None);
        assert_eq!(d.poll(t0 + D * 2), None);
    }

    #[test]
    fn repeated_release_does_not_extend_delay() {
        let mut d = Debounce::new(D);
        let t0 = Instant::now();
        d.request(true, t0);
        d.request(false, t0);
        d.request(false, t0 + Duration::from_secs(4));
        assert_eq!(d.poll(t0 + D), Some(false));
    }

    #[test]
    fn release_without_hold_is_noop() {
        let mut d = Debounce::new(D);
        let t0 = Instant::now();
        assert_eq!(d.request(false, t0), None);
        assert_eq!(d.deadline(), None);
        assert_eq!(d.request(true, t0), Some(true));
        assert_eq!(d.request(true, t0), None);
    }
}

//! Desktop notifications: which machine has control, agent sessions,
//! approval waits of the desktop portals and connection attempts from
//! unknown certificates.
//!
//! Notices are handed to a platform worker (a local task on Linux, a thread
//! on Windows and macOS) so the service loop never waits for the desktop.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const APP_NAME: &str = "MiMouse";
/// unauthorized attempts, from any fingerprint, are announced at most once
/// per interval; the next notice counts the ones in between
const UNAUTHORIZED_INTERVAL: Duration = Duration::from_secs(60);
/// the same approval wait is announced at most once per interval
const APPROVAL_INTERVAL: Duration = Duration::from_secs(300);
/// longest agent purpose shown, in characters
const MAX_PURPOSE: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    summary: String,
    body: String,
}

pub(crate) struct Notifier {
    switch_enabled: bool,
    control: ControlTracker,
    local_host: Option<String>,
    unauthorized: Summarized,
    approval: RateLimit,
    backend: backend::Backend,
}

impl Notifier {
    pub(crate) fn new(switch_enabled: bool) -> Self {
        Self {
            switch_enabled,
            control: Default::default(),
            local_host: None,
            unauthorized: Summarized::new(UNAUTHORIZED_INTERVAL),
            approval: RateLimit::new(APPROVAL_INTERVAL),
            backend: backend::Backend::new(),
        }
    }

    /// `notify_switch` from the config; approval waits and unauthorized
    /// attempts are always announced
    pub(crate) fn set_switch_enabled(&mut self, enabled: bool) {
        self.switch_enabled = enabled;
    }

    /// control moved to the remote host `to`, or back to this machine (`None`)
    pub(crate) fn switched(&mut self, to: Option<&str>) {
        if !self.control.update(to) || !self.switch_enabled {
            return;
        }
        let local = self.local_host.get_or_insert_with(local_hostname);
        self.backend.show(Notice {
            summary: switch_text(to, local),
            body: String::new(),
        });
    }

    /// a desktop portal waits for the user to approve `what` (e.g. "input capture")
    pub(crate) fn awaiting_approval(&mut self, what: &str) {
        if self.approval.allow(what, Instant::now()) {
            self.backend.show(approval_notice(what));
        }
    }

    /// a peer with an unknown certificate tried to connect
    pub(crate) fn unauthorized(&mut self, fingerprint: &str) {
        if let Some(notice) = self.unauthorized_notice(fingerprint, Instant::now()) {
            self.backend.show(notice);
        }
    }

    /// the notice for an unauthorized attempt at `now`, if one is due
    fn unauthorized_notice(&mut self, fingerprint: &str, now: Instant) -> Option<Notice> {
        let held_back = self.unauthorized.allow(now)?;
        Some(unauthorized_notice(fingerprint, held_back))
    }

    /// An agent session started (D8: always announced): `who` holds it,
    /// `purpose` is what the agent said it is doing.
    pub(crate) fn agent_started(&mut self, who: &str, purpose: Option<&str>) {
        self.backend.show(agent_started_notice(who, purpose));
    }

    /// the agent session of `who` ended for `reason`
    pub(crate) fn agent_ended(&mut self, who: &str, reason: &str) {
        self.backend.show(Notice {
            summary: format!("Agent control by {who} ended"),
            body: format!("reason: {reason}"),
        });
    }
}

/// Which machine currently has control; filters repeated reports of the same
/// state (e.g. `Enter` repeated until acknowledged, release after release).
#[derive(Default)]
struct ControlTracker {
    /// remote host being controlled, `None` while local
    current: Option<String>,
}

impl ControlTracker {
    /// returns whether this is a change
    fn update(&mut self, to: Option<&str>) -> bool {
        if self.current.as_deref() == to {
            return false;
        }
        self.current = to.map(str::to_owned);
        true
    }
}

/// allows a key once per interval
struct RateLimit {
    interval: Duration,
    last: HashMap<String, Instant>,
}

impl RateLimit {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Default::default(),
        }
    }

    fn allow(&mut self, key: &str, now: Instant) -> bool {
        // expired entries are dropped so unknown peers cannot grow the map
        let interval = self.interval;
        self.last
            .retain(|_, at| now.saturating_duration_since(*at) < interval);
        if self.last.contains_key(key) {
            return false;
        }
        self.last.insert(key.to_owned(), now);
        true
    }
}

/// allows once per interval, counting what it holds back
struct Summarized {
    interval: Duration,
    last: Option<Instant>,
    held_back: u32,
}

impl Summarized {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            held_back: 0,
        }
    }

    /// the number held back since the last allowed one, if this one is allowed
    fn allow(&mut self, now: Instant) -> Option<u32> {
        if self
            .last
            .is_some_and(|at| now.saturating_duration_since(at) < self.interval)
        {
            self.held_back = self.held_back.saturating_add(1);
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held_back))
    }
}

fn switch_text(to: Option<&str>, local: &str) -> String {
    match to {
        Some(host) => format!("Controlling {host}"),
        None => format!("Back on {local}"),
    }
}

fn approval_notice(what: &str) -> Notice {
    Notice {
        summary: "MiMouse is waiting for approval".into(),
        body: format!("Allow {what} in the system dialog."),
    }
}

/// The notice for a started agent session. The purpose comes from the
/// agent: control characters are dropped and it is cut at `MAX_PURPOSE`.
fn agent_started_notice(who: &str, purpose: Option<&str>) -> Notice {
    let purpose: String = purpose
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PURPOSE)
        .collect();
    let mut body = String::from("It can use this machine within its grants.");
    if !purpose.trim().is_empty() {
        body = format!("{}\n{body}", purpose.trim());
    }
    Notice {
        summary: format!("Agent control by {who} started"),
        body,
    }
}

/// the notice for an attempt by `fingerprint`, after `held_back` attempts
/// that got none
fn unauthorized_notice(fingerprint: &str, held_back: u32) -> Notice {
    let mut body = format!(
        "Fingerprint {fingerprint}\nTrust it with: mimouse cli authorize-key <name> {fingerprint}"
    );
    match held_back {
        0 => {}
        1 => body.push_str("\n1 more unauthorized attempt"),
        n => body.push_str(&format!("\n{n} more unauthorized attempts")),
    }
    Notice {
        summary: "Unknown device tried to connect".into(),
        body,
    }
}

fn local_hostname() -> String {
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(windows))]
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        });
    raw.as_deref()
        .and_then(short_hostname)
        .unwrap_or_else(|| "this machine".into())
}

/// first label of a host name (`mac.local` -> `mac`)
fn short_hostname(raw: &str) -> Option<String> {
    let label = raw.trim().split('.').next()?.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

/// org.freedesktop.Notifications over the session bus; one bubble that each
/// notice replaces
#[cfg(all(unix, not(target_os = "macos")))]
mod backend {
    use super::{APP_NAME, Notice};
    use ashpd::zbus::{self, Connection, zvariant::Value};
    use std::collections::HashMap;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    const EXPIRE_MS: i32 = 3000;

    pub(super) struct Backend {
        tx: Option<UnboundedSender<Notice>>,
    }

    impl Backend {
        pub(super) fn new() -> Self {
            Self { tx: None }
        }

        pub(super) fn show(&mut self, notice: Notice) {
            let tx = self.tx.get_or_insert_with(|| {
                let (tx, rx) = unbounded_channel();
                tokio::task::spawn_local(run(rx));
                tx
            });
            let _ = tx.send(notice);
        }
    }

    async fn run(mut rx: UnboundedReceiver<Notice>) {
        let mut conn = None;
        let mut replaces_id = 0;
        let mut warned = false;
        while let Some(notice) = rx.recv().await {
            match notify(&mut conn, replaces_id, &notice).await {
                Ok(id) => {
                    replaces_id = id;
                    log::info!("notification {id}: {}", notice.summary);
                }
                Err(e) if !warned => {
                    warned = true;
                    log::warn!("could not show notification: {e}");
                }
                Err(e) => log::debug!("could not show notification: {e}"),
            }
        }
    }

    async fn notify(
        conn: &mut Option<Connection>,
        replaces_id: u32,
        notice: &Notice,
    ) -> zbus::Result<u32> {
        // reconnect after a failure (session bus or daemon restarted)
        let c = match conn.take() {
            Some(c) => c,
            None => Connection::session().await?,
        };
        let actions: &[&str] = &[];
        let hints = HashMap::from([
            ("transient", Value::from(true)),
            ("urgency", Value::from(1u8)),
        ]);
        let reply = c
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    APP_NAME,
                    replaces_id,
                    "input-mouse",
                    notice.summary.as_str(),
                    notice.body.as_str(),
                    actions,
                    hints,
                    EXPIRE_MS,
                ),
            )
            .await?;
        let id = reply.body().deserialize::<u32>()?;
        *conn = Some(c);
        Ok(id)
    }
}

/// balloon of a tray icon owned by a message-only window on its own thread
#[cfg(windows)]
mod backend {
    use super::{APP_NAME, Notice};
    use std::{
        sync::mpsc::{Receiver, Sender, channel},
        thread::JoinHandle,
    };
    use windows::{
        Win32::UI::{
            Shell::{
                NIF_ICON, NIF_INFO, NIF_REALTIME, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
                NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, HWND_MESSAGE, IDI_APPLICATION, LoadIconW,
                WINDOW_EX_STYLE, WINDOW_STYLE,
            },
        },
        core::w,
    };

    pub(super) struct Backend {
        tx: Option<Sender<Notice>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Backend {
        pub(super) fn new() -> Self {
            Self {
                tx: None,
                thread: None,
            }
        }

        pub(super) fn show(&mut self, notice: Notice) {
            if self.tx.is_none() {
                let (tx, rx) = channel();
                match std::thread::Builder::new()
                    .name("notify".into())
                    .spawn(move || run(rx))
                {
                    Ok(thread) => {
                        self.tx = Some(tx);
                        self.thread = Some(thread);
                    }
                    Err(e) => {
                        log::warn!("could not start notification thread: {e}");
                        return;
                    }
                }
            }
            if let Some(tx) = &self.tx {
                let _ = tx.send(notice);
            }
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            // closing the channel makes the thread remove the tray icon
            self.tx.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn run(rx: Receiver<Notice>) {
        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("MiMouse"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        } {
            Ok(hwnd) => hwnd,
            Err(e) => {
                log::warn!("could not create notification window: {e}");
                return;
            }
        };
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
            ..Default::default()
        };
        super::fill_wide(&mut data.szTip, APP_NAME);
        let mut added = false;
        for notice in rx {
            let info = if notice.body.is_empty() {
                notice.summary
            } else {
                format!("{}\n{}", notice.summary, notice.body)
            };
            super::fill_wide(&mut data.szInfoTitle, APP_NAME);
            super::fill_wide(&mut data.szInfo, &info);
            data.uFlags = NIF_ICON | NIF_TIP | NIF_INFO | NIF_REALTIME;
            data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
            // the icon disappears when explorer restarts: add it again
            let shown = (added && unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool())
                || unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
            added |= shown;
            if shown {
                log::info!("notification shown: {info}");
            } else {
                log::warn!("Shell_NotifyIconW failed: {info}");
            }
        }
        if added {
            data.uFlags = Default::default();
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
        }
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// `osascript display notification`, run one at a time on a thread
#[cfg(target_os = "macos")]
mod backend {
    use super::Notice;
    use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

    /// notices waiting for `osascript`; more are dropped
    const MAX_QUEUED: usize = 4;

    pub(super) struct Backend {
        tx: Option<SyncSender<Notice>>,
    }

    impl Backend {
        pub(super) fn new() -> Self {
            Self { tx: None }
        }

        pub(super) fn show(&mut self, notice: Notice) {
            if self.tx.is_none() {
                let (tx, rx) = sync_channel::<Notice>(MAX_QUEUED);
                let spawned = std::thread::Builder::new()
                    .name("notify".into())
                    .spawn(move || {
                        for notice in rx {
                            let script = super::osascript(&notice);
                            match std::process::Command::new("osascript")
                                .arg("-e")
                                .arg(&script)
                                .status()
                            {
                                Ok(s) if s.success() => {
                                    log::info!("notification shown: {}", notice.summary)
                                }
                                Ok(s) => log::warn!("osascript exited with {s}"),
                                Err(e) => log::warn!("could not run osascript: {e}"),
                            }
                        }
                    });
                match spawned {
                    Ok(_) => self.tx = Some(tx),
                    Err(e) => {
                        log::warn!("could not start notification thread: {e}");
                        return;
                    }
                }
            }
            if let Some(tx) = &self.tx {
                if let Err(TrySendError::Full(notice)) = tx.try_send(notice) {
                    log::debug!(
                        "notification dropped, {MAX_QUEUED} queued: {}",
                        notice.summary
                    );
                }
            }
        }
    }
}

/// copies `s` into a fixed, nul-terminated UTF-16 buffer, truncating
/// without splitting a surrogate pair
#[cfg(any(windows, test))]
fn fill_wide(dst: &mut [u16], s: &str) {
    let Some(max) = dst.len().checked_sub(1) else {
        return;
    };
    let mut len = 0;
    for c in s.chars() {
        let mut buf = [0; 2];
        let units = c.encode_utf16(&mut buf);
        if len + units.len() > max {
            break;
        }
        dst[len..len + units.len()].copy_from_slice(units);
        len += units.len();
    }
    dst[len..].fill(0);
}

/// AppleScript for a notification; all text is passed as quoted literals
#[cfg(any(target_os = "macos", test))]
fn osascript(notice: &Notice) -> String {
    if notice.body.is_empty() {
        format!(
            "display notification {} with title {}",
            applescript_string(&notice.summary),
            applescript_string(APP_NAME)
        )
    } else {
        format!(
            "display notification {} with title {} subtitle {}",
            applescript_string(&notice.body),
            applescript_string(APP_NAME),
            applescript_string(&notice.summary)
        )
    }
}

/// AppleScript string literal: backslash and double quote escaped, line
/// breaks as spaces
#[cfg(any(target_os = "macos", test))]
fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_texts() {
        assert_eq!(
            switch_text(Some("megapc"), "gpumaster"),
            "Controlling megapc"
        );
        assert_eq!(switch_text(None, "gpumaster"), "Back on gpumaster");
    }

    #[test]
    fn agent_purpose_is_cleaned_and_cut() {
        let long = "x".repeat(MAX_PURPOSE + 50);
        let notice = agent_started_notice("laptop", Some(&long));
        assert_eq!(notice.body.lines().next().unwrap().len(), MAX_PURPOSE);
        // a peer cannot forge extra notice lines
        let notice = agent_started_notice("laptop", Some("fix\nthe\u{1b}[2Jbuild"));
        assert_eq!(notice.body.lines().next(), Some("fixthe[2Jbuild"));
        let notice = agent_started_notice("laptop", Some("  "));
        assert_eq!(notice.body.lines().count(), 1);
    }

    #[test]
    fn control_tracker_reports_only_changes() {
        let mut t = ControlTracker::default();
        // starts local: a release while local is not a switch
        assert!(!t.update(None));
        assert!(t.update(Some("megapc")));
        // Enter repeated until acknowledged
        assert!(!t.update(Some("megapc")));
        assert!(t.update(Some("laptop")));
        assert!(t.update(None));
        assert!(!t.update(None));
        assert!(t.update(Some("laptop")));
    }

    #[test]
    fn rate_limit_per_key_and_interval() {
        let mut r = RateLimit::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(r.allow("a", t0));
        assert!(!r.allow("a", t0 + Duration::from_secs(59)));
        assert!(r.allow("b", t0 + Duration::from_secs(1)));
        assert!(r.allow("a", t0 + Duration::from_secs(60)));
        // expired keys are pruned
        assert!(r.allow("c", t0 + Duration::from_secs(200)));
        assert_eq!(r.last.len(), 1);
    }

    #[test]
    fn short_hostnames() {
        assert_eq!(short_hostname("gpumaster\n").as_deref(), Some("gpumaster"));
        assert_eq!(
            short_hostname("Mac-mini.local").as_deref(),
            Some("Mac-mini")
        );
        assert_eq!(short_hostname("  \n"), None);
        assert_eq!(short_hostname(".local"), None);
    }

    #[test]
    fn unauthorized_notice_names_fingerprint_and_fix() {
        let n = unauthorized_notice("ab:cd", 0);
        assert!(n.body.contains("mimouse cli authorize-key <name> ab:cd"));
    }

    /// B5: a fresh certificate per handshake does not yield a notice each;
    /// the next notice counts the attempts in between
    #[test]
    fn unauthorized_notices_are_rate_limited_globally() {
        let mut n = Notifier::new(false);
        let t0 = Instant::now();
        assert!(n.unauthorized_notice("fp0", t0).is_some());
        for i in 1..=100 {
            let at = t0 + Duration::from_millis(i * 100);
            assert_eq!(n.unauthorized_notice(&format!("fp{i}"), at), None);
        }
        let notice = n
            .unauthorized_notice("fp101", t0 + UNAUTHORIZED_INTERVAL)
            .expect("notice after the interval");
        assert!(notice.body.contains("fp101"), "{}", notice.body);
        assert!(
            notice.body.contains("100 more unauthorized attempts"),
            "{}",
            notice.body
        );
        let quiet = t0 + 3 * UNAUTHORIZED_INTERVAL;
        let notice = n.unauthorized_notice("fp0", quiet).expect("notice");
        assert!(!notice.body.contains("more"), "{}", notice.body);
    }

    #[test]
    fn wide_buffer_truncates_and_terminates() {
        let mut buf = [0xffffu16; 4];
        fill_wide(&mut buf, "ab");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, 0, 0]);
        fill_wide(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
        // a surrogate pair that does not fit is dropped whole
        fill_wide(&mut buf, "ab😀");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, 0, 0]);
        let mut empty: [u16; 0] = [];
        fill_wide(&mut empty, "x");
    }

    #[test]
    fn applescript_quoting() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(applescript_string("x\ny"), "\"x y\"");
        let script = osascript(&Notice {
            summary: "Controlling ev\"il".into(),
            body: String::new(),
        });
        assert_eq!(
            script,
            r#"display notification "Controlling ev\"il" with title "MiMouse""#
        );
    }
}

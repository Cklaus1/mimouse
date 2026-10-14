//! Clipboard backend using the xdg-desktop-portal Clipboard interface on a
//! dedicated RemoteDesktop session (works on GNOME, which does not implement
//! the wlr / ext data-control protocols).
//!
//! The session is persisted with a restore token so the permission dialog is
//! only shown once.
//!
//! Copied files are read from and offered as `text/uri-list` and
//! `x-special/gnome-copied-files` (Nautilus).
//!
//! Copies offering `x-kde-passwordManagerHint` are secret and never read; an
//! owner change without mime types is a clear, held back for
//! [`RESTORE_WINDOW`] so that contents restored after a secret copy are
//! dropped.
//!
//! Once clipboard access is granted, the compositor announces the current
//! selection as an owner change (mutter: right away). The first owner change
//! within [`STARTUP_WINDOW`] is taken for it: its contents seed the
//! clipboard state and are not sent, like the contents found by the Windows
//! backend at startup.

use std::{
    borrow::Cow,
    ffi::OsString,
    fmt::Write,
    fs, io,
    os::{
        fd::OwnedFd,
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    time::Duration,
};

use ashpd::desktop::{
    PersistMode, Session,
    clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions},
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use ashpd::enumflags2::BitFlags;
use futures::StreamExt;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    task::{spawn_blocking, spawn_local},
    time::{Instant, sleep_until, timeout},
};

use super::{
    BackendTx, ChangesTx, ClipboardData, LocalChange, MAX_CLIPBOARD_SIZE, MIME_FILES, MIME_HTML,
    MIME_PNG, MIME_TEXT, Offer, SetRx, StatusTx,
    files::{FileRef, cache_dir, local_files},
    set_channel,
};
use crate::ipc::{ClipboardState, ClipboardStatus};

/// file list format of GNOME Files: `copy` or `cut`, then one URI per line
const GNOME_FILES: &str = "x-special/gnome-copied-files";

const RETRY_DELAY: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a local clear is held back. When the clipboard owner goes away
/// (it exits, or a portal client unsets its selection), mutter's clipboard
/// manager re-owns the clipboard right away (within the same signal
/// emission, or 10 ms later for X11 clients) with the text or image it
/// saved from that owner, without the owner's secret marker.
const RESTORE_WINDOW: Duration = Duration::from_millis(300);

/// How long after clipboard access was granted an owner change is taken for
/// the compositor's announcement of the selection it already had (mutter
/// sends it within the same second, smoke on GNOME 50).
const STARTUP_WINDOW: Duration = Duration::from_secs(1);

/// mime types offered for each canonical format, most specific first
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
const HTML_MIMES: &[&str] = &["text/html"];
const PNG_MIMES: &[&str] = &["image/png"];
const FORMATS: &[(&str, &[&str])] = &[
    (MIME_TEXT, TEXT_MIMES),
    (MIME_HTML, HTML_MIMES),
    (MIME_PNG, PNG_MIMES),
];

#[derive(Debug, Error)]
enum PortalError {
    #[error(transparent)]
    Ashpd(#[from] ashpd::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("timed out")]
    Timeout,
    #[error("clipboard access was not granted")]
    NotGranted,
    #[error("portal session closed")]
    Closed,
}

impl From<tokio::time::error::Elapsed> for PortalError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

/// what to do about a change of the clipboard owner
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// nothing for now (a clear is held back)
    Wait,
    /// send a clear instead of the restored contents of a secret copy
    DropRestore,
    /// a copy marked secret: not shared
    Secret,
    /// read the offered formats and send them
    Read,
    /// the selection found when the session started: read it as the
    /// startup contents, which are not sent
    Seed,
    /// the selection found when the session started that is not seeded
    /// (nothing offered, secret, or replaced by ours): nothing
    Ignore,
}

/// the selection the compositor announces when the session starts
struct Startup {
    /// an owner change until then is that announcement
    until: Instant,
    /// whether to seed it; not if we set our own selection at the start
    seed: bool,
}

/// local owner changes; a clear waits for `RESTORE_WINDOW` so that
/// contents restored after a secret copy are dropped
struct OwnerChanges {
    /// the latest offer other than a held-back clear
    last: Offer,
    /// when the held-back clear is due
    clear_at: Option<Instant>,
    /// until the first owner change of the session
    startup: Option<Startup>,
}

impl OwnerChanges {
    /// a session that started at `started`; `seed`: the selection then is
    /// the local clipboard's (we did not set our own)
    fn new(started: Instant, seed: bool) -> Self {
        Self {
            last: Offer::Formats,
            clear_at: None,
            startup: Some(Startup {
                until: started + STARTUP_WINDOW,
                seed,
            }),
        }
    }

    /// another client became the owner at `now`
    fn changed(&mut self, offer: Offer, now: Instant) -> Step {
        if let Some(startup) = self.startup.take().filter(|s| now <= s.until) {
            // a secret found at startup and restored later is still dropped
            self.last = offer;
            return match offer {
                Offer::Formats if startup.seed => Step::Seed,
                _ => Step::Ignore,
            };
        }
        if offer == Offer::Nothing {
            self.clear_at.get_or_insert(now + RESTORE_WINDOW);
            return Step::Wait;
        }
        // a restore keeps `last`, so restoring the secret again is dropped too
        if self.clear_at.take().is_some() && self.last == Offer::Secret {
            return Step::DropRestore;
        }
        self.last = offer;
        match offer {
            Offer::Secret => Step::Secret,
            _ => Step::Read,
        }
    }

    /// whether the held-back clear is due at `now`; it is sent only once
    fn clear_due(&mut self, now: Instant) -> bool {
        if self.clear_at.is_none_or(|at| at > now) {
            return false;
        }
        self.clear_at = None;
        self.last = Offer::Nothing;
        true
    }

    /// we set the clipboard, which voids a held-back clear; a selection
    /// announced for the session start after that is not seeded
    fn set_by_us(&mut self) {
        self.clear_at = None;
        self.last = Offer::Formats;
        if let Some(startup) = &mut self.startup {
            startup.seed = false;
        }
    }
}

/// what to report for `data` read after an owner change noticed at `at`:
/// the startup contents for `Step::Seed` (not sent; nothing if empty), else
/// a change (no supported format: not shared, and not a clear)
fn read_change(step: &Step, at: std::time::Instant, data: ClipboardData) -> Option<LocalChange> {
    let changed = (*step != Step::Seed).then_some(at);
    LocalChange::read(changed, (!data.is_empty()).then_some(data))
}

pub(super) fn spawn(changes: ChangesTx, status: StatusTx) -> BackendTx {
    let (tx, rx) = set_channel();
    spawn_local(run(changes, status, rx));
    tx
}

async fn run(changes: ChangesTx, status: StatusTx, mut set_rx: SetRx) {
    // contents we own (set from a remote device), kept across portal sessions
    let mut owned: Option<Arc<ClipboardData>> = None;
    loop {
        let ended = match session(&changes, &status, &mut set_rx, &mut owned).await {
            Ok(()) => return,
            Err(PortalError::Ashpd(ashpd::Error::Response(e))) => {
                Some(format!("permission request failed: {e}"))
            }
            Err(e @ PortalError::NotGranted) => Some(e.to_string()),
            Err(e) => {
                log::warn!("clipboard portal: {e}, retrying in {RETRY_DELAY:?}");
                status.send_replace(ClipboardStatus::disabled(format!(
                    "{e}, retrying in {RETRY_DELAY:?}"
                )));
                None
            }
        };
        if let Some(reason) = ended {
            log::warn!("clipboard portal: {reason}");
            status.send_replace(ClipboardStatus::disabled(format!(
                "{reason}; restart the service to ask again"
            )));
            return;
        }
        // keep the latest remote contents while waiting
        let mut sleep = pin!(tokio::time::sleep(RETRY_DELAY));
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                data = set_rx.recv() => match data {
                    Some(data) => owned = Some(data),
                    None => return,
                },
            }
        }
    }
}

/// a portal session that can be closed; faked in tests
trait Close {
    async fn close(&self);
}

impl Close for Session<RemoteDesktop> {
    async fn close(&self) {
        if let Err(e) = Session::close(self).await {
            log::debug!("clipboard portal: failed to close the session: {e}");
        }
    }
}

/// runs `body` on `session`, then closes it whatever the outcome, so that
/// errors do not leave sessions behind in the compositor
async fn closing<S: Close, T>(session: S, body: impl AsyncFnOnce(&S) -> T) -> T {
    let result = body(&session).await;
    session.close().await;
    result
}

/// runs one portal session; returns Ok(()) on shutdown
async fn session(
    changes: &ChangesTx,
    status: &StatusTx,
    set_rx: &mut SetRx,
    owned: &mut Option<Arc<ClipboardData>>,
) -> Result<(), PortalError> {
    let remote_desktop = RemoteDesktop::new().await?;
    let clipboard = Clipboard::new().await?;
    let session = remote_desktop.create_session(Default::default()).await?;
    closing(session, async |session| {
        use_session(
            &remote_desktop,
            &clipboard,
            session,
            changes,
            status,
            set_rx,
            owned,
        )
        .await
    })
    .await
}

/// asks for clipboard access on `session`, then shares the clipboard until
/// shutdown (Ok) or a portal error
async fn use_session(
    remote_desktop: &RemoteDesktop,
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    changes: &ChangesTx,
    status: &StatusTx,
    set_rx: &mut SetRx,
    owned: &mut Option<Arc<ClipboardData>>,
) -> Result<(), PortalError> {
    let restore_token = read_token();
    let options = SelectDevicesOptions::default()
        .set_devices(BitFlags::from(DeviceType::Keyboard))
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(session, options).await?;
    clipboard
        .request(session, RequestClipboardOptions::default())
        .await?;
    log::info!("clipboard portal: requesting clipboard access");
    status.send_replace(ClipboardStatus::new(ClipboardState::AwaitingApproval));
    let response = remote_desktop
        .start(session, None, Default::default())
        .await?
        .response()?;
    if let Some(token) = response.restore_token() {
        if let Err(e) = write_token(token) {
            log::warn!("clipboard portal: failed to save restore token: {e}");
        }
    }
    if !response.is_clipboard_enabled() {
        return Err(PortalError::NotGranted);
    }
    log::info!("clipboard portal: clipboard access granted");
    let started = Instant::now();

    let mut owner_changed = pin!(
        clipboard
            .receive_selection_owner_changed::<RemoteDesktop>()
            .await?
    );
    let mut transfers = pin!(
        clipboard
            .receive_selection_transfer::<RemoteDesktop>()
            .await?
    );
    let mut closed = pin!(session.receive_closed().await?);

    if let Some(data) = owned.as_ref() {
        set_selection(clipboard, session, data).await?;
    }
    status.send_replace(ClipboardStatus::new(ClipboardState::Enabled));

    let mut owner_changes = OwnerChanges::new(started, owned.is_none());
    loop {
        let clear_at = owner_changes.clear_at;
        tokio::select! {
            event = owner_changed.next() => {
                let Some((_, event)) = event else { return Err(PortalError::Closed) };
                if event.session_is_owner() == Some(true) {
                    continue;
                }
                let at = std::time::Instant::now();
                let changed = |data| changes.send(LocalChange::Changed(at, data));
                let mime_types = event.mime_types();
                let step = owner_changes.changed(Offer::from_mime_types(mime_types), Instant::now());
                if step == Step::Ignore {
                    continue;
                }
                *owned = None;
                match step {
                    Step::Wait | Step::Ignore => {}
                    Step::DropRestore => {
                        log::debug!("clipboard portal: secret restored by compositor, not sent");
                        let _ = changed(Some(Arc::new(ClipboardData::default())));
                    }
                    Step::Secret => {
                        log::debug!("clipboard portal: not sending a copy marked secret");
                        let _ = changed(None);
                    }
                    Step::Read | Step::Seed => match read_selection(clipboard, session, mime_types).await {
                        Ok(data) => {
                            if data.is_empty() {
                                log::debug!("clipboard portal: no supported format in {mime_types:?}");
                            }
                            if let Some(change) = read_change(&step, at, data) {
                                let _ = changes.send(change);
                            }
                        }
                        Err(e) => log::warn!("clipboard portal: failed to read clipboard: {e}"),
                    },
                }
            }
            _ = async move {
                match clear_at {
                    Some(at) => sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                if owner_changes.clear_due(Instant::now()) {
                    // the clear happened when it was held back
                    let at = (clear_at.expect("due") - RESTORE_WINDOW).into_std();
                    let clear = Some(Arc::new(ClipboardData::default()));
                    let _ = changes.send(LocalChange::Changed(at, clear));
                }
            }
            transfer = transfers.next() => {
                let Some((_, mime, serial)) = transfer else { return Err(PortalError::Closed) };
                let served = match owned.as_ref().and_then(|d| lookup(d, &mime)) {
                    Some(data) => write_selection(clipboard, session, serial, &data).await,
                    None => Err(PortalError::Io(io::ErrorKind::NotFound.into())),
                };
                if let Err(e) = served {
                    log::warn!("clipboard portal: failed to serve {mime}: {e}");
                    clipboard.selection_write_done(session, serial, false).await?;
                }
            }
            data = set_rx.recv() => {
                // only the latest update is kept: every SetSelection cancels
                // transfers requested from the previous one
                let Some(data) = data else { return Ok(()) };
                set_selection(clipboard, session, &data).await?;
                owner_changes.set_by_us();
                *owned = Some(data);
            }
            _ = closed.next() => return Err(PortalError::Closed),
        }
    }
}

/// the data to serve for a requested mime type
fn lookup<'a>(data: &'a ClipboardData, mime: &str) -> Option<Cow<'a, [u8]>> {
    if data.is_empty() {
        return TEXT_MIMES.contains(&mime).then_some(Cow::Borrowed(&[]));
    }
    if !data.files().is_empty() {
        match mime {
            MIME_FILES => return Some(Cow::Owned(uri_list(data.files()))),
            GNOME_FILES => return Some(Cow::Owned(gnome_copied_files(data.files()))),
            _ => {}
        }
    }
    FORMATS
        .iter()
        .find(|(_, mimes)| mimes.contains(&mime))
        .and_then(|(canonical, _)| data.get(canonical))
        .map(Cow::Borrowed)
}

async fn set_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    data: &ClipboardData,
) -> Result<(), PortalError> {
    // a clear is set as empty text: when the selection is unset instead,
    // mutter's clipboard manager restores the previous contents
    let mut mimes: Vec<&str> = if data.is_empty() {
        TEXT_MIMES.to_vec()
    } else {
        FORMATS
            .iter()
            .filter(|(canonical, _)| data.get(canonical).is_some())
            .flat_map(|(_, mimes)| mimes.iter().copied())
            .collect()
    };
    if !data.files().is_empty() {
        mimes.extend([MIME_FILES, GNOME_FILES]);
    }
    let options = SetSelectionOptions::default().set_mime_types(&mimes);
    clipboard.set_selection(session, options).await?;
    Ok(())
}

async fn write_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    serial: u32,
    data: &[u8],
) -> Result<(), PortalError> {
    let fd = OwnedFd::from(clipboard.selection_write(session, serial).await?);
    let mut pipe = pipe::Sender::from_owned_fd(fd)?;
    timeout(IO_TIMEOUT, async {
        pipe.write_all(data).await?;
        pipe.flush().await
    })
    .await??;
    drop(pipe);
    clipboard
        .selection_write_done(session, serial, true)
        .await?;
    Ok(())
}

async fn read_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    offered: &[String],
) -> Result<ClipboardData, PortalError> {
    let mut data = ClipboardData::default();
    let files = match [MIME_FILES, GNOME_FILES]
        .into_iter()
        .find(|m| offered.iter().any(|o| o == m))
    {
        Some(mime) => {
            let paths = parse_uri_list(&read_mime(clipboard, session, mime).await?);
            spawn_blocking(move || local_files(paths))
                .await
                .map_err(io::Error::other)?
        }
        None => Vec::new(),
    };
    for (canonical, mimes) in FORMATS {
        // file managers offer the paths of copied files as text
        if *canonical == MIME_TEXT && !files.is_empty() {
            continue;
        }
        let Some(mime) = mimes.iter().find(|m| offered.iter().any(|o| o == *m)) else {
            continue;
        };
        let bytes = read_mime(clipboard, session, mime).await?;
        let bytes = match *canonical {
            MIME_HTML => normalize_html(bytes),
            MIME_TEXT => strip_nul(bytes),
            _ => bytes,
        };
        if data.size() + bytes.len() > MAX_CLIPBOARD_SIZE {
            log::warn!("clipboard portal: skipping {mime}: clipboard too large");
            continue;
        }
        data.push(canonical, bytes);
    }
    data.set_files(files);
    Ok(data)
}

async fn read_mime(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
) -> Result<Vec<u8>, PortalError> {
    let fd = OwnedFd::from(clipboard.selection_read(session, mime).await?);
    let pipe = pipe::Receiver::from_owned_fd(fd)?;
    let mut buf = Vec::new();
    timeout(
        IO_TIMEOUT,
        pipe.take(MAX_CLIPBOARD_SIZE as u64 + 1)
            .read_to_end(&mut buf),
    )
    .await??;
    Ok(buf)
}

fn strip_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

/// some applications (Firefox) offer text/html as UTF-16
fn normalize_html(bytes: Vec<u8>) -> Vec<u8> {
    let Some(utf16) = bytes.strip_prefix(&[0xff, 0xfe]) else {
        return strip_nul(bytes);
    };
    let units: Vec<u16> = utf16
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    strip_nul(String::from_utf16_lossy(&units).into_bytes())
}

fn token_path() -> PathBuf {
    cache_dir()
        .unwrap_or_default()
        .join("mimouse")
        .join("clipboard.token")
}

fn read_token() -> Option<String> {
    fs::read_to_string(token_path())
        .ok()
        .map(|t| t.trim().to_owned())
}

fn write_token(token: &str) -> io::Result<()> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, token)
}

/// local paths of the `file://` URIs in a `text/uri-list` or
/// `x-special/gnome-copied-files` list; other schemes, remote hosts and
/// comments are ignored
fn parse_uri_list(list: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(list)
        .lines()
        .filter_map(|line| file_uri_path(line.trim()))
        .collect()
}

fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| &uri[5..])?;
    let path = match rest.strip_prefix("//") {
        Some(authority) => {
            let (host, path) = authority.split_at(authority.find('/')?);
            (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)?
        }
        None => rest.starts_with('/').then_some(rest)?,
    };
    Some(PathBuf::from(OsString::from_vec(percent_decode(path)?)))
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        let hi = (bytes.next()? as char).to_digit(16)?;
        let lo = (bytes.next()? as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// `file://` URI of an absolute path
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri
}

fn uri_list(files: &[FileRef]) -> Vec<u8> {
    let mut list = String::new();
    for file in files {
        list.push_str(&file_uri(&file.path));
        list.push_str("\r\n");
    }
    list.into_bytes()
}

fn gnome_copied_files(files: &[FileRef]) -> Vec<u8> {
    let mut list = String::from("copy");
    for file in files {
        list.push('\n');
        list.push_str(&file_uri(&file.path));
    }
    list.into_bytes()
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use tokio::time::Instant;

    use super::{
        super::{ClipboardData, MIME_TEXT, Offer},
        Close, OwnerChanges, PortalError, RESTORE_WINDOW, STARTUP_WINDOW, Step, closing, file_uri,
        parse_uri_list, read_change,
    };

    const MS: Duration = Duration::from_millis(1);

    /// a session past its start
    fn running() -> OwnerChanges {
        let mut changes = OwnerChanges::new(Instant::now(), true);
        changes.startup = None;
        changes
    }

    #[test]
    fn startup_selection_is_the_first_change_within_the_window() {
        let t = Instant::now();
        // nothing or a secret at startup: nothing seeded, no clear held back
        let mut changes = OwnerChanges::new(t, true);
        assert_eq!(changes.changed(Offer::Nothing, t + MS), Step::Ignore);
        assert!(!changes.clear_due(t + RESTORE_WINDOW + MS));
        // a secret found at startup and restored later is dropped
        let mut changes = OwnerChanges::new(t, true);
        assert_eq!(changes.changed(Offer::Secret, t + MS), Step::Ignore);
        assert_eq!(changes.changed(Offer::Nothing, t + 2 * MS), Step::Wait);
        assert_eq!(
            changes.changed(Offer::Formats, t + 3 * MS),
            Step::DropRestore
        );
        // replaced by our own selection: not seeded
        let mut changes = OwnerChanges::new(t, true);
        changes.set_by_us();
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::Ignore);
        let mut changes = OwnerChanges::new(t, false);
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::Ignore);
        // nothing announced within the window: a later copy is read
        let mut changes = OwnerChanges::new(t, true);
        assert_eq!(
            changes.changed(Offer::Formats, t + STARTUP_WINDOW + MS),
            Step::Read
        );
    }

    fn text(s: &str) -> ClipboardData {
        let mut data = ClipboardData::default();
        data.push(MIME_TEXT, s.as_bytes().to_vec());
        data
    }

    /// GNOME smoke: the selection mutter announces when the session starts
    /// seeds the clipboard and is not sent, also not when the cursor enters
    /// a client; the next copy is sent
    #[tokio::test]
    async fn startup_selection_is_not_sent() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut task = super::super::tests::task();
                let handle = task.client_manager.add_client();
                task.client_manager.activate_client(handle);
                let t = Instant::now();
                let at = std::time::Instant::now();
                let mut changes = OwnerChanges::new(t, true);

                let found = changes.changed(Offer::Formats, t + 10 * MS);
                assert_eq!(found, Step::Seed);
                assert!(read_change(&found, at, ClipboardData::default()).is_none());
                task.local_changed(read_change(&found, at, text("old")).expect("seed"));
                task.update_if_stale(handle);
                assert!(task.senders.is_empty(), "startup selection sent");

                let copied = changes.changed(Offer::Formats, t + 20 * MS);
                assert_eq!(copied, Step::Read);
                task.local_changed(read_change(&copied, at, text("new")).expect("change"));
                assert!(task.senders.contains_key(&handle), "copy not sent");
            })
            .await;
    }

    #[test]
    fn restored_secret_is_dropped() {
        let t = Instant::now();
        let mut changes = running();
        assert_eq!(changes.changed(Offer::Secret, t), Step::Secret);
        // the owner exits, mutter restores its text without the marker
        assert_eq!(changes.changed(Offer::Nothing, t), Step::Wait);
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::DropRestore);
        // the clear went out with the dropped restore
        assert!(!changes.clear_due(t + RESTORE_WINDOW));
        // restoring it again is dropped too
        let t = t + 1000 * MS;
        assert_eq!(changes.changed(Offer::Nothing, t), Step::Wait);
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::DropRestore);
        // a new copy is read
        assert_eq!(changes.changed(Offer::Formats, t + 1000 * MS), Step::Read);
    }

    #[test]
    fn restore_after_a_plain_copy_is_read() {
        let t = Instant::now();
        let mut changes = running();
        assert_eq!(changes.changed(Offer::Formats, t), Step::Read);
        assert_eq!(changes.changed(Offer::Nothing, t), Step::Wait);
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::Read);
        assert!(!changes.clear_due(t + RESTORE_WINDOW));
    }

    #[test]
    fn clear_is_sent_once_after_the_window() {
        let t = Instant::now();
        let mut changes = running();
        assert_eq!(changes.changed(Offer::Secret, t), Step::Secret);
        assert_eq!(changes.changed(Offer::Nothing, t), Step::Wait);
        // a second clear does not extend the window
        assert_eq!(changes.changed(Offer::Nothing, t + 100 * MS), Step::Wait);
        assert!(!changes.clear_due(t + RESTORE_WINDOW - MS));
        assert!(changes.clear_due(t + RESTORE_WINDOW));
        assert!(!changes.clear_due(t + 2 * RESTORE_WINDOW));
        // the next owner is a new copy
        assert_eq!(
            changes.changed(Offer::Formats, t + 2 * RESTORE_WINDOW),
            Step::Read
        );
    }

    #[test]
    fn own_selection_voids_a_held_back_clear() {
        let t = Instant::now();
        let mut changes = running();
        assert_eq!(changes.changed(Offer::Secret, t), Step::Secret);
        assert_eq!(changes.changed(Offer::Nothing, t), Step::Wait);
        changes.set_by_us();
        assert!(!changes.clear_due(t + RESTORE_WINDOW));
        assert_eq!(changes.changed(Offer::Formats, t + MS), Step::Read);
    }

    fn paths(list: &str) -> Vec<PathBuf> {
        parse_uri_list(list.as_bytes())
    }

    #[test]
    fn parses_uri_lists() {
        let list = "# comment\r\n\
                    file:///home/u/a%20b.txt\r\n\
                    https://example.com/x\r\n\
                    file://localhost/tmp/gr%C3%BC%C3%9Fe\r\n\
                    file://otherhost/tmp/remote\r\n\
                    sftp://host/tmp/x\r\n\
                    FILE:/tmp/short%25\r\n\
                    \r\n";
        assert_eq!(
            paths(list),
            [
                PathBuf::from("/home/u/a b.txt"),
                PathBuf::from("/tmp/grüße"),
                PathBuf::from("/tmp/short%"),
            ]
        );
    }

    #[test]
    fn parses_gnome_copied_files() {
        assert_eq!(
            paths("copy\nfile:///tmp/a\nfile:///tmp/b%2Fc"),
            [PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b/c")]
        );
        assert_eq!(paths("cut\nfile:///tmp/a"), [PathBuf::from("/tmp/a")]);
    }

    #[test]
    fn rejects_malformed_percent_escapes() {
        assert!(paths("file:///tmp/a%2\nfile:///tmp/a%zz\nfile:///tmp/%+1").is_empty());
    }

    #[test]
    fn non_utf8_paths() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let path = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff x#?%"));
        let uri = file_uri(&path);
        assert_eq!(uri, "file:///tmp/%FF%20x%23%3F%25");
        assert_eq!(paths(&uri), [path]);
    }

    /// a session that counts how often it was closed
    struct FakeSession(std::rc::Rc<std::cell::Cell<usize>>);

    impl Close for FakeSession {
        async fn close(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// B22: a portal error after `create_session` (e.g. the D-Bus call of a
    /// failed serve) closes the session before the retry, as does shutdown
    #[tokio::test]
    async fn session_is_closed_on_every_exit() {
        for fail_at in [None, Some(0), Some(1)] {
            let closed = std::rc::Rc::default();
            let result = closing(FakeSession(std::rc::Rc::clone(&closed)), async |_| {
                for step in 0..2 {
                    if fail_at == Some(step) {
                        Err(PortalError::Closed)?;
                    }
                }
                Ok::<_, PortalError>(())
            })
            .await;
            assert_eq!(result.is_ok(), fail_at.is_none());
            assert_eq!(closed.get(), 1, "fail at {fail_at:?}");
        }
    }
}

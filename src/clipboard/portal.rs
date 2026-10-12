//! Clipboard backend using the xdg-desktop-portal Clipboard interface on a
//! dedicated RemoteDesktop session (works on GNOME, which does not implement
//! the wlr / ext data-control protocols).
//!
//! The session is persisted with a restore token so the permission dialog is
//! only shown once.
//!
//! Copied files are read from and offered as `text/uri-list` and
//! `x-special/gnome-copied-files` (Nautilus).

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
    sync::mpsc,
    task::{spawn_blocking, spawn_local},
    time::timeout,
};

use super::{
    BackendTx, ClipboardData, MAX_CLIPBOARD_SIZE, MIME_FILES, MIME_HTML, MIME_PNG, MIME_TEXT,
    files::{FileRef, cache_dir, local_files},
};

/// file list format of GNOME Files: `copy` or `cut`, then one URI per line
const GNOME_FILES: &str = "x-special/gnome-copied-files";

const RETRY_DELAY: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

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

pub(super) fn spawn(changes: BackendTx) -> BackendTx {
    let (tx, rx) = mpsc::unbounded_channel();
    spawn_local(run(changes, rx));
    tx
}

async fn run(changes: BackendTx, mut set_rx: mpsc::UnboundedReceiver<Arc<ClipboardData>>) {
    // contents we own (set from a remote device), kept across portal sessions
    let mut owned: Option<Arc<ClipboardData>> = None;
    loop {
        match session(&changes, &mut set_rx, &mut owned).await {
            Ok(()) => return,
            Err(PortalError::Ashpd(ashpd::Error::Response(e))) => {
                return log::warn!("clipboard portal: permission request failed: {e}");
            }
            Err(PortalError::NotGranted) => {
                return log::warn!("clipboard portal: clipboard access was not granted");
            }
            Err(e) => log::warn!("clipboard portal: {e}, retrying in {RETRY_DELAY:?}"),
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

/// runs one portal session; returns Ok(()) on shutdown
async fn session(
    changes: &BackendTx,
    set_rx: &mut mpsc::UnboundedReceiver<Arc<ClipboardData>>,
    owned: &mut Option<Arc<ClipboardData>>,
) -> Result<(), PortalError> {
    let remote_desktop = RemoteDesktop::new().await?;
    let clipboard = Clipboard::new().await?;
    let session = remote_desktop.create_session(Default::default()).await?;
    let restore_token = read_token();
    let options = SelectDevicesOptions::default()
        .set_devices(BitFlags::from(DeviceType::Keyboard))
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;
    clipboard
        .request(&session, RequestClipboardOptions::default())
        .await?;
    log::info!("clipboard portal: requesting clipboard access");
    let response = remote_desktop
        .start(&session, None, Default::default())
        .await?
        .response()?;
    if let Some(token) = response.restore_token() {
        if let Err(e) = write_token(token) {
            log::warn!("clipboard portal: failed to save restore token: {e}");
        }
    }
    if !response.is_clipboard_enabled() {
        let _ = session.close().await;
        return Err(PortalError::NotGranted);
    }
    log::info!("clipboard portal: clipboard access granted");

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
        set_selection(&clipboard, &session, data).await?;
    }

    loop {
        tokio::select! {
            event = owner_changed.next() => {
                let Some((_, event)) = event else { return Err(PortalError::Closed) };
                if event.session_is_owner() == Some(true) {
                    continue;
                }
                *owned = None;
                match read_selection(&clipboard, &session, event.mime_types()).await {
                    Ok(data) if !data.is_empty() => {
                        let _ = changes.send(Arc::new(data));
                    }
                    Ok(_) => log::debug!(
                        "clipboard portal: no supported format in {:?}",
                        event.mime_types()
                    ),
                    Err(e) => log::warn!("clipboard portal: failed to read clipboard: {e}"),
                }
            }
            transfer = transfers.next() => {
                let Some((_, mime, serial)) = transfer else { return Err(PortalError::Closed) };
                let served = match owned.as_ref().and_then(|d| lookup(d, &mime)) {
                    Some(data) => write_selection(&clipboard, &session, serial, &data).await,
                    None => Err(PortalError::Io(io::ErrorKind::NotFound.into())),
                };
                if let Err(e) = served {
                    log::warn!("clipboard portal: failed to serve {mime}: {e}");
                    clipboard.selection_write_done(&session, serial, false).await?;
                }
            }
            data = set_rx.recv() => {
                let Some(mut data) = data else {
                    let _ = session.close().await;
                    return Ok(());
                };
                // only the latest update matters: every SetSelection cancels
                // transfers requested from the previous one
                while let Ok(newer) = set_rx.try_recv() {
                    data = newer;
                }
                set_selection(&clipboard, &session, &data).await?;
                *owned = Some(data);
            }
            _ = closed.next() => return Err(PortalError::Closed),
        }
    }
}

/// the data to serve for a requested mime type
fn lookup<'a>(data: &'a ClipboardData, mime: &str) -> Option<Cow<'a, [u8]>> {
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
    let mut mimes: Vec<&str> = FORMATS
        .iter()
        .filter(|(canonical, _)| data.get(canonical).is_some())
        .flat_map(|(_, mimes)| mimes.iter().copied())
        .collect();
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
    use std::path::PathBuf;

    use super::{file_uri, parse_uri_list};

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
}

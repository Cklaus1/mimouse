//! Clipboard sharing.
//!
//! Every local clipboard change is pushed to all active clients over a
//! mutually authenticated TLS connection (TCP, same port number as the DTLS
//! input channel). A client that missed an update (offline, unreachable)
//! receives the current clipboard as soon as the cursor enters it, similar to
//! Deskflow's push-on-enter behaviour.
//!
//! Copies marked secret by a password manager are never sent. Clearing the
//! clipboard (the owner offers no format at all) is sent as an empty update
//! and clears the peers' clipboards; a copy without a supported format is
//! not sent and does not clear them.
//!
//! Copied files (no directories) are sent only to the client the cursor
//! enters, up to `max_file_size` bytes in total; the receiver stores them in
//! its cache directory and puts them on its clipboard as files.
//!
//! The receiving peer must have authorized the sender's certificate
//! fingerprint; the sender accepts the receiver its client is pinned to.

pub(crate) mod files;
mod transport;
mod wire;

#[cfg(all(unix, not(target_os = "macos")))]
mod portal;
#[cfg(windows)]
mod windows;

use std::{
    cell::Cell,
    collections::{HashMap, hash_map::DefaultHasher},
    future::Future,
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
    time::Instant,
};

use crate::ipc::{ClientHandle, ClipboardDirection, ClipboardState, ClipboardStatus};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::crypto::Certificate;

use crate::{client::ClientManager, pair};
use files::FileRef;
use transport::TransportError;

// mime types and accessors below are used by the platform backends only;
// macOS has no clipboard backend yet
/// UTF-8 text
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const MIME_TEXT: &str = "text/plain;charset=utf-8";
/// HTML fragment (UTF-8)
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const MIME_HTML: &str = "text/html";
/// PNG image
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const MIME_PNG: &str = "image/png";
/// copied files; listed in events and served by the Linux backend, the
/// contents travel as files (see `wire`)
pub(crate) const MIME_FILES: &str = "text/uri-list";

/// maximum accepted size of a clipboard update (all formats combined,
/// copied files are limited by `max_file_size` instead)
pub(crate) const MAX_CLIPBOARD_SIZE: usize = 32 * 1024 * 1024;

/// clipboard contents in all formats offered by the source,
/// keyed by one of the canonical mime types above, and copied files
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ClipboardData {
    items: Vec<(String, Vec<u8>)>,
    files: Vec<FileRef>,
}

impl ClipboardData {
    pub(crate) fn push(&mut self, mime: &str, data: Vec<u8>) {
        if !data.is_empty() {
            self.items.push((mime.to_owned(), data));
        }
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn get(&self, mime: &str) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(m, _)| m == mime)
            .map(|(_, d)| d.as_slice())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty() && self.files.is_empty()
    }

    pub(crate) fn set_files(&mut self, files: Vec<FileRef>) {
        self.files = files;
    }

    pub(crate) fn files(&self) -> &[FileRef] {
        &self.files
    }

    /// size of all formats except files
    pub(crate) fn size(&self) -> usize {
        self.items.iter().map(|(m, d)| m.len() + d.len()).sum()
    }

    pub(crate) fn files_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub(crate) fn total_size(&self) -> u64 {
        self.size() as u64 + self.files_size()
    }

    pub(crate) fn mime_types(&self) -> impl Iterator<Item = &str> {
        let files = (!self.files.is_empty()).then_some(MIME_FILES);
        self.items.iter().map(|(m, _)| m.as_str()).chain(files)
    }

    fn digest(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// mime type with which Linux password managers (KeePassXC) mark a copy as
/// secret, value `secret`
const SECRET_MIME: &str = "x-kde-passwordManagerHint";
/// Windows format whose presence keeps a copy from clipboard monitors
const WIN_EXCLUDE_FORMAT: &str = "ExcludeClipboardContentFromMonitorProcessing";
/// Windows format holding a DWORD; zero keeps a copy out of the history
const WIN_HISTORY_FORMAT: &str = "CanIncludeInClipboardHistory";
/// Windows formats that may mark a copy as secret
#[cfg_attr(not(windows), allow(dead_code))]
const WIN_MARKERS: [&str; 2] = [WIN_EXCLUDE_FORMAT, WIN_HISTORY_FORMAT];

/// what the owner of the local clipboard offers; platform-independent so
/// the backends' decisions are tested on every host
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
enum Offer {
    /// no format at all: the clipboard was cleared, sent as an empty update
    Nothing,
    /// marked secret by a password manager: neither read, sent nor stored
    Secret,
    /// formats to read; a copy without any supported format is not sent and
    /// is not a clear either
    Formats,
}

impl Offer {
    /// classify by the mime types offered (xdg-desktop-portal); the secret
    /// hint counts whatever its value, reading it would take a transfer
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    fn from_mime_types(mime_types: &[String]) -> Self {
        if mime_types.is_empty() {
            Self::Nothing
        } else if mime_types.iter().any(|m| m == SECRET_MIME) {
            Self::Secret
        } else {
            Self::Formats
        }
    }

    /// classify by the number of Windows clipboard formats and the marker
    /// formats present (name, contents); a history marker that is not a
    /// nonzero DWORD counts as secret
    #[cfg_attr(not(windows), allow(dead_code))]
    fn from_windows_formats(count: usize, markers: &[(&str, Vec<u8>)]) -> Self {
        let secret = |(name, value): &(&str, Vec<u8>)| match *name {
            WIN_EXCLUDE_FORMAT => true,
            WIN_HISTORY_FORMAT => value
                .first_chunk::<4>()
                .is_none_or(|v| u32::from_le_bytes(*v) == 0),
            _ => false,
        };
        if count == 0 {
            Self::Nothing
        } else if markers.iter().any(secret) {
            Self::Secret
        } else {
            Self::Formats
        }
    }
}

/// clipboard contents with their digest. They are encoded for sending once,
/// by the first client sender that needs it; the other senders share that.
#[derive(Clone)]
struct Snapshot {
    digest: u64,
    data: Arc<ClipboardData>,
    encoded: Rc<tokio::sync::OnceCell<Rc<wire::Encoded>>>,
}

impl Snapshot {
    fn new(data: Arc<ClipboardData>) -> Self {
        Self {
            digest: data.digest(),
            data,
            encoded: Default::default(),
        }
    }

    /// the update to send, at most `max_file_size` bytes of files
    async fn encoded(&self, max_file_size: u64) -> Result<Rc<wire::Encoded>, TransportError> {
        self.encoded_with(|data| async move {
            // copies up to `MAX_CLIPBOARD_SIZE` bytes
            tokio::task::spawn_blocking(move || wire::encode(&data, max_file_size))
                .await
                .map_err(std::io::Error::other)?
        })
        .await
    }

    /// the update made by `encode` on first use; a failed encode is retried
    /// by the next caller
    async fn encoded_with<F>(
        &self,
        encode: impl FnOnce(Arc<ClipboardData>) -> F,
    ) -> Result<Rc<wire::Encoded>, TransportError>
    where
        F: Future<Output = Result<wire::Encoded, TransportError>>,
    {
        let encoded = self
            .encoded
            .get_or_try_init(|| async { encode(self.data.clone()).await.map(Rc::new) });
        encoded.await.cloned()
    }
}

/// digest of the snapshot a client sender is transferring
type Sending = Rc<Cell<Option<u64>>>;

/// a clipboard transfer to or from a peer; contents are never included
pub(crate) struct Transfer {
    pub(crate) direction: ClipboardDirection,
    pub(crate) peer: IpAddr,
    pub(crate) mime: Vec<String>,
    pub(crate) size: usize,
}

impl Transfer {
    fn new(direction: ClipboardDirection, peer: IpAddr, data: &ClipboardData) -> Self {
        Self {
            direction,
            peer,
            mime: data.mime_types().map(str::to_owned).collect(),
            size: usize::try_from(data.total_size()).unwrap_or(usize::MAX),
        }
    }
}

pub(crate) enum ClipboardEvent {
    Transfer(Transfer),
    /// the state of clipboard sharing changed
    Status(ClipboardStatus),
}

/// a client received the clipboard with the given digest
struct Delivered {
    handle: ClientHandle,
    digest: u64,
    event: Transfer,
}

enum Request {
    ClientEntered(ClientHandle),
    ClientRemoved(ClientHandle),
    PortChanged(u16),
}

pub(crate) struct Clipboard {
    request_tx: Option<mpsc::UnboundedSender<Request>>,
    event_rx: Option<mpsc::UnboundedReceiver<Transfer>>,
    status: watch::Receiver<ClipboardStatus>,
    task: Option<JoinHandle<()>>,
}

impl Clipboard {
    /// Clipboard sharing over TLS. The TLS listener also serves pairing
    /// requests (to `pair_tx`), so it runs even with sharing disabled.
    pub(crate) fn new(
        enabled: bool,
        port: u16,
        max_file_size: u64,
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        client_manager: ClientManager,
        pair_tx: mpsc::UnboundedSender<pair::Incoming>,
    ) -> Self {
        let tls = match transport::Tls::new(cert, authorized_keys) {
            Ok(tls) => Rc::new(tls),
            Err(e) => {
                log::warn!("clipboard sharing and pairing unavailable: {e}");
                return Self {
                    request_tx: None,
                    event_rx: None,
                    status: watch::channel(ClipboardStatus::disabled(e.to_string())).1,
                    task: None,
                };
            }
        };
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        if !enabled {
            log::info!("clipboard sharing disabled (set `clipboard = true` to enable)");
            return Self {
                request_tx: Some(request_tx),
                event_rx: None,
                status: watch::channel(ClipboardStatus::new(ClipboardState::Off)).1,
                task: Some(spawn_local(listen_only(port, tls, pair_tx, request_rx))),
            };
        }
        let (delivered_tx, delivered_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ClipboardStatus::new(ClipboardState::Starting));
        let task = ClipboardTask {
            tls,
            max_file_size,
            client_manager,
            current: None,
            seeded: false,
            last_local: None,
            delivered: Default::default(),
            senders: Default::default(),
            delivered_tx,
            event_tx,
        };
        let task = spawn_local(task.run(port, request_rx, delivered_rx, pair_tx, status_tx));
        Self {
            request_tx: Some(request_tx),
            event_rx: Some(event_rx),
            status,
            task: Some(task),
        }
    }

    /// the state of clipboard sharing
    pub(crate) fn status(&self) -> ClipboardStatus {
        self.status.borrow().clone()
    }

    /// the next clipboard transfer or status change
    pub(crate) async fn event(&mut self) -> ClipboardEvent {
        let Self {
            event_rx, status, ..
        } = self;
        let transfer = async {
            match event_rx.as_mut() {
                Some(rx) => match rx.recv().await {
                    Some(event) => event,
                    None => std::future::pending().await,
                },
                None => std::future::pending().await,
            }
        };
        let changed = async {
            match status.changed().await {
                Ok(()) => status.borrow_and_update().clone(),
                // the backend is gone; its last status stays
                Err(_) => std::future::pending().await,
            }
        };
        tokio::select! {
            transfer = transfer => ClipboardEvent::Transfer(transfer),
            status = changed => ClipboardEvent::Status(status),
        }
    }

    /// the cursor entered the given client
    pub(crate) fn client_entered(&self, handle: ClientHandle) {
        if let Some(tx) = &self.request_tx {
            let _ = tx.send(Request::ClientEntered(handle));
        }
    }

    /// the given client was removed: its delivery state is forgotten
    pub(crate) fn client_removed(&self, handle: ClientHandle) {
        if let Some(tx) = &self.request_tx {
            let _ = tx.send(Request::ClientRemoved(handle));
        }
    }

    /// the mimouse port changed
    pub(crate) fn port_changed(&self, port: u16) {
        if let Some(tx) = &self.request_tx {
            let _ = tx.send(Request::PortChanged(port));
        }
    }

    pub(crate) async fn terminate(&mut self) {
        self.request_tx.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// a local clipboard backend: receives clipboard contents to set locally,
/// reports local clipboard changes through the sender passed to `spawn`;
/// closed once the backend ended for good. Only the latest contents are
/// kept until the backend takes them (it may be slow or wait for approval).
type BackendTx = watch::Sender<Option<Arc<ClipboardData>>>;
/// local clipboard changes reported by a backend
type ChangesTx = mpsc::UnboundedSender<LocalChange>;
/// the backend's state, shown by `cli list` and `cli doctor`
type StatusTx = watch::Sender<ClipboardStatus>;

// no backend reports changes on macOS yet
#[cfg_attr(target_os = "macos", allow(dead_code))]
enum LocalChange {
    /// the clipboard changed at the given time (when the backend noticed
    /// it, before reading it): the contents to send (empty for a clear),
    /// or `None` when it holds contents that are not shared (marked
    /// secret, no supported format)
    Changed(Instant, Option<Arc<ClipboardData>>),
    /// the contents when the backend started; not sent
    Startup(Arc<ClipboardData>),
}

impl LocalChange {
    /// what to report for contents read at startup (`changed` is `None`) or
    /// after a change noticed at `changed` (`data` as for `Changed`): the
    /// contents at startup are not sent, and a clipboard empty at startup
    /// was not cleared by anyone
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn read(changed: Option<Instant>, data: Option<ClipboardData>) -> Option<Self> {
        let data = data.map(Arc::new);
        match changed {
            Some(at) => Some(Self::Changed(at, data)),
            None => data.filter(|data| !data.is_empty()).map(Self::Startup),
        }
    }
}

/// receiving end of a backend's set channel
struct SetRx(watch::Receiver<Option<Arc<ClipboardData>>>);

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn set_channel() -> (BackendTx, SetRx) {
    let (tx, rx) = watch::channel(None);
    (tx, SetRx(rx))
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
impl SetRx {
    /// the latest contents to set that were not taken yet; `None` once the
    /// clipboard task is gone
    async fn recv(&mut self) -> Option<Arc<ClipboardData>> {
        loop {
            self.0.changed().await.ok()?;
            if let Some(data) = self.0.borrow_and_update().clone() {
                return Some(data);
            }
        }
    }

    #[cfg(windows)]
    fn blocking_recv(&mut self) -> Option<Arc<ClipboardData>> {
        futures::executor::block_on(self.recv())
    }
}

/// the sequence number to remember as that of our latest write, after a
/// write attempt that changed the clipboard (`wrote`) or not; `now` is the
/// clipboard's sequence number after the attempt
#[cfg_attr(not(windows), allow(dead_code))]
fn own_seq_after_write(previous: u32, wrote: bool, now: u32) -> u32 {
    if wrote { now } else { previous }
}

fn spawn_backend(changes: ChangesTx, status: StatusTx) -> Option<BackendTx> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return Some(portal::spawn(changes, status));
    #[cfg(windows)]
    return Some(windows::spawn(changes, status));
    #[cfg(target_os = "macos")]
    {
        let _ = changes;
        status.send_replace(ClipboardStatus::disabled(
            "not supported on this platform yet",
        ));
        None
    }
}

struct ClipboardTask {
    tls: Rc<transport::Tls>,
    /// largest total size of copied files to send
    max_file_size: u64,
    client_manager: ClientManager,
    /// latest clipboard contents (local or received)
    current: Option<Snapshot>,
    /// `current` is what the clipboard held when the backend started: it is
    /// not sent, also not to clients entered later
    seeded: bool,
    /// when the local clipboard last changed; updates whose connection was
    /// accepted before are older and dropped
    last_local: Option<Instant>,
    /// digest of the clipboard contents each client is known to have
    delivered: HashMap<ClientHandle, u64>,
    /// per-client sender tasks; a watch channel coalesces rapid updates and
    /// keeps them in order. The cell holds the digest being sent until it is
    /// delivered or fails, so a re-enter meanwhile does not send it again
    senders: HashMap<ClientHandle, (watch::Sender<Option<Snapshot>>, Sending)>,
    delivered_tx: mpsc::UnboundedSender<Delivered>,
    event_tx: mpsc::UnboundedSender<Transfer>,
}

/// serve only pairing on the TLS port, following port changes
async fn listen_only(
    mut port: u16,
    tls: Rc<transport::Tls>,
    pair_tx: mpsc::UnboundedSender<pair::Incoming>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    // no clipboard receiver: files are never accepted, so no size limit applies
    let listen = |port| transport::listen(port, tls.clone(), 0, None, pair_tx.clone());
    let mut listener = listen(port);
    while let Some(request) = requests.recv().await {
        match request {
            Request::PortChanged(p) if p != port => {
                port = p;
                listener.abort();
                listener = listen(port);
            }
            Request::PortChanged(_) | Request::ClientEntered(_) | Request::ClientRemoved(_) => {}
        }
    }
    listener.abort();
}

impl ClipboardTask {
    async fn run(
        mut self,
        mut port: u16,
        mut requests: mpsc::UnboundedReceiver<Request>,
        mut delivered_rx: mpsc::UnboundedReceiver<Delivered>,
        pair_tx: mpsc::UnboundedSender<pair::Incoming>,
        status: StatusTx,
    ) {
        let (local_tx, mut local_rx) = mpsc::unbounded_channel();
        let Some(backend) = spawn_backend(local_tx, status) else {
            log::warn!("clipboard sharing is not supported on this platform");
            return listen_only(port, self.tls, pair_tx, requests).await;
        };
        let (received_tx, received_rx) = mpsc::unbounded_channel();
        // dropped once the backend ended: received updates are then refused
        // (not acknowledged); until then they are kept by the backend, also
        // while it waits for approval
        let mut received_rx = Some(received_rx);
        let tls = self.tls.clone();
        let max_file_size = self.max_file_size;
        let listen = |port| {
            transport::listen(
                port,
                tls.clone(),
                max_file_size,
                Some(received_tx.clone()),
                pair_tx.clone(),
            )
        };
        let mut listener = listen(port);

        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    None => break,
                    Some(Request::ClientEntered(handle)) => self.update_if_stale(handle),
                    Some(Request::ClientRemoved(handle)) => self.forget(handle),
                    Some(Request::PortChanged(p)) if p != port => {
                        port = p;
                        listener.abort();
                        listener = listen(port);
                    }
                    Some(Request::PortChanged(_)) => {}
                },
                Some(change) = local_rx.recv() => self.local_changed(change),
                Some(received) = async {
                    match received_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.received(received, &backend),
                () = backend.closed(), if received_rx.is_some() => {
                    log::warn!("clipboard: backend ended, refusing received updates");
                    received_rx = None;
                }
                Some(delivered) = delivered_rx.recv() => {
                    if let Some((_, sending)) = self.senders.get(&delivered.handle) {
                        if sending.get() == Some(delivered.digest) {
                            sending.set(None);
                        }
                    }
                    self.delivered.insert(delivered.handle, delivered.digest);
                    let _ = self.event_tx.send(delivered.event);
                }
            }
        }
        listener.abort();
    }

    fn local_changed(&mut self, change: LocalChange) {
        let (at, change) = match change {
            LocalChange::Startup(data) => {
                // a portal session restart announces the copy still being
                // delivered: it stays a change, not startup contents
                if self
                    .current
                    .as_ref()
                    .is_some_and(|s| s.digest == data.digest())
                {
                    return;
                }
                self.current = Some(Snapshot::new(data));
                self.seeded = true;
                return;
            }
            LocalChange::Changed(at, change) => (at, change),
        };
        self.last_local = Some(at);
        // copying what the clipboard held at startup again is sent
        let seeded = std::mem::take(&mut self.seeded);
        let Some(data) = change else {
            // entering a client must not push the previous contents
            self.current = None;
            return;
        };
        let digest = data.digest();
        if !seeded && self.current.as_ref().is_some_and(|s| s.digest == digest) {
            return;
        }
        if data.is_empty() {
            log::info!("local clipboard cleared");
        } else {
            log::info!(
                "local clipboard changed ({} bytes: {})",
                data.total_size(),
                data.mime_types().collect::<Vec<_>>().join(", ")
            );
        }
        if data.files_size() > self.max_file_size {
            log::warn!(
                "clipboard: copied files are not sent: {} bytes exceed max_file_size ({} bytes)",
                data.files_size(),
                self.max_file_size
            );
            self.current = None;
            return;
        }
        let has_files = !data.files().is_empty();
        self.current = Some(Snapshot::new(data));
        // files go only to the client the cursor enters
        if has_files {
            return;
        }
        for (handle, _, state) in self.client_manager.get_client_states() {
            if state.active {
                self.update(handle);
            }
        }
    }

    fn received(
        &mut self,
        (data, from, fingerprint, accepted): transport::Received,
        backend: &BackendTx,
    ) {
        crate::telemetry::COUNTERS.transfer(&data);
        let _ = self
            .event_tx
            .send(Transfer::new(ClipboardDirection::Received, from, &data));
        let digest = data.digest();
        // the sender already has this clipboard: the client pinned to its
        // certificate, or an unpinned one at its address
        for (handle, config, state) in self.client_manager.get_client_states() {
            let sender = match &config.fingerprint {
                Some(pinned) => *pinned == fingerprint,
                None => {
                    state.ips.contains(&from) || state.active_addr.is_some_and(|a| a.ip() == from)
                }
            };
            if sender {
                self.delivered.insert(handle, digest);
            }
        }
        // a slow transfer must not replace what was copied here meanwhile
        if self.last_local.is_some_and(|local| accepted < local) {
            log::info!("clipboard from {from} dropped: the local clipboard changed since");
            return;
        }
        if self.current.as_ref().is_some_and(|s| s.digest == digest) {
            return;
        }
        self.current = Some(Snapshot::new(data.clone()));
        self.seeded = false;
        let _ = backend.send(Some(data));
    }

    /// send the current clipboard to `handle` unless it already has it
    fn update_if_stale(&mut self, handle: ClientHandle) {
        let Some(Snapshot { digest, .. }) = &self.current else {
            return;
        };
        if !self.seeded && self.delivered.get(&handle) != Some(digest) {
            self.update(handle);
        }
    }

    /// drops what is known about a removed client; its sender ends
    fn forget(&mut self, handle: ClientHandle) {
        self.delivered.remove(&handle);
        self.senders.remove(&handle);
    }

    fn update(&mut self, handle: ClientHandle) {
        let snapshot = self.current.clone();
        if let Some((tx, sending)) = self.senders.get(&handle).filter(|(tx, _)| !tx.is_closed()) {
            // skip if exactly this clipboard is being sent, with nothing
            // else queued behind it
            let digest = snapshot.as_ref().map(|s| s.digest);
            let in_flight = digest.is_some()
                && sending.get() == digest
                && tx.borrow().as_ref().map(|s| s.digest) == digest;
            if !in_flight {
                tx.send_replace(snapshot);
            }
            return;
        }
        let (tx, rx) = watch::channel(snapshot);
        let sending = Rc::new(Cell::new(None));
        spawn_local(client_sender(
            handle,
            rx,
            sending.clone(),
            self.tls.clone(),
            self.client_manager.clone(),
            self.delivered_tx.clone(),
            self.max_file_size,
        ));
        self.senders.insert(handle, (tx, sending));
    }
}

/// sends clipboard updates to a single client, one at a time
async fn client_sender(
    handle: ClientHandle,
    mut rx: watch::Receiver<Option<Snapshot>>,
    sending: Sending,
    tls: Rc<transport::Tls>,
    client_manager: ClientManager,
    delivered_tx: mpsc::UnboundedSender<Delivered>,
    max_file_size: u64,
) {
    loop {
        let snapshot = rx.borrow_and_update().clone();
        if let Some(snapshot) = snapshot {
            let Snapshot { digest, data, .. } = &snapshot;
            match client_addrs(&client_manager, handle) {
                None => return, /* client removed */
                Some(addrs) if addrs.is_empty() => {
                    log::debug!("clipboard: no address known for client {handle}")
                }
                Some(addrs) => {
                    sending.set(Some(*digest));
                    let Some(policy) = client_manager.server_policy(handle) else {
                        return; /* client removed */
                    };
                    let trust =
                        |addr, fingerprint: &str| client_manager.pin(handle, addr, fingerprint);
                    let sent = async {
                        let encoded = snapshot.encoded(max_file_size).await?;
                        transport::send(&tls, &addrs, &encoded, max_file_size, policy, trust).await
                    };
                    match sent.await {
                        Ok(addr) => {
                            log::info!("clipboard sent to {addr} ({} bytes)", data.total_size());
                            crate::telemetry::COUNTERS.transfer(data);
                            let _ = delivered_tx.send(Delivered {
                                handle,
                                digest: *digest,
                                event: Transfer::new(ClipboardDirection::Sent, addr.ip(), data),
                            });
                        }
                        Err(e) => {
                            // a later re-enter retries
                            sending.set(None);
                            log::warn!("could not send clipboard to client {handle}: {e}");
                        }
                    }
                }
            }
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// candidate addresses of a client, the currently active one first
fn client_addrs(client_manager: &ClientManager, handle: ClientHandle) -> Option<Vec<SocketAddr>> {
    let (config, state) = client_manager.get_state(handle)?;
    let mut addrs: Vec<SocketAddr> = Vec::new();
    if let Some(addr) = state.active_addr {
        addrs.push(SocketAddr::new(addr.ip(), config.port));
    }
    for ip in state.ips {
        let addr = SocketAddr::new(ip, config.port);
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    Some(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn portal(mimes: &[&str]) -> Offer {
        Offer::from_mime_types(&mimes.iter().map(|m| m.to_string()).collect::<Vec<_>>())
    }

    fn windows(count: usize, markers: &[(&str, &[u8])]) -> Offer {
        let markers: Vec<_> = markers.iter().map(|(n, v)| (*n, v.to_vec())).collect();
        Offer::from_windows_formats(count, &markers)
    }

    #[test]
    fn portal_secret_marker() {
        // KeePassXC on Linux
        assert_eq!(
            portal(&["text/plain", "x-kde-passwordManagerHint"]),
            Offer::Secret
        );
        assert_eq!(portal(&[MIME_TEXT, "UTF8_STRING"]), Offer::Formats);
    }

    #[test]
    fn portal_clear_is_not_unsupported() {
        assert_eq!(portal(&[]), Offer::Nothing);
        assert_eq!(portal(&["application/x-unsupported"]), Offer::Formats);
    }

    #[test]
    fn windows_secret_markers() {
        // KeePassXC on Windows
        let keepassxc: &[(&str, &[u8])] = &[
            ("ExcludeClipboardContentFromMonitorProcessing", b"1"),
            ("CanIncludeInClipboardHistory", &[0; 4]),
        ];
        assert_eq!(windows(5, keepassxc), Offer::Secret);
        for markers in [
            &[("ExcludeClipboardContentFromMonitorProcessing", &b""[..])][..],
            &[("CanIncludeInClipboardHistory", &[0; 4])],
            // malformed: not a DWORD
            &[("CanIncludeInClipboardHistory", &[1])],
        ] {
            assert_eq!(windows(3, markers), Offer::Secret, "{markers:?}");
        }
        for markers in [
            &[][..],
            &[("CanIncludeInClipboardHistory", &[1, 0, 0, 0][..])],
            &[("CanUploadToCloudClipboard", &[0; 4])],
        ] {
            assert_eq!(windows(3, markers), Offer::Formats, "{markers:?}");
        }
    }

    #[test]
    fn windows_clear_is_not_unsupported() {
        assert_eq!(windows(0, &[]), Offer::Nothing);
        // e.g. only a private format no backend reads
        assert_eq!(windows(1, &[]), Offer::Formats);
    }

    /// C8: a write that did not change the clipboard (it could not be
    /// opened or emptied) is not taken for ours, so the copy the user made
    /// meanwhile is still sent
    #[test]
    fn failed_write_is_not_taken_for_ours() {
        assert_eq!(own_seq_after_write(5, false, 9), 5);
        assert_eq!(own_seq_after_write(5, true, 9), 9);
    }

    pub(super) fn task() -> ClipboardTask {
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        ClipboardTask {
            tls: Rc::new(transport::Tls::new(&cert, Default::default()).unwrap()),
            max_file_size: 0,
            client_manager: Default::default(),
            current: None,
            seeded: false,
            last_local: None,
            delivered: Default::default(),
            senders: Default::default(),
            delivered_tx: mpsc::unbounded_channel().0,
            event_tx: mpsc::unbounded_channel().0,
        }
    }

    fn text(s: &str) -> Arc<ClipboardData> {
        let mut data = ClipboardData::default();
        data.push(MIME_TEXT, s.as_bytes().to_vec());
        Arc::new(data)
    }

    fn from_peer(data: Arc<ClipboardData>, accepted: Instant) -> transport::Received {
        (data, IpAddr::from([10, 0, 0, 2]), "peer".into(), accepted)
    }

    /// B21: a transfer accepted before the latest local copy (a slow file
    /// transfer) does not replace it; one accepted after it does
    #[test]
    fn older_transfer_does_not_replace_a_newer_copy() {
        let mut task = task();
        let (backend, mut set) = set_channel();
        let accepted = Instant::now();
        let copied = accepted + std::time::Duration::from_millis(1);
        task.local_changed(LocalChange::Changed(copied, Some(text("fresh"))));

        task.received(from_peer(text("slow"), accepted), &backend);
        assert!(set.recv().now_or_never().is_none(), "older transfer set");

        let later = copied + std::time::Duration::from_millis(1);
        task.received(from_peer(text("newer"), later), &backend);
        assert_eq!(set.recv().now_or_never().flatten(), Some(text("newer")));
    }

    /// C6: of the updates received before the backend reads, only the
    /// latest is set; the backend ending is still noticed
    #[tokio::test]
    async fn backend_sets_only_the_latest() {
        let mut task = task();
        let (backend, mut set) = set_channel();
        let accepted = Instant::now();
        for i in 0..100 {
            task.received(from_peer(text(&i.to_string()), accepted), &backend);
        }
        assert_eq!(set.recv().await, Some(text("99")));
        assert!(set.recv().now_or_never().is_none(), "older contents queued");

        drop(set);
        assert!(
            backend.closed().now_or_never().is_some(),
            "ended backend not noticed"
        );
    }

    /// C7: the client senders a copy goes to share one encoding of it
    #[tokio::test]
    async fn senders_share_one_encode() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut task = task();
                let handles: Vec<_> = (0..3)
                    .map(|_| {
                        let handle = task.client_manager.add_client();
                        task.client_manager.activate_client(handle);
                        handle
                    })
                    .collect();
                task.local_changed(LocalChange::Changed(Instant::now(), Some(text("copied"))));
                let encodes = Cell::new(0);
                for handle in &handles {
                    let snapshot = task.senders[handle].0.borrow().clone().expect("sent");
                    let encoded = snapshot.encoded_with(|data| {
                        encodes.set(encodes.get() + 1);
                        async move { wire::encode(&data, 0) }
                    });
                    encoded.await.expect("encoded");
                }
                assert_eq!(encodes.get(), 1);
            })
            .await;
    }

    /// B23: what the clipboard holds when the service starts is not sent,
    /// neither at once nor when the cursor enters a client; copying it
    /// again is
    #[tokio::test]
    async fn startup_contents_are_not_sent() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut task = task();
                let handle = task.client_manager.add_client();
                task.client_manager.activate_client(handle);
                assert!(LocalChange::read(None, None).is_none());
                assert!(LocalChange::read(None, Some(Default::default())).is_none());

                let startup = LocalChange::read(None, Some((*text("old")).clone()));
                task.local_changed(startup.expect("seed"));
                task.update_if_stale(handle);
                assert!(task.senders.is_empty(), "startup contents sent");

                let again = LocalChange::read(Some(Instant::now()), Some((*text("old")).clone()));
                task.local_changed(again.expect("change"));
                assert!(task.senders.contains_key(&handle), "copy not sent");
            })
            .await;
    }

    /// B23 is only the service start: a portal session restart announcing
    /// the copy still being delivered does not make it startup contents
    #[tokio::test]
    async fn session_restart_keeps_the_copy_pending() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut task = task();
                let handle = task.client_manager.add_client();
                task.local_changed(LocalChange::Changed(Instant::now(), Some(text("x"))));
                assert!(task.senders.is_empty(), "sent to an inactive client");

                task.local_changed(LocalChange::Startup(text("x")));
                task.client_manager.activate_client(handle);
                task.update_if_stale(handle);
                assert!(task.senders.contains_key(&handle), "copy not sent on enter");
            })
            .await;
    }

    /// B8: once a client is removed, what it was known to have is forgotten,
    /// so a client given its handle does not inherit it
    #[tokio::test]
    async fn removed_client_is_forgotten() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut task = task();
                let (backend, _set) = set_channel();
                let handle = task.client_manager.add_client();
                task.client_manager
                    .set_fix_ips(handle, vec![IpAddr::from([10, 0, 0, 2])]);
                // the client sent this clipboard, so it has it
                task.received(from_peer(text("theirs"), Instant::now()), &backend);
                task.update_if_stale(handle);
                assert!(task.senders.is_empty(), "sent back to its sender");

                task.forget(handle);
                task.update_if_stale(handle);
                assert!(task.senders.contains_key(&handle), "delivery inherited");
                task.forget(handle);
                assert!(task.senders.is_empty(), "sender kept");
            })
            .await;
    }
}

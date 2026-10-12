//! Clipboard sharing.
//!
//! Every local clipboard change is pushed to all active clients over a
//! mutually authenticated TLS connection (TCP, same port number as the DTLS
//! input channel). A client that missed an update (offline, unreachable)
//! receives the current clipboard as soon as the cursor enters it, similar to
//! Deskflow's push-on-enter behaviour.
//!
//! Copied files (no directories) are sent only to the client the cursor
//! enters, up to `max_file_size` bytes in total; the receiver stores them in
//! its cache directory and puts them on its clipboard as files.
//!
//! Both peers must have authorized each other's certificate fingerprint.

mod files;
mod transport;
mod wire;

#[cfg(all(unix, not(target_os = "macos")))]
mod portal;
#[cfg(windows)]
mod windows;

use std::{
    cell::Cell,
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
};

use crate::ipc::{ClientHandle, ClipboardDirection};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::crypto::Certificate;

use crate::{client::ClientManager, pair};
use files::FileRef;

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

    #[cfg_attr(target_os = "macos", allow(dead_code))]
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

type Snapshot = (u64, Arc<ClipboardData>);

/// a clipboard transfer to or from a peer; contents are never included
pub(crate) struct ClipboardEvent {
    pub(crate) direction: ClipboardDirection,
    pub(crate) peer: IpAddr,
    pub(crate) mime: Vec<String>,
    pub(crate) size: usize,
}

impl ClipboardEvent {
    fn new(direction: ClipboardDirection, peer: IpAddr, data: &ClipboardData) -> Self {
        Self {
            direction,
            peer,
            mime: data.mime_types().map(str::to_owned).collect(),
            size: usize::try_from(data.total_size()).unwrap_or(usize::MAX),
        }
    }
}

/// a client received the clipboard with the given digest
struct Delivered {
    handle: ClientHandle,
    digest: u64,
    event: ClipboardEvent,
}

enum Request {
    ClientEntered(ClientHandle),
    PortChanged(u16),
}

pub(crate) struct Clipboard {
    request_tx: Option<mpsc::UnboundedSender<Request>>,
    event_rx: Option<mpsc::UnboundedReceiver<ClipboardEvent>>,
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
                task: Some(spawn_local(listen_only(port, tls, pair_tx, request_rx))),
            };
        }
        let (delivered_tx, delivered_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task = ClipboardTask {
            tls,
            max_file_size,
            client_manager,
            current: None,
            delivered: Default::default(),
            senders: Default::default(),
            delivered_tx,
            event_tx,
        };
        let task = spawn_local(task.run(port, request_rx, delivered_rx, pair_tx));
        Self {
            request_tx: Some(request_tx),
            event_rx: Some(event_rx),
            task: Some(task),
        }
    }

    /// the next clipboard transfer; never resolves while sharing is disabled
    pub(crate) async fn event(&mut self) -> ClipboardEvent {
        match self.event_rx.as_mut() {
            Some(rx) => match rx.recv().await {
                Some(event) => event,
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }

    /// the cursor entered the given client
    pub(crate) fn client_entered(&self, handle: ClientHandle) {
        if let Some(tx) = &self.request_tx {
            let _ = tx.send(Request::ClientEntered(handle));
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
/// reports local clipboard changes through the sender passed to `spawn`
type BackendTx = mpsc::UnboundedSender<Arc<ClipboardData>>;

fn spawn_backend(changes: BackendTx) -> Option<BackendTx> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return Some(portal::spawn(changes));
    #[cfg(windows)]
    return Some(windows::spawn(changes));
    #[cfg(target_os = "macos")]
    {
        let _ = changes;
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
    /// digest of the clipboard contents each client is known to have
    delivered: HashMap<ClientHandle, u64>,
    /// per-client sender tasks; a watch channel coalesces rapid updates and
    /// keeps them in order. The cell holds the digest being sent until it is
    /// delivered or fails, so a re-enter meanwhile does not send it again
    senders: HashMap<ClientHandle, (watch::Sender<Option<Snapshot>>, Rc<Cell<Option<u64>>>)>,
    delivered_tx: mpsc::UnboundedSender<Delivered>,
    event_tx: mpsc::UnboundedSender<ClipboardEvent>,
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
            Request::PortChanged(_) | Request::ClientEntered(_) => {}
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
    ) {
        let (local_tx, mut local_rx) = mpsc::unbounded_channel();
        let Some(backend) = spawn_backend(local_tx) else {
            log::warn!("clipboard sharing is not supported on this platform");
            return listen_only(port, self.tls, pair_tx, requests).await;
        };
        let (received_tx, mut received_rx) = mpsc::unbounded_channel();
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
                    Some(Request::PortChanged(p)) if p != port => {
                        port = p;
                        listener.abort();
                        listener = listen(port);
                    }
                    Some(Request::PortChanged(_)) => {}
                },
                Some(data) = local_rx.recv() => self.local_changed(data),
                Some((data, ip)) = received_rx.recv() => self.received(data, ip, &backend),
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

    fn local_changed(&mut self, data: Arc<ClipboardData>) {
        let digest = data.digest();
        if self.current.as_ref().is_some_and(|(d, _)| *d == digest) {
            return;
        }
        log::info!(
            "local clipboard changed ({} bytes: {})",
            data.total_size(),
            data.mime_types().collect::<Vec<_>>().join(", ")
        );
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
        self.current = Some((digest, data));
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

    fn received(&mut self, data: Arc<ClipboardData>, from: IpAddr, backend: &BackendTx) {
        let _ = self.event_tx.send(ClipboardEvent::new(
            ClipboardDirection::Received,
            from,
            &data,
        ));
        let digest = data.digest();
        // the sender already has this clipboard
        for (handle, _, state) in self.client_manager.get_client_states() {
            if state.ips.contains(&from) || state.active_addr.is_some_and(|a| a.ip() == from) {
                self.delivered.insert(handle, digest);
            }
        }
        if self.current.as_ref().is_some_and(|(d, _)| *d == digest) {
            return;
        }
        self.current = Some((digest, data.clone()));
        let _ = backend.send(data);
    }

    /// send the current clipboard to `handle` unless it already has it
    fn update_if_stale(&mut self, handle: ClientHandle) {
        let Some((digest, _)) = &self.current else {
            return;
        };
        if self.delivered.get(&handle) != Some(digest) {
            self.update(handle);
        }
    }

    fn update(&mut self, handle: ClientHandle) {
        let snapshot = self.current.clone();
        if let Some((tx, sending)) = self.senders.get(&handle).filter(|(tx, _)| !tx.is_closed()) {
            // skip if exactly this clipboard is being sent, with nothing
            // else queued behind it
            let digest = snapshot.as_ref().map(|(d, _)| *d);
            let in_flight = digest.is_some()
                && sending.get() == digest
                && tx.borrow().as_ref().map(|(d, _)| *d) == digest;
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
    sending: Rc<Cell<Option<u64>>>,
    tls: Rc<transport::Tls>,
    client_manager: ClientManager,
    delivered_tx: mpsc::UnboundedSender<Delivered>,
    max_file_size: u64,
) {
    loop {
        let snapshot = rx.borrow_and_update().clone();
        if let Some((digest, data)) = snapshot {
            match client_addrs(&client_manager, handle) {
                None => return, /* client removed */
                Some(addrs) if addrs.is_empty() => {
                    log::debug!("clipboard: no address known for client {handle}")
                }
                Some(addrs) => {
                    sending.set(Some(digest));
                    match transport::send(&tls, &addrs, data.clone(), max_file_size).await {
                        Ok(addr) => {
                            log::info!("clipboard sent to {addr} ({} bytes)", data.total_size());
                            let _ = delivered_tx.send(Delivered {
                                handle,
                                digest,
                                event: ClipboardEvent::new(
                                    ClipboardDirection::Sent,
                                    addr.ip(),
                                    &data,
                                ),
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

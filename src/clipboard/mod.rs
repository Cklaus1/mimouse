//! Clipboard sharing.
//!
//! Every local clipboard change is pushed to all active clients over a
//! mutually authenticated TLS connection (TCP, same port number as the DTLS
//! input channel). A client that missed an update (offline, unreachable)
//! receives the current clipboard as soon as the cursor enters it, similar to
//! Deskflow's push-on-enter behaviour.
//!
//! Both peers must have authorized each other's certificate fingerprint.

mod transport;

#[cfg(all(unix, not(target_os = "macos")))]
mod portal;
#[cfg(windows)]
mod windows;

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
};

use crate::ipc::ClientHandle;
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::crypto::Certificate;

use crate::client::ClientManager;

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

/// maximum accepted size of a clipboard update (all formats combined)
pub(crate) const MAX_CLIPBOARD_SIZE: usize = 32 * 1024 * 1024;

/// clipboard contents in all formats offered by the source,
/// keyed by one of the canonical mime types above
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ClipboardData {
    items: Vec<(String, Vec<u8>)>,
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
        self.items.is_empty()
    }

    pub(crate) fn size(&self) -> usize {
        self.items.iter().map(|(m, d)| m.len() + d.len()).sum()
    }

    pub(crate) fn mime_types(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|(m, _)| m.as_str())
    }

    fn digest(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

type Snapshot = (u64, Arc<ClipboardData>);

enum Request {
    ClientEntered(ClientHandle),
    PortChanged(u16),
}

pub(crate) struct Clipboard {
    request_tx: Option<mpsc::UnboundedSender<Request>>,
    task: Option<JoinHandle<()>>,
}

impl Clipboard {
    pub(crate) fn new(
        enabled: bool,
        port: u16,
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        client_manager: ClientManager,
    ) -> Self {
        let disabled = Self {
            request_tx: None,
            task: None,
        };
        if !enabled {
            log::info!("clipboard sharing disabled (set `clipboard = true` to enable)");
            return disabled;
        }
        let tls = match transport::Tls::new(cert, authorized_keys) {
            Ok(tls) => Rc::new(tls),
            Err(e) => {
                log::warn!("clipboard sharing unavailable: {e}");
                return disabled;
            }
        };
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (delivered_tx, delivered_rx) = mpsc::unbounded_channel();
        let task = ClipboardTask {
            tls,
            client_manager,
            current: None,
            delivered: Default::default(),
            senders: Default::default(),
            delivered_tx,
        };
        let task = spawn_local(task.run(port, request_rx, delivered_rx));
        Self {
            request_tx: Some(request_tx),
            task: Some(task),
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
    client_manager: ClientManager,
    /// latest clipboard contents (local or received)
    current: Option<Snapshot>,
    /// digest of the clipboard contents each client is known to have
    delivered: HashMap<ClientHandle, u64>,
    /// per-client sender tasks; a watch channel coalesces rapid updates and
    /// keeps them in order
    senders: HashMap<ClientHandle, watch::Sender<Option<Snapshot>>>,
    delivered_tx: mpsc::UnboundedSender<(ClientHandle, u64)>,
}

impl ClipboardTask {
    async fn run(
        mut self,
        mut port: u16,
        mut requests: mpsc::UnboundedReceiver<Request>,
        mut delivered_rx: mpsc::UnboundedReceiver<(ClientHandle, u64)>,
    ) {
        let (local_tx, mut local_rx) = mpsc::unbounded_channel();
        let Some(backend) = spawn_backend(local_tx) else {
            log::warn!("clipboard sharing is not supported on this platform");
            return;
        };
        let (received_tx, mut received_rx) = mpsc::unbounded_channel();
        let mut listener = transport::listen(port, self.tls.clone(), received_tx.clone());

        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    None => break,
                    Some(Request::ClientEntered(handle)) => self.update_if_stale(handle),
                    Some(Request::PortChanged(p)) if p != port => {
                        port = p;
                        listener.abort();
                        listener = transport::listen(port, self.tls.clone(), received_tx.clone());
                    }
                    Some(Request::PortChanged(_)) => {}
                },
                Some(data) = local_rx.recv() => self.local_changed(data),
                Some((data, ip)) = received_rx.recv() => self.received(data, ip, &backend),
                Some((handle, digest)) = delivered_rx.recv() => {
                    self.delivered.insert(handle, digest);
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
            data.size(),
            data.mime_types().collect::<Vec<_>>().join(", ")
        );
        self.current = Some((digest, data));
        for (handle, _, state) in self.client_manager.get_client_states() {
            if state.active {
                self.update(handle);
            }
        }
    }

    fn received(&mut self, data: Arc<ClipboardData>, from: IpAddr, backend: &BackendTx) {
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
        if let Some(tx) = self.senders.get(&handle).filter(|tx| !tx.is_closed()) {
            tx.send_replace(snapshot);
            return;
        }
        let (tx, rx) = watch::channel(snapshot);
        spawn_local(client_sender(
            handle,
            rx,
            self.tls.clone(),
            self.client_manager.clone(),
            self.delivered_tx.clone(),
        ));
        self.senders.insert(handle, tx);
    }
}

/// sends clipboard updates to a single client, one at a time
async fn client_sender(
    handle: ClientHandle,
    mut rx: watch::Receiver<Option<Snapshot>>,
    tls: Rc<transport::Tls>,
    client_manager: ClientManager,
    delivered_tx: mpsc::UnboundedSender<(ClientHandle, u64)>,
) {
    loop {
        let snapshot = rx.borrow_and_update().clone();
        if let Some((digest, data)) = snapshot {
            match client_addrs(&client_manager, handle) {
                None => return, /* client removed */
                Some(addrs) if addrs.is_empty() => {
                    log::debug!("clipboard: no address known for client {handle}")
                }
                Some(addrs) => match transport::send(&tls, &addrs, &data).await {
                    Ok(addr) => {
                        log::info!("clipboard sent to {addr} ({} bytes)", data.size());
                        let _ = delivered_tx.send((handle, digest));
                    }
                    Err(e) => log::warn!("could not send clipboard to client {handle}: {e}"),
                },
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

/// wire format of a clipboard update:
/// `"LMCB" | version: u8 | count: u8 | count * (mime_len: u8 | mime | len: u32 BE | data)`
mod wire {
    use super::{ClipboardData, MAX_CLIPBOARD_SIZE};

    const MAGIC: &[u8; 4] = b"LMCB";
    const VERSION: u8 = 1;

    pub(super) fn encode(data: &ClipboardData) -> Vec<u8> {
        let mut buf = Vec::with_capacity(data.size() + 6 + 5 * data.items.len());
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        let items: Vec<_> = data
            .items
            .iter()
            .filter(|(mime, _)| mime.len() <= u8::MAX as usize)
            .take(u8::MAX as usize)
            .collect();
        buf.push(items.len() as u8);
        for (mime, bytes) in items {
            buf.push(mime.len() as u8);
            buf.extend_from_slice(mime.as_bytes());
            buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            buf.extend_from_slice(bytes);
        }
        buf
    }

    pub(super) fn decode(buf: &[u8]) -> Option<ClipboardData> {
        let rest = buf.strip_prefix(MAGIC)?;
        let (&version, rest) = rest.split_first()?;
        if version != VERSION {
            return None;
        }
        let (&count, mut rest) = rest.split_first()?;
        let mut data = ClipboardData::default();
        for _ in 0..count {
            let (&mime_len, r) = rest.split_first()?;
            let (mime, r) = r.split_at_checked(mime_len as usize)?;
            let (len, r) = r.split_first_chunk::<4>()?;
            let (bytes, r) = r.split_at_checked(u32::from_be_bytes(*len) as usize)?;
            rest = r;
            data.push(std::str::from_utf8(mime).ok()?, bytes.to_vec());
        }
        (rest.is_empty() && data.size() <= MAX_CLIPBOARD_SIZE).then_some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClipboardData, MIME_HTML, MIME_PNG, MIME_TEXT, wire};

    fn sample() -> ClipboardData {
        let mut data = ClipboardData::default();
        data.push(MIME_TEXT, "grüße".into());
        data.push(MIME_HTML, "<b>x</b>".into());
        data.push(MIME_PNG, vec![0x89, b'P', b'N', b'G', 0, 0xff]);
        data
    }

    #[test]
    fn roundtrip() {
        let data = sample();
        assert_eq!(wire::decode(&wire::encode(&data)), Some(data));
    }

    #[test]
    fn rejects_truncated_and_trailing_bytes() {
        let encoded = wire::encode(&sample());
        for len in 0..encoded.len() {
            assert_eq!(wire::decode(&encoded[..len]), None, "prefix of {len} bytes");
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(wire::decode(&trailing), None);
    }

    #[test]
    fn rejects_unknown_version() {
        let mut encoded = wire::encode(&sample());
        encoded[4] = 2;
        assert_eq!(wire::decode(&encoded), None);
    }
}

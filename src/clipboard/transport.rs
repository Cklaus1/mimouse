//! TLS over TCP transport for clipboard updates.
//!
//! Uses the mimouse DTLS certificate on both ends. The server accepts only
//! certificates in `authorized_fingerprints`; the client accepts the server
//! its client is pinned to, trusting it on first use (`crypto::peers`).
//!
//! One connection carries one update: the client sends a frame
//! (`len: u32 BE | payload`, payload see `wire`), the server answers with a
//! single ack byte once the update (including copied files) is stored and
//! handed to the clipboard. A connection whose peer's fingerprint gets
//! removed from `authorized_fingerprints` is closed without an ack, as is
//! every update once the local clipboard backend has ended.
//!
//! Pairing connections (`crate::pair`) arrive on the same listener and are
//! told apart by their ALPN protocol before the handshake completes.

use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use rustls_pki_types::{CertificateDer, ServerName};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit, mpsc},
    task::{JoinHandle, spawn_blocking, spawn_local},
    time::timeout,
};
use tokio_rustls::{LazyConfigAcceptor, StartHandshake, TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;

use super::{
    ClipboardData,
    files::{FileStore, Receiving},
    wire,
};
use crate::{
    crypto::{self, peers::Policy},
    pair,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// slowest link a large transfer is given time for, in bytes per second
const MIN_TRANSFER_RATE: u64 = 1024 * 1024;
const ACK: u8 = 1;
/// live inbound connections, authenticated or not (pairing included); more
/// are closed at once
const MAX_CONNECTIONS: usize = 16;
/// inbound updates read at once; each buffers up to `MAX_CLIPBOARD_SIZE`
/// of formats (files are streamed to disk), so this bounds that memory to
/// 128 MiB
const MAX_TRANSFERS: usize = 4;
/// inbound updates read at once from one peer: its updates are read in
/// order, and it cannot take every slot of `MAX_TRANSFERS`
const MAX_PEER_TRANSFERS: usize = 1;
/// how often an open connection checks that its peer is still authorized
const REVOCATION_CHECK: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    #[error("timed out")]
    Timeout,
    #[error("clipboard update too large ({0} bytes)")]
    TooLarge(u64),
    #[error("too many files ({0})")]
    TooManyFiles(usize),
    #[error("file name cannot be sent: {}", .0.display())]
    FileName(PathBuf),
    #[error("no cache directory for received files (HOME / LOCALAPPDATA unset)")]
    NoFileStore,
    #[error("invalid clipboard update")]
    Invalid,
    #[error("a copied file changed while it was sent")]
    FileChanged,
    #[error("peer did not acknowledge the update")]
    NoAck,
    #[error("no address reachable")]
    Unreachable,
    #[error("server certificate {0} is not the one pinned for this client")]
    Untrusted(String),
    #[error("peer presented no certificate")]
    NoCertificate,
    #[error("certificate {0} is no longer authorized")]
    Revoked(String),
    #[error("the local clipboard backend has ended")]
    BackendEnded,
}

impl From<tokio::time::error::Elapsed> for TransportError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

/// a clipboard update received from the peer at an address, with the
/// fingerprint of its certificate and when its connection was accepted
pub(crate) type Received = (Arc<ClipboardData>, IpAddr, String, Instant);

pub(crate) struct Tls {
    acceptor: TlsAcceptor,
    cert: Certificate,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
}

impl Tls {
    pub(crate) fn new(
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, TransportError> {
        let server =
            crypto::peers::tls_server_config(cert, Policy::Authorized(authorized_keys.clone()))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(server)),
            cert: cert.clone(),
            authorized_keys,
        })
    }

    /// the name `fingerprint` is authorized as, else the fingerprint
    fn describe(&self, fingerprint: &str) -> String {
        let keys = self.authorized_keys.read().expect("lock");
        keys.get(fingerprint)
            .cloned()
            .unwrap_or_else(|| fingerprint.to_owned())
    }
}

fn fingerprint(certs: Option<&[CertificateDer]>) -> Result<String, TransportError> {
    certs
        .and_then(|c| c.first())
        .map(|c| crypto::generate_fingerprint(c))
        .ok_or(TransportError::NoCertificate)
}

/// time allowed to transfer a frame of `len` bytes
fn transfer_timeout(len: u64) -> Duration {
    TRANSFER_TIMEOUT + Duration::from_secs(len / MIN_TRANSFER_RATE)
}

/// send the update `encoded` to the first reachable address whose server
/// `policy` and `trust` (given the address and the server's fingerprint)
/// accept; copied files are read when sending and may total at most
/// `max_file_size` bytes
pub(crate) async fn send(
    tls: &Tls,
    addrs: &[SocketAddr],
    encoded: &wire::Encoded,
    max_file_size: u64,
    policy: Policy,
    trust: impl Fn(SocketAddr, &str) -> bool,
) -> Result<SocketAddr, TransportError> {
    let connector = TlsConnector::from(Arc::new(crypto::peers::tls_client_config(
        &tls.cert, policy,
    )?));
    let mut last_err = TransportError::Unreachable;
    for &addr in addrs {
        match send_to(&connector, addr, encoded, max_file_size, &trust).await {
            Ok(()) => return Ok(addr),
            Err(e) => {
                log::debug!("clipboard: {addr}: {e}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

async fn send_to(
    connector: &TlsConnector,
    addr: SocketAddr,
    encoded: &wire::Encoded,
    max_file_size: u64,
    trust: impl Fn(SocketAddr, &str) -> bool,
) -> Result<(), TransportError> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;
    let name = ServerName::try_from("mimouse").expect("valid server name");
    let mut stream = timeout(HANDSHAKE_TIMEOUT, connector.connect(name, stream)).await??;
    let fingerprint = fingerprint(stream.get_ref().1.peer_certificates())?;
    if !trust(addr, &fingerprint) {
        return Err(TransportError::Untrusted(fingerprint));
    }
    let paths = encoded.paths();
    let opened = spawn_blocking(move || wire::open(&paths, max_file_size))
        .await
        .map_err(io::Error::other)??;
    let len = encoded.len(&opened);
    let len32 = u32::try_from(len).map_err(|_| TransportError::TooLarge(len))?;
    timeout(transfer_timeout(len), async {
        stream.write_all(&len32.to_be_bytes()).await?;
        encoded.write(&mut stream, opened).await?;
        stream.flush().await?;
        match stream.read_u8().await {
            Ok(ACK) => Ok(()),
            _ => Err(TransportError::NoAck),
        }
    })
    .await?
}

/// Accept TLS connections on `port`: pairing requests (by ALPN) go to
/// `pair_tx`, clipboard updates to `tx` (received files, at most
/// `max_file_size` bytes in total, are stored first); without `tx`
/// (clipboard sharing off) only pairing is served.
pub(crate) fn listen(
    port: u16,
    tls: Rc<Tls>,
    max_file_size: u64,
    tx: Option<mpsc::UnboundedSender<Received>>,
    pair_tx: mpsc::UnboundedSender<pair::Incoming>,
) -> JoinHandle<()> {
    let inbound = tx.map(|tx| {
        let store = FileStore::new();
        if store.is_none() {
            log::warn!("clipboard: no cache directory, copied files cannot be received");
        }
        Rc::new(Inbound {
            tls: tls.clone(),
            max_file_size,
            store,
            tx,
            admission: Admission::default(),
        })
    });
    spawn_local(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => return log::warn!("clipboard: could not listen on tcp {addr}: {e}"),
        };
        match &inbound {
            Some(_) => log::info!("clipboard: listening on tcp {addr}"),
            None => log::info!("listening on tcp {addr} for pairing"),
        }
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        accept_loop(
            async || listener.accept().await,
            |stream, peer| {
                let accepted = Instant::now();
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    return log::debug!("tcp {peer}: {MAX_CONNECTIONS} connections open, closing");
                };
                let inbound = inbound.clone();
                let pair_tx = pair_tx.clone();
                spawn_local(async move {
                    let start = match client_hello(stream).await {
                        Ok(start) => start,
                        // also reachability probes (`cli discover`, `cli doctor`)
                        Err(e) => return log::debug!("tcp {peer}: {e}"),
                    };
                    if pair::is_pairing(&start) {
                        let _ = pair_tx.send((start, peer, permit));
                        return;
                    }
                    let Some(inbound) = inbound else {
                        return log::debug!("tcp {peer}: clipboard sharing is disabled");
                    };
                    if let Err(e) = receive(&inbound, start, peer, accepted).await {
                        log::warn!("clipboard: {peer}: {e}");
                    }
                });
            },
        )
        .await;
    })
}

/// what serving clipboard updates needs
struct Inbound {
    tls: Rc<Tls>,
    max_file_size: u64,
    store: Option<FileStore>,
    tx: mpsc::UnboundedSender<Received>,
    admission: Admission,
}

/// limits the inbound updates read at once, overall and per peer; updates
/// over a limit wait for a slot (their connections count against
/// `MAX_CONNECTIONS`)
struct Admission {
    overall: Semaphore,
    /// by certificate fingerprint; removed once unused
    peers: RefCell<HashMap<String, Arc<Semaphore>>>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            overall: Semaphore::new(MAX_TRANSFERS),
            peers: Default::default(),
        }
    }
}

/// a slot for reading one update
struct Admitted<'a> {
    admission: &'a Admission,
    fingerprint: &'a str,
    peer: Option<OwnedSemaphorePermit>,
    _overall: SemaphorePermit<'a>,
}

impl Admission {
    /// waits for a slot to read an update from the peer with `fingerprint`
    async fn admit<'a>(&'a self, fingerprint: &'a str) -> Admitted<'a> {
        let peer = self
            .peers
            .borrow_mut()
            .entry(fingerprint.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_PEER_TRANSFERS)))
            .clone();
        // neither semaphore is ever closed
        let peer = peer.acquire_owned().await.expect("open");
        let overall = self.overall.acquire().await.expect("open");
        Admitted {
            admission: self,
            fingerprint,
            peer: Some(peer),
            _overall: overall,
        }
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        drop(self.peer.take());
        let mut peers = self.admission.peers.borrow_mut();
        // nobody else holds or waits for this peer's slot
        if peers
            .get(self.fingerprint)
            .is_some_and(|s| Arc::strong_count(s) == 1)
        {
            peers.remove(self.fingerprint);
        }
    }
}

/// accepts connections with `accept` and hands each to `serve`
async fn accept_loop<S>(
    mut accept: impl AsyncFnMut() -> io::Result<(S, SocketAddr)>,
    mut serve: impl FnMut(S, SocketAddr),
) {
    let mut failures = 0;
    loop {
        match accept().await {
            Ok((stream, peer)) => {
                failures = 0;
                serve(stream, peer);
            }
            Err(e) => {
                log::warn!("clipboard: accept failed: {e}");
                tokio::time::sleep(accept_backoff(failures)).await;
                failures = failures.saturating_add(1);
            }
        }
    }
}

/// delay after `failures` consecutive accept errors; at the open file limit
/// accept fails instantly, and retrying at once would spin (as in `ipc`)
fn accept_backoff(failures: u32) -> Duration {
    Duration::from_millis(10)
        .saturating_mul(1 << failures.min(7))
        .min(Duration::from_secs(1))
}

async fn client_hello(stream: TcpStream) -> Result<StartHandshake<TcpStream>, TransportError> {
    stream.set_nodelay(true)?;
    let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
    Ok(timeout(HANDSHAKE_TIMEOUT, acceptor).await??)
}

/// receives a clipboard update and hands it to the clipboard task before
/// acknowledging it; the connection is dropped once the peer's fingerprint
/// is no longer authorized, or without an ack once the task's receiver is
/// closed (the backend ended)
async fn receive(
    inbound: &Inbound,
    start: StartHandshake<TcpStream>,
    peer: SocketAddr,
    accepted: Instant,
) -> Result<(), TransportError> {
    let Inbound {
        tls,
        max_file_size,
        store,
        tx,
        admission,
    } = inbound;
    if tx.is_closed() {
        return Err(TransportError::BackendEnded);
    }
    let config = tls.acceptor.config().clone();
    let mut stream = timeout(HANDSHAKE_TIMEOUT, start.into_stream(config)).await??;
    let from = fingerprint(stream.get_ref().1.peer_certificates())?;
    let policy = Policy::Authorized(tls.authorized_keys.clone());
    let transfer = async {
        let _admitted = admission.admit(&from).await;
        let mut files = store.as_ref().map(FileStore::receive);
        let mut data = read_update(&mut stream, *max_file_size, &mut files).await?;
        if let Some(files) = files {
            let files = files.finish().await?;
            if !files.is_empty() {
                data.set_files(files);
            }
        }
        Ok::<_, TransportError>(data)
    };
    let data = tokio::select! {
        data = transfer => data?,
        () = revoked(&policy, &from) => return Err(TransportError::Revoked(from)),
    };
    // revoked since the last check
    if policy.check(&from).is_err() {
        return Err(TransportError::Revoked(from));
    }
    let description = format!(
        "{} bytes: {}",
        data.total_size(),
        data.mime_types().collect::<Vec<_>>().join(", ")
    );
    let sender = tls.describe(&from);
    tx.send((Arc::new(data), peer.ip(), from, accepted))
        .map_err(|_| TransportError::BackendEnded)?;
    log::info!("clipboard received from {sender} ({description})");
    stream.write_u8(ACK).await?;
    stream.shutdown().await?;
    Ok(())
}

/// received files go to the store; without one, updates with files are
/// refused
impl wire::FileSink for Option<Receiving> {
    async fn create(&mut self, name: &str) -> Result<(), TransportError> {
        let files = self.as_mut().ok_or(TransportError::NoFileStore)?;
        Ok(files.create(name).await?)
    }

    async fn append(&mut self, chunk: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        let files = self.as_mut().expect("a file was created");
        Ok(files.append(chunk).await?)
    }
}

/// resolves once `policy` no longer trusts `fingerprint`
async fn revoked(policy: &Policy, fingerprint: &str) {
    let mut check = tokio::time::interval(REVOCATION_CHECK);
    while policy.check(fingerprint).is_ok() {
        check.tick().await;
    }
}

/// read a frame (`len: u32 BE | payload`) and its update; files go to
/// `files` as they arrive
async fn read_update(
    stream: &mut (impl AsyncRead + Unpin),
    max_file_size: u64,
    files: &mut impl wire::FileSink,
) -> Result<ClipboardData, TransportError> {
    let len = timeout(TRANSFER_TIMEOUT, stream.read_u32()).await?? as u64;
    if len > wire::max_frame_len(max_file_size) {
        return Err(TransportError::TooLarge(len));
    }
    let update = wire::read(stream, len, max_file_size, files);
    timeout(transfer_timeout(len), update).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::task::LocalSet;

    /// a clipboard listener on a free loopback port trusting `clients`, its
    /// authorized keys and the receiver of its updates
    async fn server(
        clients: &[&Certificate],
    ) -> (
        SocketAddr,
        Arc<RwLock<HashMap<String, String>>>,
        mpsc::UnboundedReceiver<Received>,
        JoinHandle<()>,
    ) {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let keys = clients
            .iter()
            .map(|c| (crypto::certificate_fingerprint(c), "test".into()))
            .collect();
        let keys = Arc::new(RwLock::new(keys));
        let cert = certificate();
        let tls = Rc::new(Tls::new(&cert, keys.clone()).unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        let (pair_tx, _) = mpsc::unbounded_channel();
        let task = listen(port, tls, 0, Some(tx), pair_tx);
        // the listener binds on its first poll
        tokio::task::yield_now().await;
        (
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            keys,
            rx,
            task,
        )
    }

    fn certificate() -> Certificate {
        Certificate::generate_self_signed(["ignored".to_owned()]).unwrap()
    }

    type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;

    /// a TLS connection to `addr` presenting `cert`
    async fn connect(addr: SocketAddr, cert: &Certificate) -> ClientStream {
        let connector = TlsConnector::from(Arc::new(
            crypto::peers::tls_client_config(cert, Policy::Any).unwrap(),
        ));
        let stream = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("mimouse").unwrap();
        connector.connect(name, stream).await.unwrap()
    }

    /// a transfer from `cert` that sent the start of an update and stalls
    async fn stalled(addr: SocketAddr, cert: &Certificate) -> ClientStream {
        let mut stream = connect(addr, cert).await;
        stream.write_all(&100u32.to_be_bytes()).await.unwrap();
        stream.write_all(b"LMCB\x01\x01").await.unwrap();
        stream.flush().await.unwrap();
        stream
    }

    /// whether the server closes `stream` within `within`
    async fn closed(stream: &mut (impl AsyncRead + Unpin), within: Duration) -> bool {
        let mut buf = [0; 64];
        loop {
            match timeout(within, stream.read(&mut buf)).await {
                Ok(Ok(0) | Err(_)) => return true,
                Ok(Ok(_)) => continue,
                Err(_) => return false,
            }
        }
    }

    /// B2: removing the key of a connected peer closes its connection,
    /// also while it sends nothing
    #[tokio::test]
    async fn revoked_key_closes_open_connection() {
        LocalSet::new()
            .run_until(async {
                let cert = certificate();
                let (addr, keys, _rx, server) = server(&[&cert]).await;
                let mut stream = connect(addr, &cert).await;
                // the handshake completed and the server waits for a frame
                assert!(!closed(&mut stream, Duration::from_millis(200)).await);

                keys.write().unwrap().clear();
                assert!(
                    closed(&mut stream, Duration::from_secs(2)).await,
                    "revoked connection kept open"
                );
                server.abort();
            })
            .await;
    }

    /// B24: updates are acknowledged while the clipboard backend runs (also
    /// while its permission dialog is pending) and refused once it ended
    #[tokio::test]
    async fn updates_are_refused_once_the_backend_ended() {
        LocalSet::new()
            .run_until(async {
                let cert = certificate();
                let (addr, _keys, mut rx, server) = server(&[&cert]).await;
                let tls = Tls::new(&cert, Default::default()).unwrap();
                let data = text("copied");
                let addrs = [addr];
                let push = || push(&tls, &addrs, &data);

                push().await.expect("acknowledged");
                let (received, ..) = rx.try_recv().expect("handed to the clipboard");
                assert_eq!(received, data);

                // the backend ended: the task drops the receiver
                drop(rx);
                assert!(
                    push().await.is_err(),
                    "acknowledged although the backend ended"
                );
                server.abort();
            })
            .await;
    }

    fn text(s: &str) -> Arc<ClipboardData> {
        let mut data = ClipboardData::default();
        data.push(crate::clipboard::MIME_TEXT, s.as_bytes().to_vec());
        Arc::new(data)
    }

    /// send `data` (without files) to the first of `addrs` that takes it
    async fn push(
        tls: &Tls,
        addrs: &[SocketAddr],
        data: &ClipboardData,
    ) -> Result<SocketAddr, TransportError> {
        let encoded = wire::encode(data, 0)?;
        send(tls, addrs, &encoded, 0, Policy::Any, |_, _| true).await
    }

    /// a reader of `bytes` that records the largest read it was asked for
    struct Recording<'a> {
        bytes: &'a [u8],
        largest: usize,
    }

    impl AsyncRead for Recording<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            self.largest = self.largest.max(buf.remaining());
            std::pin::Pin::new(&mut self.bytes).poll_read(cx, buf)
        }
    }

    /// received files, counted in bytes
    #[derive(Default)]
    struct Count(u64);

    impl wire::FileSink for Count {
        async fn create(&mut self, _: &str) -> Result<(), TransportError> {
            Ok(())
        }

        async fn append(&mut self, chunk: Vec<u8>) -> Result<Vec<u8>, TransportError> {
            self.0 += chunk.len() as u64;
            Ok(chunk)
        }
    }

    /// C7: a large copied file is read in pieces of at most `wire::CHUNK`
    /// bytes, not buffered whole
    #[tokio::test]
    async fn received_files_are_not_buffered_whole() {
        let tmp = super::super::files::tests::TempDir::new();
        let contents = vec![5; 8 * wire::CHUNK];
        let max = contents.len() as u64;
        let mut data = ClipboardData::default();
        data.set_files(vec![tmp.file("big", &contents)]);
        let encoded = wire::encode(&data, max).unwrap();
        let opened = wire::open(&encoded.paths(), max).unwrap();
        let mut frame = (encoded.len(&opened) as u32).to_be_bytes().to_vec();
        encoded.write(&mut frame, opened).await.unwrap();

        let mut stream = Recording {
            bytes: &frame,
            largest: 0,
        };
        let mut files = Count::default();
        read_update(&mut stream, max, &mut files).await.unwrap();
        assert_eq!(files.0, max);
        assert!(
            stream.largest <= wire::CHUNK,
            "{} bytes buffered",
            stream.largest
        );
    }

    /// C7: an update waits while the same peer's previous transfer is read,
    /// and is acknowledged once that ended
    #[tokio::test]
    async fn transfers_from_one_peer_are_read_one_at_a_time() {
        LocalSet::new()
            .run_until(async {
                let cert = certificate();
                let (addr, _keys, mut rx, server) = server(&[&cert]).await;
                let previous = stalled(addr, &cert).await;
                let tls = Tls::new(&cert, Default::default()).unwrap();
                let addrs = [addr];
                let data = text("next");
                let push = push(&tls, &addrs, &data);
                let mut push = std::pin::pin!(push);
                assert!(
                    timeout(Duration::from_millis(300), &mut push)
                        .await
                        .is_err(),
                    "read while the previous transfer of its peer was"
                );
                drop(previous);
                push.await
                    .expect("acknowledged once the previous transfer ended");
                assert_eq!(rx.try_recv().expect("received").0, data);
                server.abort();
            })
            .await;
    }

    /// C7: at most `MAX_TRANSFERS` updates are read at once; another peer's
    /// update waits for a free slot instead of being refused
    #[tokio::test]
    async fn transfers_over_the_cap_wait() {
        LocalSet::new()
            .run_until(async {
                let certs: Vec<_> = (0..=MAX_TRANSFERS).map(|_| certificate()).collect();
                let (addr, _keys, mut rx, server) = server(&certs.iter().collect::<Vec<_>>()).await;
                let (last, busy) = certs.split_last().unwrap();
                let mut stalled_transfers = vec![];
                for cert in busy {
                    stalled_transfers.push(stalled(addr, cert).await);
                }
                let tls = Tls::new(last, Default::default()).unwrap();
                let addrs = [addr];
                let data = text("waiting");
                let push = push(&tls, &addrs, &data);
                let mut push = std::pin::pin!(push);
                assert!(
                    timeout(Duration::from_millis(300), &mut push)
                        .await
                        .is_err(),
                    "read while {MAX_TRANSFERS} transfers were"
                );
                stalled_transfers.pop();
                push.await.expect("acknowledged once a slot was free");
                assert_eq!(rx.try_recv().expect("received").0, data);
                server.abort();
            })
            .await;
    }

    /// B3: connections over the cap are closed at once, the others stay
    #[tokio::test]
    async fn connections_over_the_cap_are_closed() {
        LocalSet::new()
            .run_until(async {
                let cert = certificate();
                let (addr, _keys, _rx, server) = server(&[&cert]).await;
                let mut open = vec![];
                for _ in 0..MAX_CONNECTIONS {
                    open.push(TcpStream::connect(addr).await.unwrap());
                }
                let mut excess = TcpStream::connect(addr).await.unwrap();
                assert!(
                    closed(&mut excess, Duration::from_secs(1)).await,
                    "connection over the cap kept open"
                );
                for stream in &mut open {
                    assert!(!closed(stream, Duration::from_millis(10)).await);
                }
                server.abort();
            })
            .await;
    }

    /// B3: failing accepts (e.g. EMFILE) are retried with a backoff, not in
    /// a busy loop
    #[tokio::test]
    async fn accept_errors_back_off() {
        let attempts = Cell::new(0);
        let failing = async || {
            attempts.set(attempts.get() + 1);
            tokio::task::yield_now().await;
            Err::<((), SocketAddr), _>(io::Error::other("too many open files"))
        };
        let run = accept_loop(failing, |_, _| {});
        let _ = timeout(Duration::from_millis(200), run).await;
        // 10, 20, 40, 80 ms: at most 5 attempts in 200 ms
        assert!(attempts.get() <= 5, "{} attempts", attempts.get());
    }
}

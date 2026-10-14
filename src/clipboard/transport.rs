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
    sync::{Semaphore, mpsc},
    task::{JoinHandle, spawn_blocking, spawn_local},
    time::timeout,
};
use tokio_rustls::{LazyConfigAcceptor, StartHandshake, TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;

use super::{ClipboardData, files::FileStore, wire};
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

/// send `data` to the first reachable address whose server `policy` and
/// `trust` (given the address and the server's fingerprint) accept; copied
/// files are read when sending and may total at most `max_file_size` bytes
pub(crate) async fn send(
    tls: &Tls,
    addrs: &[SocketAddr],
    data: Arc<ClipboardData>,
    max_file_size: u64,
    policy: Policy,
    trust: impl Fn(SocketAddr, &str) -> bool,
) -> Result<SocketAddr, TransportError> {
    let payload = spawn_blocking(move || wire::encode(&data, max_file_size))
        .await
        .map_err(io::Error::other)??;
    if u32::try_from(payload.len()).is_err() {
        return Err(TransportError::TooLarge(payload.len() as u64));
    }
    let connector = TlsConnector::from(Arc::new(crypto::peers::tls_client_config(
        &tls.cert, policy,
    )?));
    let mut last_err = TransportError::Unreachable;
    for &addr in addrs {
        match send_to(&connector, addr, &payload, &trust).await {
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
    payload: &[u8],
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
    timeout(transfer_timeout(payload.len() as u64), async {
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await?;
        stream.write_all(payload).await?;
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
    let store = tx.as_ref().and_then(|_| FileStore::new());
    if tx.is_some() && store.is_none() {
        log::warn!("clipboard: no cache directory, copied files cannot be received");
    }
    spawn_local(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => return log::warn!("clipboard: could not listen on tcp {addr}: {e}"),
        };
        match tx {
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
                let tls = tls.clone();
                let tx = tx.clone();
                let pair_tx = pair_tx.clone();
                let store = store.clone();
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
                    let Some(tx) = tx else {
                        return log::debug!("tcp {peer}: clipboard sharing is disabled");
                    };
                    let received = receive(&tls, start, max_file_size, store, &tx, peer, accepted);
                    if let Err(e) = received.await {
                        log::warn!("clipboard: {peer}: {e}");
                    }
                });
            },
        )
        .await;
    })
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

/// receives a clipboard update and hands it to `tx` before acknowledging it;
/// the connection is dropped once the peer's fingerprint is no longer
/// authorized, or without an ack if `tx` is closed (the backend ended)
async fn receive(
    tls: &Tls,
    start: StartHandshake<TcpStream>,
    max_file_size: u64,
    store: Option<FileStore>,
    tx: &mpsc::UnboundedSender<Received>,
    peer: SocketAddr,
    accepted: Instant,
) -> Result<(), TransportError> {
    if tx.is_closed() {
        return Err(TransportError::BackendEnded);
    }
    let config = tls.acceptor.config().clone();
    let mut stream = timeout(HANDSHAKE_TIMEOUT, start.into_stream(config)).await??;
    let from = fingerprint(stream.get_ref().1.peer_certificates())?;
    let policy = Policy::Authorized(tls.authorized_keys.clone());
    let transfer = async {
        let payload = read_frame(&mut stream, wire::max_frame_len(max_file_size)).await?;
        spawn_blocking(move || {
            let frame = wire::decode(&payload, max_file_size).ok_or(TransportError::Invalid)?;
            let mut data = frame.data;
            if !frame.files.is_empty() {
                let store = store.ok_or(TransportError::NoFileStore)?;
                data.set_files(store.store(&frame.files)?);
            }
            Ok::<_, TransportError>(data)
        })
        .await
        .map_err(io::Error::other)?
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

/// resolves once `policy` no longer trusts `fingerprint`
async fn revoked(policy: &Policy, fingerprint: &str) {
    let mut check = tokio::time::interval(REVOCATION_CHECK);
    while policy.check(fingerprint).is_ok() {
        check.tick().await;
    }
}

async fn read_frame(
    stream: &mut (impl AsyncRead + Unpin),
    max_len: u64,
) -> Result<Vec<u8>, TransportError> {
    let len = timeout(TRANSFER_TIMEOUT, stream.read_u32()).await?? as u64;
    if len > max_len {
        return Err(TransportError::TooLarge(len));
    }
    let mut payload = vec![0; len as usize];
    timeout(transfer_timeout(len), stream.read_exact(&mut payload)).await??;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::task::LocalSet;

    /// a clipboard listener on a free loopback port trusting `client`, its
    /// authorized keys and the receiver of its updates
    async fn server(
        client: &Certificate,
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
        let fingerprint = crypto::certificate_fingerprint(client);
        let keys = Arc::new(RwLock::new(HashMap::from([(fingerprint, "test".into())])));
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
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
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (addr, keys, _rx, server) = server(&cert).await;
                let connector = TlsConnector::from(Arc::new(
                    crypto::peers::tls_client_config(&cert, Policy::Any).unwrap(),
                ));
                let stream = TcpStream::connect(addr).await.unwrap();
                let name = ServerName::try_from("mimouse").unwrap();
                let mut stream = connector.connect(name, stream).await.unwrap();
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
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (addr, _keys, mut rx, server) = server(&cert).await;
                let tls = Tls::new(&cert, Default::default()).unwrap();
                let mut data = ClipboardData::default();
                data.push(crate::clipboard::MIME_TEXT, b"copied".to_vec());
                let data = Arc::new(data);
                let addrs = [addr];
                let push = || send(&tls, &addrs, data.clone(), 0, Policy::Any, |_, _| true);

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

    /// B3: connections over the cap are closed at once, the others stay
    #[tokio::test]
    async fn connections_over_the_cap_are_closed() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (addr, _keys, _rx, server) = server(&cert).await;
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

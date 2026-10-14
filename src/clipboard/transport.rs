//! TLS over TCP transport for clipboard updates.
//!
//! Uses the mimouse DTLS certificate on both ends. The server accepts only
//! certificates in `authorized_fingerprints`; the client accepts the server
//! its client is pinned to, trusting it on first use (`crypto::peers`).
//!
//! One connection carries one update: the client sends a frame
//! (`len: u32 BE | payload`, payload see `wire`), the server answers with a
//! single ack byte once the update (including copied files) is stored.
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
    time::Duration,
};

use rustls_pki_types::{CertificateDer, ServerName};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
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
}

impl From<tokio::time::error::Elapsed> for TransportError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

/// a clipboard update received from the peer at an address, with the
/// fingerprint of its certificate
pub(crate) type Received = (Arc<ClipboardData>, IpAddr, String);

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
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("clipboard: accept failed: {e}");
                    continue;
                }
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
                    let _ = pair_tx.send((start, peer));
                    return;
                }
                let Some(tx) = tx else {
                    return log::debug!("tcp {peer}: clipboard sharing is disabled");
                };
                match receive(&tls, start, max_file_size, store).await {
                    Ok((data, fingerprint)) => {
                        log::info!(
                            "clipboard received from {} ({} bytes: {})",
                            tls.describe(&fingerprint),
                            data.total_size(),
                            data.mime_types().collect::<Vec<_>>().join(", ")
                        );
                        let _ = tx.send((Arc::new(data), peer.ip(), fingerprint));
                    }
                    Err(e) => log::warn!("clipboard: {peer}: {e}"),
                }
            });
        }
    })
}

async fn client_hello(stream: TcpStream) -> Result<StartHandshake<TcpStream>, TransportError> {
    stream.set_nodelay(true)?;
    let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
    Ok(timeout(HANDSHAKE_TIMEOUT, acceptor).await??)
}

/// a clipboard update and the fingerprint of the peer that sent it
async fn receive(
    tls: &Tls,
    start: StartHandshake<TcpStream>,
    max_file_size: u64,
    store: Option<FileStore>,
) -> Result<(ClipboardData, String), TransportError> {
    let config = tls.acceptor.config().clone();
    let mut stream = timeout(HANDSHAKE_TIMEOUT, start.into_stream(config)).await??;
    let from = fingerprint(stream.get_ref().1.peer_certificates())?;
    let payload = read_frame(&mut stream, wire::max_frame_len(max_file_size)).await?;
    let data = spawn_blocking(move || {
        let frame = wire::decode(&payload, max_file_size).ok_or(TransportError::Invalid)?;
        let mut data = frame.data;
        if !frame.files.is_empty() {
            let store = store.ok_or(TransportError::NoFileStore)?;
            data.set_files(store.store(&frame.files)?);
        }
        Ok::<_, TransportError>(data)
    })
    .await
    .map_err(io::Error::other)??;
    stream.write_u8(ACK).await?;
    stream.shutdown().await?;
    Ok((data, from))
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

//! TLS over TCP transport for clipboard updates.
//!
//! Uses the mimouse DTLS certificate on both ends. A connection is accepted
//! only if the peer's certificate fingerprint is in `authorized_fingerprints`
//! (checked by both the client and the server).
//!
//! One connection carries one update: the client sends a frame
//! (`len: u32 BE | payload`), the server answers with a single ack byte.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};

use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, ring},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::{JoinHandle, spawn_local},
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;

use super::{ClipboardData, MAX_CLIPBOARD_SIZE, wire};
use crate::crypto;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
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
    TooLarge(usize),
    #[error("invalid clipboard update")]
    Invalid,
    #[error("peer did not acknowledge the update")]
    NoAck,
    #[error("no address reachable")]
    Unreachable,
}

impl From<tokio::time::error::Elapsed> for TransportError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

pub(crate) struct Tls {
    acceptor: TlsAcceptor,
    connector: TlsConnector,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
}

impl Tls {
    pub(crate) fn new(
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, TransportError> {
        // the PEM file uses a nonstandard `PRIVATE_KEY` tag, so take the
        // already parsed certificate; mimouse generates PKCS#8 keys
        let certs = cert.certificate.clone();
        let key: PrivateKeyDer =
            PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into();
        let provider = Arc::new(ring::default_provider());
        let verifier = Arc::new(PeerVerifier {
            authorized_keys: authorized_keys.clone(),
            algorithms: provider.signature_verification_algorithms,
        });
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier.clone())
            .with_single_cert(certs.clone(), key.clone_key())?;
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(certs, key)?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(server)),
            connector: TlsConnector::from(Arc::new(client)),
            authorized_keys,
        })
    }

    fn describe(&self, cert: Option<&CertificateDer>) -> String {
        let Some(cert) = cert else {
            return "unknown peer".into();
        };
        let fingerprint = crypto::generate_fingerprint(cert);
        let keys = self.authorized_keys.read().expect("lock");
        keys.get(&fingerprint).cloned().unwrap_or(fingerprint)
    }
}

/// accepts peers whose certificate fingerprint is authorized;
/// mimouse certificates are self-signed, so there is no chain to verify
#[derive(Debug)]
struct PeerVerifier {
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PeerVerifier {
    fn check(&self, cert: &CertificateDer) -> Result<(), rustls::Error> {
        let fingerprint = crypto::generate_fingerprint(cert);
        if self
            .authorized_keys
            .read()
            .expect("lock")
            .contains_key(&fingerprint)
        {
            Ok(())
        } else {
            Err(rustls::Error::General(format!(
                "unauthorized certificate fingerprint {fingerprint}"
            )))
        }
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// send `data` to the first reachable address
pub(crate) async fn send(
    tls: &Tls,
    addrs: &[SocketAddr],
    data: &ClipboardData,
) -> Result<SocketAddr, TransportError> {
    let payload = wire::encode(data);
    if payload.len() > MAX_CLIPBOARD_SIZE {
        return Err(TransportError::TooLarge(payload.len()));
    }
    let mut last_err = TransportError::Unreachable;
    for &addr in addrs {
        match send_to(tls, addr, &payload).await {
            Ok(()) => return Ok(addr),
            Err(e) => {
                log::debug!("clipboard: {addr}: {e}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

async fn send_to(tls: &Tls, addr: SocketAddr, payload: &[u8]) -> Result<(), TransportError> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;
    let name = ServerName::try_from("mimouse").expect("valid server name");
    let mut stream = timeout(HANDSHAKE_TIMEOUT, tls.connector.connect(name, stream)).await??;
    timeout(TRANSFER_TIMEOUT, async {
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

/// accept clipboard updates on `port`; received updates are forwarded to `tx`
pub(crate) fn listen(
    port: u16,
    tls: Rc<Tls>,
    tx: mpsc::UnboundedSender<(Arc<ClipboardData>, IpAddr)>,
) -> JoinHandle<()> {
    spawn_local(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => return log::warn!("clipboard: could not listen on tcp {addr}: {e}"),
        };
        log::info!("clipboard: listening on tcp {addr}");
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
            spawn_local(async move {
                match receive(&tls, stream).await {
                    Ok((data, from)) => {
                        log::info!(
                            "clipboard received from {from} ({} bytes: {})",
                            data.size(),
                            data.mime_types().collect::<Vec<_>>().join(", ")
                        );
                        let _ = tx.send((Arc::new(data), peer.ip()));
                    }
                    Err(e) => log::warn!("clipboard: {peer}: {e}"),
                }
            });
        }
    })
}

async fn receive(tls: &Tls, stream: TcpStream) -> Result<(ClipboardData, String), TransportError> {
    stream.set_nodelay(true)?;
    let mut stream = timeout(HANDSHAKE_TIMEOUT, tls.acceptor.accept(stream)).await??;
    let from = tls.describe(
        stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first()),
    );
    let payload = timeout(TRANSFER_TIMEOUT, read_frame(&mut stream)).await??;
    let data = wire::decode(&payload).ok_or(TransportError::Invalid)?;
    stream.write_u8(ACK).await?;
    stream.shutdown().await?;
    Ok((data, from))
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, TransportError> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_CLIPBOARD_SIZE {
        return Err(TransportError::TooLarge(len));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

//! Pairing: two machines learn and trust each other's certificate after their
//! users compare a 6-digit code.
//!
//! Runs over TLS on the MiMouse TCP port, told apart from clipboard
//! connections by its ALPN protocol. Both sides present their certificate but
//! neither verifies it against `authorized_fingerprints`; a code derived from
//! both fingerprints is what the users compare. The initiator commits to its
//! nonce before seeing the acceptor's, so a machine in the middle cannot pick
//! certificates or nonces that make the codes on both ends agree (the
//! numeric comparison scheme of Bluetooth Secure Simple Pairing).
//!
//! Frames are `len: u32 BE | JSON`:
//! initiator `hello` -> acceptor `welcome` -> initiator `reveal`, both show
//! the code, then each side sends `confirm` once its user confirmed, or
//! `cancel`. A side trusts the other only after both confirmed.

use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use rustls::{ClientConfig, ServerConfig, crypto::ring};
use rustls_pki_types::{CertificateDer, ServerName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, split},
    net::TcpStream,
    sync::mpsc,
    task::{JoinHandle, spawn_local},
    time::{Instant, sleep_until, timeout},
};
use tokio_rustls::{StartHandshake, TlsConnector};
use webrtc_dtls::crypto::Certificate;

use crate::{
    crypto::{self, peers::Policy},
    ipc::Position,
};

/// ALPN protocol of pairing connections
pub(crate) const ALPN: &[u8] = b"mimouse-pair/1";
/// longest `pair --accept` window
pub(crate) const MAX_WINDOW: Duration = Duration::from_secs(3600);
const MAX_FRAME: usize = 4096;
const MAX_NAME: usize = 253;
const MAX_REASON: usize = 512;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// time each user has to compare the codes
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// a pairing connection whose TLS handshake has not completed yet, and the
/// address it came from; sent by the TCP listener
pub(crate) type Incoming = (StartHandshake<TcpStream>, SocketAddr);

/// whether a TLS client asks for a pairing connection
pub(crate) fn is_pairing(start: &StartHandshake<TcpStream>) -> bool {
    start
        .client_hello()
        .alpn()
        .is_some_and(|mut protocols| protocols.any(|p| p == ALPN))
}

#[derive(Debug, Error)]
pub(crate) enum PairError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    #[error("timed out")]
    Timeout,
    #[error("invalid pairing message: {0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Refused(String),
    #[error("the other machine did not send the nonce it committed to")]
    CommitmentMismatch,
    #[error("cannot pair this machine with itself")]
    SelfPair,
    #[error("cancelled")]
    Cancelled,
    #[error("no randomness available")]
    Random,
    #[error("no answer from {0}")]
    Unreachable(String),
    #[error("{0} does not accept pairing; run `mimouse cli pair --accept` there")]
    NotPairing(SocketAddr),
}

impl From<tokio::time::error::Elapsed> for PairError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    /// initiator: its name, its MiMouse port, where the acceptor is relative
    /// to it and the commitment `SHA-256(nonce)`
    Hello {
        name: String,
        port: u16,
        position: Position,
        #[serde(with = "hex32")]
        commitment: [u8; 32],
    },
    /// acceptor: its name and nonce
    Welcome {
        name: String,
        #[serde(with = "hex32")]
        nonce: [u8; 32],
    },
    /// initiator: the nonce it committed to
    Reveal {
        #[serde(with = "hex32")]
        nonce: [u8; 32],
    },
    /// the user on the sending side confirmed the code
    Confirm,
    /// the sending side gives up
    Cancel { reason: String },
}

/// 32 bytes as 64 lowercase hex digits
mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let mut bytes = [0; 32];
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(D::Error::custom("expected 64 hex digits"));
        }
        for (byte, pair) in bytes.iter_mut().zip(s.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).expect("ascii");
            *byte = u8::from_str_radix(pair, 16).map_err(D::Error::custom)?;
        }
        Ok(bytes)
    }
}

fn valid_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// parse and check one frame
fn decode(payload: &[u8]) -> Result<Frame, PairError> {
    let frame: Frame = serde_json::from_slice(payload).map_err(|_| PairError::Invalid("syntax"))?;
    match &frame {
        Frame::Hello { name, port, .. } => {
            if name.is_empty() || !valid_text(name, MAX_NAME) {
                return Err(PairError::Invalid("name"));
            }
            if *port == 0 {
                return Err(PairError::Invalid("port"));
            }
        }
        Frame::Welcome { name, .. } if name.is_empty() || !valid_text(name, MAX_NAME) => {
            return Err(PairError::Invalid("name"));
        }
        Frame::Cancel { reason } if !valid_text(reason, MAX_REASON) => {
            return Err(PairError::Invalid("reason"));
        }
        _ => {}
    }
    Ok(frame)
}

fn encode(frame: &Frame) -> Vec<u8> {
    let payload = serde_json::to_vec(frame).expect("serializable frame");
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    buf
}

async fn write_frame(w: &mut (impl AsyncWrite + Unpin), frame: &Frame) -> Result<(), PairError> {
    timeout(FRAME_TIMEOUT, async {
        w.write_all(&encode(frame)).await?;
        w.flush().await
    })
    .await??;
    Ok(())
}

/// the next frame; not cancel safe
async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Result<Frame, PairError> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(PairError::Invalid("too long"));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).await?;
    decode(&payload)
}

async fn expect_frame(r: &mut (impl AsyncRead + Unpin)) -> Result<Frame, PairError> {
    match timeout(FRAME_TIMEOUT, read_frame(r)).await?? {
        Frame::Cancel { reason } => Err(PairError::Refused(reason)),
        frame => Ok(frame),
    }
}

fn commitment(nonce: &[u8; 32]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"mimouse-pair commitment")
        .chain_update(nonce)
        .finalize()
        .into()
}

/// The code both users compare: independent of which fingerprint is whose,
/// different when either fingerprint or nonce differs.
pub(crate) fn code(
    fingerprint_a: &str,
    fingerprint_b: &str,
    initiator_nonce: &[u8; 32],
    acceptor_nonce: &[u8; 32],
) -> String {
    let (lo, hi) = if fingerprint_a <= fingerprint_b {
        (fingerprint_a, fingerprint_b)
    } else {
        (fingerprint_b, fingerprint_a)
    };
    let digest = Sha256::new()
        .chain_update(b"mimouse-pair code\0")
        .chain_update(lo)
        .chain_update(b"\0")
        .chain_update(hi)
        .chain_update(b"\0")
        .chain_update(initiator_nonce)
        .chain_update(acceptor_nonce)
        .finalize();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes")) % 1_000_000;
    format!("{n:06}")
}

/// `name` if it can be used as a host name to resolve
fn dns_name(name: &str) -> Option<String> {
    let valid_label = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let name = name.trim_end_matches('.');
    (name.len() <= MAX_NAME && name.split('.').all(valid_label) && name.parse::<IpAddr>().is_err())
        .then(|| name.to_owned())
}

/// window during which one incoming pairing request is accepted
#[derive(Debug, Default)]
struct AcceptWindow {
    until: Option<Instant>,
}

impl AcceptWindow {
    fn open(&mut self, now: Instant, duration: Duration) {
        self.until = Some(now + duration);
    }

    fn close(&mut self) -> bool {
        self.until.take().is_some()
    }

    fn deadline(&self) -> Option<Instant> {
        self.until
    }

    /// whether a request arriving at `now` is accepted; the first accepted
    /// request (or the first one after expiry) closes the window
    fn take(&mut self, now: Instant) -> bool {
        self.until.take().is_some_and(|until| now < until)
    }
}

/// a pairing completed: the peer is to be trusted and configured
#[derive(Debug)]
pub(crate) struct Trusted {
    /// name the peer reported
    pub(crate) name: String,
    pub(crate) fingerprint: String,
    pub(crate) hostname: Option<String>,
    pub(crate) ips: Vec<IpAddr>,
    pub(crate) port: u16,
    /// where the peer is relative to this machine
    pub(crate) position: Position,
}

/// what the service has to act on
#[derive(Debug)]
pub(crate) enum PairEvent {
    /// show the code; the user confirms or cancels
    Code {
        code: String,
        peer: String,
        fingerprint: String,
        position: Position,
    },
    Trusted(Trusted),
    Failed(String),
}

/// where to pair with (`mimouse cli pair <host>`)
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) host: String,
    pub(crate) ips: Vec<IpAddr>,
    pub(crate) port: u16,
    /// where the target is relative to this machine
    pub(crate) position: Position,
}

enum SessionEvent {
    Code {
        code: String,
        peer: String,
        fingerprint: String,
        position: Position,
    },
    Done(Result<Trusted, PairError>),
}

struct Session {
    id: u64,
    /// the code shown to the user, once known
    code: Option<String>,
    decisions: mpsc::UnboundedSender<bool>,
    /// the user cancelled; the session must not end in trust
    cancelled: bool,
    task: JoinHandle<()>,
}

/// what a session task needs
struct Context {
    id: u64,
    fingerprint: String,
    events: mpsc::UnboundedSender<(Option<u64>, SessionEvent)>,
    decisions: mpsc::UnboundedReceiver<bool>,
}

pub(crate) struct Pairing {
    client_config: Arc<ClientConfig>,
    server_config: Arc<ServerConfig>,
    fingerprint: String,
    window: AcceptWindow,
    session: Option<Session>,
    next_id: u64,
    /// session events; `None` for events not tied to a running session
    events_tx: mpsc::UnboundedSender<(Option<u64>, SessionEvent)>,
    events_rx: mpsc::UnboundedReceiver<(Option<u64>, SessionEvent)>,
    incoming_rx: mpsc::UnboundedReceiver<Incoming>,
}

impl Pairing {
    /// pairing with `cert`; connections arrive through the returned sender.
    /// Both sides accept any certificate (handshake signatures are still
    /// checked): the users decide trust by comparing codes
    pub(crate) fn new(
        cert: &Certificate,
    ) -> Result<(Self, mpsc::UnboundedSender<Incoming>), rustls::Error> {
        let mut server = crypto::peers::tls_server_config(cert, Policy::Any)?;
        server.alpn_protocols = vec![ALPN.to_vec()];
        let mut client = crypto::peers::tls_client_config(cert, Policy::Any)?;
        client.alpn_protocols = vec![ALPN.to_vec()];
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let pairing = Self {
            client_config: Arc::new(client),
            server_config: Arc::new(server),
            fingerprint: crypto::certificate_fingerprint(cert),
            window: Default::default(),
            session: None,
            next_id: 0,
            events_tx,
            events_rx,
            incoming_rx,
        };
        Ok((pairing, incoming_tx))
    }

    fn idle(&self) -> Result<(), String> {
        if self.session.is_some() {
            return Err(
                "a pairing is already in progress (`mimouse cli pair --cancel` aborts it)".into(),
            );
        }
        if self.window.deadline().is_some() {
            return Err(
                "waiting for a pairing request (`mimouse cli pair --cancel` stops waiting)".into(),
            );
        }
        Ok(())
    }

    /// pair with `target`; `port` is this machine's MiMouse port
    pub(crate) fn start(&mut self, target: Target, port: u16) -> Result<(), String> {
        self.idle()?;
        let config = self.client_config.clone();
        self.spawn(move |ctx| initiate(ctx, config, target, port));
        Ok(())
    }

    /// accept one pairing request during the next `duration`
    pub(crate) fn accept(&mut self, duration: Duration) -> Result<(), String> {
        if duration.is_zero() || duration > MAX_WINDOW {
            return Err(format!(
                "the timeout must be between 1 and {} seconds",
                MAX_WINDOW.as_secs()
            ));
        }
        if self.session.is_some() {
            return self.idle();
        }
        self.window.open(Instant::now(), duration);
        log::info!("pairing: accepting a request for {} s", duration.as_secs());
        Ok(())
    }

    /// the user confirmed `code`
    pub(crate) fn confirm(&mut self, code: &str) -> Result<(), String> {
        match &self.session {
            Some(Session {
                code: Some(c),
                decisions,
                ..
            }) if c == code.trim() => {
                let _ = decisions.send(true);
                Ok(())
            }
            Some(Session { code: Some(_), .. }) => {
                Err(format!("{code} is not the code of the pairing in progress"))
            }
            _ => Err("no pairing is waiting for confirmation".into()),
        }
    }

    pub(crate) fn cancel(&mut self) -> Result<(), String> {
        if let Some(session) = &mut self.session {
            if session.code.is_some() {
                // tells the peer, then ends with `Cancelled`; if the session
                // already finished, `event` turns its result into a failure
                session.cancelled = true;
                let _ = session.decisions.send(false);
            } else {
                session.task.abort();
                self.session = None;
                self.fail(PairError::Cancelled);
            }
            return Ok(());
        }
        if self.window.close() {
            self.fail(PairError::Cancelled);
            return Ok(());
        }
        Err("no pairing in progress".into())
    }

    fn fail(&self, e: PairError) {
        let _ = self.events_tx.send((None, SessionEvent::Done(Err(e))));
    }

    fn spawn<F, Fut>(&mut self, run: F)
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = Result<Trusted, PairError>> + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let (decisions_tx, decisions_rx) = mpsc::unbounded_channel();
        let events = self.events_tx.clone();
        let session = run(Context {
            id,
            fingerprint: self.fingerprint.clone(),
            events: events.clone(),
            decisions: decisions_rx,
        });
        let task = spawn_local(async move {
            let result = session.await;
            let _ = events.send((Some(id), SessionEvent::Done(result)));
        });
        self.session = Some(Session {
            id,
            code: None,
            decisions: decisions_tx,
            cancelled: false,
            task,
        });
    }

    fn incoming(&mut self, start: StartHandshake<TcpStream>, addr: SocketAddr) {
        let config = self.server_config.clone();
        if self.session.is_none() && self.window.take(Instant::now()) {
            log::info!("pairing: request from {addr}");
            self.spawn(move |ctx| respond(ctx, config, start, addr));
            return;
        }
        let reason = if self.session.is_some() {
            "the other machine is busy with another pairing"
        } else {
            "the other machine is not accepting pairing requests; run `mimouse cli pair --accept` there"
        };
        log::info!("pairing: refused request from {addr}");
        spawn_local(async move {
            if let Err(e) = refuse(config, start, reason).await {
                log::debug!("pairing: {addr}: {e}");
            }
        });
    }

    /// the next event for the service
    pub(crate) async fn event(&mut self) -> PairEvent {
        loop {
            let deadline = self.window.deadline();
            tokio::select! {
                Some((start, addr)) = self.incoming_rx.recv() => self.incoming(start, addr),
                Some((id, event)) = self.events_rx.recv() => {
                    if id.is_some() && id != self.session.as_ref().map(|s| s.id) {
                        continue; // a session that was cancelled
                    }
                    match event {
                        SessionEvent::Code { code, peer, fingerprint, position } => {
                            if let Some(session) = &mut self.session {
                                session.code = Some(code.clone());
                            }
                            return PairEvent::Code { code, peer, fingerprint, position };
                        }
                        SessionEvent::Done(result) => {
                            let cancelled = id.is_some()
                                && self.session.take().is_some_and(|s| s.cancelled);
                            return match result {
                                Ok(_) if cancelled => PairEvent::Failed(PairError::Cancelled.to_string()),
                                Ok(trusted) => PairEvent::Trusted(trusted),
                                Err(e) => PairEvent::Failed(e.to_string()),
                            };
                        }
                    }
                }
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.window.close();
                    return PairEvent::Failed("no pairing request arrived in time".into());
                }
            }
        }
    }
}

fn random_nonce() -> Result<[u8; 32], PairError> {
    let mut nonce = [0; 32];
    ring::default_provider()
        .secure_random
        .fill(&mut nonce)
        .map_err(|_| PairError::Random)?;
    Ok(nonce)
}

fn peer_fingerprint(certs: Option<&[CertificateDer<'static>]>) -> Result<String, PairError> {
    let cert = certs
        .and_then(|c| c.first())
        .ok_or(PairError::Invalid("no certificate"))?;
    Ok(crypto::generate_fingerprint(cert))
}

/// this machine's name as the peer should know it
async fn local_name() -> String {
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(target_os = "linux")]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    #[cfg(not(any(windows, target_os = "linux")))]
    let name = crate::doctor::run_command("hostname", &["-s"], crate::doctor::COMMAND_TIMEOUT)
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    name.map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty() && valid_text(n, MAX_NAME))
        .unwrap_or_else(|| "unknown".into())
}

/// `mimouse cli pair <host>`: connect, exchange nonces, compare codes
async fn initiate(
    mut ctx: Context,
    config: Arc<ClientConfig>,
    target: Target,
    port: u16,
) -> Result<Trusted, PairError> {
    let mut addrs: Vec<SocketAddr> = target
        .ips
        .iter()
        .map(|ip| SocketAddr::new(*ip, target.port))
        .collect();
    match tokio::net::lookup_host((target.host.as_str(), target.port)).await {
        Ok(resolved) => addrs.extend(resolved),
        Err(e) if addrs.is_empty() => {
            return Err(PairError::Unreachable(format!("{}: {e}", target.host)));
        }
        Err(e) => log::debug!("pairing: {}: {e}", target.host),
    }
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));

    let (stream, addr) = connect(config, &addrs).await?;
    let fingerprint = peer_fingerprint(stream.get_ref().1.peer_certificates())?;
    if fingerprint == ctx.fingerprint {
        return Err(PairError::SelfPair);
    }
    log::info!("pairing: connected to {addr}");
    let (mut r, mut w) = split(stream);

    let nonce = random_nonce()?;
    let hello = Frame::Hello {
        name: local_name().await,
        port,
        position: target.position,
        commitment: commitment(&nonce),
    };
    write_frame(&mut w, &hello).await?;
    let (name, their_nonce) = match expect_frame(&mut r).await? {
        Frame::Welcome { name, nonce } => (name, nonce),
        _ => return Err(PairError::Invalid("expected welcome")),
    };
    write_frame(&mut w, &Frame::Reveal { nonce }).await?;

    let code = code(&ctx.fingerprint, &fingerprint, &nonce, &their_nonce);
    let shown = format!("{name} ({addr})");
    confirm(
        &mut ctx,
        &mut r,
        &mut w,
        code,
        shown,
        &fingerprint,
        target.position,
    )
    .await?;

    let mut ips = target.ips;
    if !ips.contains(&addr.ip()) {
        ips.push(addr.ip());
    }
    let hostname = target
        .host
        .parse::<IpAddr>()
        .is_err()
        .then_some(target.host);
    Ok(Trusted {
        name,
        fingerprint,
        hostname,
        ips,
        port: target.port,
        position: target.position,
    })
}

async fn connect(
    config: Arc<ClientConfig>,
    addrs: &[SocketAddr],
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, SocketAddr), PairError> {
    let connector = TlsConnector::from(config);
    let name = ServerName::try_from("mimouse").expect("valid server name");
    let mut errors = vec![];
    for &addr in addrs {
        let attempt = async {
            let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
            tcp.set_nodelay(true)?;
            let tls = timeout(FRAME_TIMEOUT, connector.connect(name.clone(), tcp)).await??;
            if tls.get_ref().1.alpn_protocol() != Some(ALPN) {
                return Err(PairError::NotPairing(addr));
            }
            Ok(tls)
        };
        match attempt.await {
            Ok(tls) => return Ok((tls, addr)),
            Err(e) => {
                log::debug!("pairing: {addr}: {e}");
                errors.push(format!("{addr} ({e})"));
            }
        }
    }
    Err(PairError::Unreachable(errors.join(", ")))
}

/// `mimouse cli pair --accept`: answer a pairing request
async fn respond(
    mut ctx: Context,
    config: Arc<ServerConfig>,
    start: StartHandshake<TcpStream>,
    addr: SocketAddr,
) -> Result<Trusted, PairError> {
    let stream = timeout(FRAME_TIMEOUT, start.into_stream(config)).await??;
    let fingerprint = peer_fingerprint(stream.get_ref().1.peer_certificates())?;
    if fingerprint == ctx.fingerprint {
        return Err(PairError::SelfPair);
    }
    let (mut r, mut w) = split(stream);

    let Frame::Hello {
        name,
        port,
        position,
        commitment: committed,
    } = expect_frame(&mut r).await?
    else {
        return Err(PairError::Invalid("expected hello"));
    };
    let nonce = random_nonce()?;
    let welcome = Frame::Welcome {
        name: local_name().await,
        nonce,
    };
    write_frame(&mut w, &welcome).await?;
    let Frame::Reveal { nonce: their_nonce } = expect_frame(&mut r).await? else {
        return Err(PairError::Invalid("expected reveal"));
    };
    if commitment(&their_nonce) != committed {
        return Err(PairError::CommitmentMismatch);
    }

    // the initiator placed us at `position`, so it is on the opposite side
    let position = position.opposite();
    let code = code(&ctx.fingerprint, &fingerprint, &their_nonce, &nonce);
    let shown = format!("{name} ({})", addr.ip());
    confirm(
        &mut ctx,
        &mut r,
        &mut w,
        code,
        shown,
        &fingerprint,
        position,
    )
    .await?;
    Ok(Trusted {
        hostname: dns_name(&name),
        name,
        fingerprint,
        ips: vec![addr.ip()],
        port,
        position,
    })
}

/// tell a requester we are not pairing now
async fn refuse(
    config: Arc<ServerConfig>,
    start: StartHandshake<TcpStream>,
    reason: &str,
) -> Result<(), PairError> {
    let mut stream = timeout(FRAME_TIMEOUT, start.into_stream(config)).await??;
    // read the hello first, so the requester is reading when we answer
    timeout(FRAME_TIMEOUT, read_frame(&mut stream)).await??;
    write_frame(
        &mut stream,
        &Frame::Cancel {
            reason: reason.into(),
        },
    )
    .await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// show the code, then wait until both users confirmed it
async fn confirm<R, W>(
    ctx: &mut Context,
    r: &mut R,
    w: &mut W,
    code: String,
    peer: String,
    fingerprint: &str,
    position: Position,
) -> Result<(), PairError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let _ = ctx.events.send((
        Some(ctx.id),
        SessionEvent::Code {
            code,
            peer,
            fingerprint: fingerprint.to_owned(),
            position,
        },
    ));
    let deadline = Instant::now() + CONFIRM_TIMEOUT;
    let peer_frame = read_frame(r);
    tokio::pin!(peer_frame);
    let (mut confirmed, mut peer_confirmed) = (false, false);
    while !(confirmed && peer_confirmed) {
        tokio::select! {
            frame = &mut peer_frame, if !peer_confirmed => match frame? {
                Frame::Confirm => peer_confirmed = true,
                Frame::Cancel { reason } => return Err(PairError::Refused(reason)),
                _ => return Err(PairError::Invalid("expected confirm")),
            },
            // read even after confirming: a later cancel wins until the end
            decision = ctx.decisions.recv() => match decision {
                Some(true) if confirmed => {}
                Some(true) => {
                    write_frame(w, &Frame::Confirm).await?;
                    confirmed = true;
                }
                _ => {
                    let reason = format!("the code was rejected on {}", local_name().await);
                    let _ = write_frame(w, &Frame::Cancel { reason }).await;
                    return Err(PairError::Cancelled);
                }
            },
            _ = sleep_until(deadline) => {
                let reason = format!("the code was not confirmed on both machines within {} s", CONFIRM_TIMEOUT.as_secs());
                let _ = write_frame(w, &Frame::Cancel { reason: reason.clone() }).await;
                return Err(PairError::Refused(reason));
            }
        }
    }
    let _ = w.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "aa:01";
    const FP_B: &str = "bb:02";
    const N1: [u8; 32] = [1; 32];
    const N2: [u8; 32] = [2; 32];

    #[test]
    fn code_is_six_digits_and_order_independent() {
        let c = code(FP_A, FP_B, &N1, &N2);
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(c, code(FP_B, FP_A, &N1, &N2));
    }

    #[test]
    fn code_changes_with_either_fingerprint_or_nonce() {
        let c = code(FP_A, FP_B, &N1, &N2);
        assert_ne!(c, code("aa:03", FP_B, &N1, &N2));
        assert_ne!(c, code(FP_A, "bb:03", &N1, &N2));
        assert_ne!(c, code(FP_A, FP_B, &[3; 32], &N2));
        assert_ne!(c, code(FP_A, FP_B, &N1, &[3; 32]));
        // the nonces have roles: swapping them is a different pairing
        assert_ne!(c, code(FP_A, FP_B, &N2, &N1));
    }

    #[test]
    fn window_accepts_one_request_before_expiry() {
        let now = Instant::now();
        let mut window = AcceptWindow::default();
        assert!(!window.take(now), "closed window");
        window.open(now, Duration::from_secs(120));
        assert!(window.take(now + Duration::from_secs(119)));
        assert!(
            !window.take(now + Duration::from_secs(119)),
            "one per window"
        );
        assert_eq!(window.deadline(), None);
    }

    #[test]
    fn window_expires() {
        let now = Instant::now();
        let mut window = AcceptWindow::default();
        window.open(now, Duration::from_secs(120));
        assert_eq!(window.deadline(), Some(now + Duration::from_secs(120)));
        assert!(!window.take(now + Duration::from_secs(120)));
        assert_eq!(window.deadline(), None, "an expired request closes it");
        window.open(now, Duration::from_secs(1));
        assert!(window.close());
        assert!(!window.take(now));
    }

    fn payload(frame: &Frame) -> Vec<u8> {
        encode(frame)[4..].to_vec()
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Hello {
                name: "megapc".into(),
                port: 4242,
                position: Position::Right,
                commitment: commitment(&N1),
            },
            Frame::Welcome {
                name: "gpumaster".into(),
                nonce: N2,
            },
            Frame::Reveal { nonce: N1 },
            Frame::Confirm,
            Frame::Cancel {
                reason: "no".into(),
            },
        ];
        for frame in frames {
            let encoded = encode(&frame);
            assert_eq!(
                u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize,
                encoded.len() - 4
            );
            assert_eq!(decode(&encoded[4..]).unwrap(), frame);
        }
    }

    #[test]
    fn invalid_frames_are_rejected() {
        let hello = |name: &str, port: u16| Frame::Hello {
            name: name.into(),
            port,
            position: Position::Left,
            commitment: N1,
        };
        assert!(decode(&payload(&hello("", 4242))).is_err());
        assert!(decode(&payload(&hello("evil\nname", 4242))).is_err());
        assert!(decode(&payload(&hello(&"a".repeat(300), 4242))).is_err());
        assert!(decode(&payload(&hello("ok", 0))).is_err());
        let cancel = Frame::Cancel {
            reason: "x".repeat(MAX_REASON + 1),
        };
        assert!(decode(&payload(&cancel)).is_err());
        let reveal = |nonce: String| format!(r#"{{"type":"reveal","nonce":"{nonce}"}}"#);
        for bad in [
            reveal("0102".into()),
            reveal("zz".repeat(32)),
            reveal("é".repeat(32)),
            reveal("+1".repeat(32)),
            r#"{"type":"welcome","name":"x"}"#.into(),
            r#"{"type":"launch"}"#.into(),
            r#"{"type":"hello","name":"x","port":1,"position":"middle","commitment":""}"#.into(),
            "not json".into(),
        ] {
            assert!(decode(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn oversized_frames_are_not_read() {
        let mut data = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        data.extend(vec![b' '; MAX_FRAME + 1]);
        assert!(matches!(
            read_frame(&mut data.as_slice()).await,
            Err(PairError::Invalid("too long"))
        ));
    }

    #[test]
    fn commitment_binds_the_nonce() {
        assert_eq!(commitment(&N1), commitment(&N1));
        assert_ne!(commitment(&N1), commitment(&N2));
    }

    #[test]
    fn only_resolvable_names_become_host_names() {
        assert_eq!(
            dns_name("DESKTOP-KSAR2I2").as_deref(),
            Some("DESKTOP-KSAR2I2")
        );
        assert_eq!(
            dns_name("megapc.tail.ts.net.").as_deref(),
            Some("megapc.tail.ts.net")
        );
        assert_eq!(dns_name("Chris's PC"), None);
        assert_eq!(dns_name("-x"), None);
        assert_eq!(dns_name("a..b"), None);
        assert_eq!(dns_name("192.168.1.2"), None);
    }

    /// a pairing endpoint listening on a loopback port, routing pairing
    /// connections like the clipboard listener does
    async fn endpoint() -> (Pairing, String, u16) {
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        let (pairing, incoming) = Pairing::new(&cert).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        spawn_local(async move {
            loop {
                let (stream, addr) = listener.accept().await.unwrap();
                let acceptor = tokio_rustls::LazyConfigAcceptor::new(
                    rustls::server::Acceptor::default(),
                    stream,
                );
                let start = acceptor.await.unwrap();
                assert!(is_pairing(&start));
                incoming.send((start, addr)).unwrap();
            }
        });
        (pairing, crypto::certificate_fingerprint(&cert), port)
    }

    async fn next(pairing: &mut Pairing) -> PairEvent {
        timeout(Duration::from_secs(20), pairing.event())
            .await
            .expect("pairing event")
    }

    fn target(port: u16) -> Target {
        Target {
            host: "127.0.0.1".into(),
            ips: vec![],
            port,
            position: Position::Right,
        }
    }

    /// both sides show the same code, then trust each other after both
    /// confirmed: the initiator places the acceptor where asked, the
    /// acceptor places the initiator on the opposite side
    #[tokio::test]
    async fn pairing_over_loopback() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut acceptor, fp_acceptor, port) = endpoint().await;
                let (mut initiator, fp_initiator, _) = endpoint().await;
                acceptor.accept(Duration::from_secs(120)).unwrap();
                initiator.start(target(port), 4343).unwrap();
                let (a, i) = tokio::join!(next(&mut acceptor), next(&mut initiator));
                let (
                    PairEvent::Code {
                        code: code_a,
                        fingerprint: seen_by_a,
                        position: pos_a,
                        ..
                    },
                    PairEvent::Code {
                        code: code_i,
                        fingerprint: seen_by_i,
                        position: pos_i,
                        ..
                    },
                ) = (a, i)
                else {
                    panic!("expected codes");
                };
                assert_eq!(code_a, code_i);
                assert_eq!(seen_by_a, fp_initiator);
                assert_eq!(seen_by_i, fp_acceptor);
                assert_eq!((pos_a, pos_i), (Position::Left, Position::Right));

                assert!(acceptor.confirm("000000x").is_err());
                acceptor.confirm(&code_a).unwrap();
                initiator.confirm(&code_i).unwrap();
                let (a, i) = tokio::join!(next(&mut acceptor), next(&mut initiator));
                let (PairEvent::Trusted(a), PairEvent::Trusted(i)) = (a, i) else {
                    panic!("expected both to trust");
                };
                assert_eq!(a.fingerprint, fp_initiator);
                assert_eq!(a.position, Position::Left);
                assert_eq!(a.port, 4343);
                assert_eq!(a.ips, vec![IpAddr::from([127, 0, 0, 1])]);
                assert_eq!(i.fingerprint, fp_acceptor);
                assert_eq!(i.position, Position::Right);
                assert_eq!(i.port, port);
                assert_eq!(i.hostname, None, "an address is not a host name");
                assert_eq!(i.ips, vec![IpAddr::from([127, 0, 0, 1])]);
                // the window admitted one request only
                assert!(acceptor.session.is_none() && acceptor.window.deadline().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn rejecting_the_code_fails_both_sides() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut acceptor, _, port) = endpoint().await;
                let (mut initiator, _, _) = endpoint().await;
                acceptor.accept(Duration::from_secs(120)).unwrap();
                initiator.start(target(port), 4343).unwrap();
                let (PairEvent::Code { code, .. }, PairEvent::Code { .. }) =
                    tokio::join!(next(&mut acceptor), next(&mut initiator))
                else {
                    panic!("expected codes");
                };
                initiator.confirm(&code).unwrap();
                acceptor.cancel().unwrap();
                let (a, i) = tokio::join!(next(&mut acceptor), next(&mut initiator));
                assert!(matches!(a, PairEvent::Failed(_)), "{a:?}");
                let PairEvent::Failed(reason) = i else {
                    panic!("initiator trusted after rejection: {i:?}");
                };
                assert!(reason.contains("rejected"), "{reason}");
            })
            .await;
    }

    /// `pair --cancel` after confirming locally still prevents trust
    #[tokio::test]
    async fn cancelling_after_confirming_prevents_trust() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut acceptor, _, port) = endpoint().await;
                let (mut initiator, _, _) = endpoint().await;
                acceptor.accept(Duration::from_secs(120)).unwrap();
                initiator.start(target(port), 4343).unwrap();
                let (PairEvent::Code { code, .. }, PairEvent::Code { .. }) =
                    tokio::join!(next(&mut acceptor), next(&mut initiator))
                else {
                    panic!("expected codes");
                };
                acceptor.confirm(&code).unwrap();
                // let the acceptor send its Confirm before cancelling
                tokio::time::sleep(Duration::from_millis(50)).await;
                acceptor.cancel().unwrap();
                initiator.confirm(&code).unwrap();
                let (a, _) = tokio::join!(next(&mut acceptor), next(&mut initiator));
                assert!(matches!(a, PairEvent::Failed(_)), "{a:?}");
            })
            .await;
    }

    /// a session that finished before the cancel was handled is not trusted
    #[tokio::test]
    async fn cancel_wins_over_a_finished_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut acceptor, _, port) = endpoint().await;
                let (mut initiator, _, _) = endpoint().await;
                acceptor.accept(Duration::from_secs(120)).unwrap();
                initiator.start(target(port), 4343).unwrap();
                let (PairEvent::Code { code, .. }, PairEvent::Code { .. }) =
                    tokio::join!(next(&mut acceptor), next(&mut initiator))
                else {
                    panic!("expected codes");
                };
                acceptor.confirm(&code).unwrap();
                initiator.confirm(&code).unwrap();
                // both sessions end without the service reading the result
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert!(acceptor.session.as_ref().unwrap().task.is_finished());
                acceptor.cancel().unwrap();
                let a = next(&mut acceptor).await;
                assert!(matches!(a, PairEvent::Failed(_)), "{a:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn requests_are_refused_unless_accepting() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut acceptor, _, port) = endpoint().await;
                let (mut initiator, _, _) = endpoint().await;
                initiator.start(target(port), 4343).unwrap();
                let event = tokio::select! {
                    e = next(&mut initiator) => e,
                    e = acceptor.event() => panic!("acceptor reported {e:?}"),
                };
                let PairEvent::Failed(reason) = event else {
                    panic!("expected a refusal: {event:?}");
                };
                assert!(reason.contains("not accepting"), "{reason}");
            })
            .await;
    }
}

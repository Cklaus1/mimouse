//! The agent channel: TLS on the MiMouse TCP port with ALPN
//! [`proto::ALPN`], frames as in [`proto`]. A target serves peers in
//! `authorized_fingerprints` ([`serve`]); a requester keeps one connection
//! per target client ([`Remote`]), verifying it like clipboard sends do
//! (`ClientManager::server_policy`, pinned after the first answer).

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};

use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::{
    io::{ReadHalf, WriteHalf, split},
    net::TcpStream,
    sync::{Mutex, broadcast, mpsc, oneshot},
    task::{JoinHandle, spawn_local},
    time::{Instant, interval, sleep_until, timeout},
};
use tokio_rustls::{StartHandshake, TlsConnector, client};
use webrtc_dtls::crypto::Certificate;

use super::{
    proto::{self, EndReason, Envelope, ErrorCode, Event, Read, Request, Response},
    target::{Desk, Target},
};
use crate::{
    clipboard::{CONNECT_TIMEOUT, HANDSHAKE_TIMEOUT, Incoming, REVOCATION_CHECK},
    config::Requester,
    crypto::{self, peers::Policy},
    ipc::ClientHandle,
};

/// A connection without frames for this long is closed; at least the
/// session idle timeout, since closing it ends the session.
pub(crate) const MIN_CONNECTION_IDLE: Duration = Duration::from_secs(60);
/// how long a requester waits for an answer, beyond an observe timeout
pub(crate) const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
const _: () = assert!(input_emulation::MAX_TYPE_PACING.as_millis() < ANSWER_TIMEOUT.as_millis());
/// how long a requester tries to connect, all of a client's addresses
/// together
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

const _: () = assert!(
    CONNECT_TIMEOUT.as_millis() + HANDSHAKE_TIMEOUT.as_millis() <= DIAL_TIMEOUT.as_millis()
);

/// whether a TLS client asks for an agent connection
pub(crate) fn is_agent(start: &StartHandshake<TcpStream>) -> bool {
    start
        .client_hello()
        .alpn()
        .is_some_and(|mut protocols| protocols.any(|p| p == proto::ALPN))
}

/// TLS of the agent channel on both ends
pub(crate) struct Tls {
    server: Arc<ServerConfig>,
    cert: Certificate,
    authorized: Arc<RwLock<HashMap<String, String>>>,
}

impl Tls {
    pub(crate) fn new(
        cert: &Certificate,
        authorized: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, rustls::Error> {
        let mut server =
            crypto::peers::tls_server_config(cert, Policy::Authorized(authorized.clone()))?;
        server.alpn_protocols = vec![proto::ALPN.to_vec()];
        Ok(Self {
            server: Arc::new(server),
            cert: cert.clone(),
            authorized,
        })
    }

    fn connector(&self, policy: Policy) -> Result<TlsConnector, rustls::Error> {
        let mut client = crypto::peers::tls_client_config(&self.cert, policy)?;
        client.alpn_protocols = vec![proto::ALPN.to_vec()];
        Ok(TlsConnector::from(Arc::new(client)))
    }
}

fn fingerprint(certs: Option<&[CertificateDer]>) -> io::Result<String> {
    certs
        .and_then(|c| c.first())
        .map(|c| crypto::generate_fingerprint(c))
        .ok_or_else(|| io::Error::other("no certificate"))
}

/// a peer's request as the target audits it
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Audited {
    pub(crate) requester: Requester,
    pub(crate) request: serde_json::Value,
    pub(crate) result: Result<(), String>,
}

/// frames read off a connection, by a task of their own: reading is not
/// cancel-safe, the connection's loop must be
fn reader<R>(mut r: R) -> (mpsc::Receiver<io::Result<Option<Read>>>, Aborting)
where
    R: tokio::io::AsyncRead + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel(1);
    let task = spawn_local(async move {
        loop {
            let read = proto::read(&mut r).await;
            let more = matches!(read, Ok(Some(_)));
            if tx.send(read).await.is_err() || !more {
                return;
            }
        }
    });
    (rx, Aborting(task))
}

/// aborts its task when dropped
struct Aborting(JoinHandle<()>);

impl Drop for Aborting {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Serves one peer's agent connection until it closes, its fingerprint is
/// removed from `authorized_fingerprints`, or it stays silent too long.
/// Closing the connection ends the peer's session.
pub(crate) async fn serve<D: Desk>(
    target: Target<D>,
    tls: Rc<Tls>,
    (start, peer, permit): Incoming,
    audit: mpsc::UnboundedSender<Audited>,
) {
    let _permit = permit;
    let stream = match timeout(HANDSHAKE_TIMEOUT, start.into_stream(tls.server.clone())).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return log::warn!("agent: {peer}: {e}"),
        Err(_) => return log::warn!("agent: {peer}: handshake timed out"),
    };
    let fingerprint = match fingerprint(stream.get_ref().1.peer_certificates()) {
        Ok(f) => f,
        Err(e) => return log::warn!("agent: {peer}: {e}"),
    };
    log::info!("agent: connection from {peer} ({fingerprint})");
    let requester = Requester::Peer(fingerprint.clone());
    let policy = Policy::Authorized(tls.authorized.clone());
    let reason = serve_frames(&target, stream, &requester, &policy, &audit).await;
    log::info!("agent: connection from {peer} closed: {reason}");
    target.end_requester(&requester, reason);
}

/// the session end reason that closing the connection implies
async fn serve_frames<D: Desk, S>(
    target: &Target<D>,
    stream: S,
    requester: &Requester,
    policy: &Policy,
    audit: &mpsc::UnboundedSender<Audited>,
) -> EndReason
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + 'static,
{
    let Requester::Peer(fingerprint) = requester else {
        unreachable!("peers only")
    };
    let (reader_half, mut writer) = split(stream);
    let (mut frames, _reading) = reader(reader_half);
    let mut ended = target.ended();
    let mut check = interval(REVOCATION_CHECK);
    let mut last = Instant::now();
    loop {
        let idle = target.idle_timeout().max(MIN_CONNECTION_IDLE);
        let message = tokio::select! {
            frame = frames.recv() => frame,
            ended = ended.recv() => {
                match ended {
                    Ok((who, reason)) if who == *requester => {
                        let event = Envelope::Event { event: Event::Ended { reason } };
                        if proto::write(&mut writer, &event).await.is_err() {
                            return EndReason::Closed;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return EndReason::Shutdown,
                    _ => {}
                }
                continue;
            }
            _ = check.tick() => {
                if policy.check(fingerprint).is_err() {
                    return EndReason::Revoked;
                }
                continue;
            }
            _ = sleep_until(last + idle) => return EndReason::Idle,
        };
        last = Instant::now();
        let (id, request) = match message {
            Some(Ok(Some(Read::Message(Envelope::Request { id, request })))) => (id, request),
            Some(Ok(Some(Read::Invalid {
                id: Some(id),
                error,
            }))) => {
                let response = Response::error(ErrorCode::BadRequest, error);
                let reply = Envelope::Response { id, response };
                if proto::write(&mut writer, &reply).await.is_err() {
                    return EndReason::Closed;
                }
                continue;
            }
            Some(Ok(Some(Read::Message(_) | Read::Invalid { id: None, .. }))) => {
                log::warn!("agent: {fingerprint} sent a frame that is no request");
                return EndReason::Closed;
            }
            Some(Err(e)) => {
                log::warn!("agent: {fingerprint}: {e}");
                return EndReason::Closed;
            }
            Some(Ok(None)) | None => return EndReason::Closed,
        };
        let audited = request.audit();
        // a long request (observing) must not delay a revocation
        let revoked = async {
            loop {
                check.tick().await;
                if policy.check(fingerprint).is_err() {
                    return;
                }
            }
        };
        let response = tokio::select! {
            response = target.handle(requester, request) => response,
            () = revoked => {
                let _ = audit.send(Audited {
                    requester: requester.clone(),
                    request: audited,
                    result: Err("revoked".into()),
                });
                return EndReason::Revoked;
            }
        };
        let result = match &response {
            Response::Error { code, .. } => Err(code.to_string()),
            _ => Ok(()),
        };
        let _ = audit.send(Audited {
            requester: requester.clone(),
            request: audited,
            result,
        });
        let reply = Envelope::Response { id, response };
        if let Err(e) = proto::write(&mut writer, &reply).await {
            log::warn!("agent: {fingerprint}: {e}");
            return EndReason::Closed;
        }
        last = Instant::now();
    }
}

/// a target's session ended, as its requester heard
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteEnded {
    pub(crate) handle: ClientHandle,
    pub(crate) reason: EndReason,
}

/// where and how to reach a target client
pub(crate) struct Dial<T: Fn(SocketAddr, &str) -> bool> {
    pub(crate) addrs: Vec<SocketAddr>,
    pub(crate) policy: Policy,
    /// pins the target's fingerprint on first use; false: refused
    pub(crate) trust: T,
}

/// the requester side: one connection per target client
pub(crate) struct Remote {
    tls: Rc<Tls>,
    conns: RefCell<HashMap<ClientHandle, Rc<Conn>>>,
    events: mpsc::UnboundedSender<RemoteEnded>,
    /// one dial at a time per target: a second connection to a target would
    /// end the session of the first when either closes
    dialing: RefCell<HashMap<ClientHandle, Rc<Mutex<()>>>>,
}

struct Conn {
    writer: Mutex<WriteHalf<client::TlsStream<TcpStream>>>,
    pending: Rc<RefCell<HashMap<u64, oneshot::Sender<Response>>>>,
    next_id: Cell<u64>,
    addr: SocketAddr,
    fingerprint: String,
    /// the fingerprint was pinned (the target answered once)
    trusted: Cell<bool>,
    reading: Aborting,
}

impl Conn {
    fn closed(&self) -> bool {
        self.reading.0.is_finished()
    }
}

impl Remote {
    pub(crate) fn new(tls: Rc<Tls>) -> (Self, mpsc::UnboundedReceiver<RemoteEnded>) {
        let (events, rx) = mpsc::unbounded_channel();
        let remote = Self {
            tls,
            conns: Default::default(),
            events,
            dialing: RefCell::default(),
        };
        (remote, rx)
    }

    /// closes the connection to a removed client
    pub(crate) fn forget(&self, handle: ClientHandle) {
        self.conns.borrow_mut().remove(&handle);
        self.dialing.borrow_mut().remove(&handle);
    }

    /// sends `request` to the target client `handle` and waits for its answer
    pub(crate) async fn request<T: Fn(SocketAddr, &str) -> bool>(
        &self,
        handle: ClientHandle,
        dial: Dial<T>,
        request: Request,
    ) -> Response {
        let wait = ANSWER_TIMEOUT
            + request
                .observe()
                .map(|o| {
                    Duration::from_millis(
                        o.timeout_ms
                            .unwrap_or(proto::Observe::DEFAULT_TIMEOUT_MS)
                            .min(proto::Observe::MAX_TIMEOUT_MS)
                            .into(),
                    )
                })
                .unwrap_or_default();
        let conn = match timeout(DIAL_TIMEOUT, self.conn(handle, &dial)).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Response::error(ErrorCode::Unreachable, e),
            Err(_) => {
                return Response::error(
                    ErrorCode::Unreachable,
                    format!("no connection within {} s", DIAL_TIMEOUT.as_secs()),
                );
            }
        };
        let id = conn.next_id.get();
        conn.next_id.set(id + 1);
        let (tx, rx) = oneshot::channel();
        conn.pending.borrow_mut().insert(id, tx);
        let sent = {
            let mut writer = conn.writer.lock().await;
            proto::write(&mut *writer, &Envelope::Request { id, request }).await
        };
        if let Err(e) = sent {
            conn.pending.borrow_mut().remove(&id);
            self.drop_conn(handle, &conn);
            return Response::error(ErrorCode::Unreachable, format!("sending failed: {e}"));
        }
        let response = match timeout(wait, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.drop_conn(handle, &conn);
                // a target refusing this machine's certificate closes the
                // connection right after the handshake
                let why = match conn.trusted.get() {
                    true => "the target closed the connection".to_owned(),
                    false => format!(
                        "{} closed the connection; it may not authorize this machine (pair first)",
                        conn.addr
                    ),
                };
                return Response::error(ErrorCode::Unreachable, why);
            }
            Err(_) => {
                conn.pending.borrow_mut().remove(&id);
                self.drop_conn(handle, &conn);
                return Response::error(
                    ErrorCode::Unreachable,
                    format!("no answer within {} s", wait.as_secs()),
                );
            }
        };
        if !conn.trusted.get() {
            if !(dial.trust)(conn.addr, &conn.fingerprint) {
                self.drop_conn(handle, &conn);
                return Response::error(
                    ErrorCode::Unreachable,
                    format!(
                        "{} presented certificate {}, which this client is not pinned to",
                        conn.addr, conn.fingerprint
                    ),
                );
            }
            conn.trusted.set(true);
        }
        response
    }

    fn drop_conn(&self, handle: ClientHandle, conn: &Rc<Conn>) {
        let mut conns = self.conns.borrow_mut();
        if conns.get(&handle).is_some_and(|c| Rc::ptr_eq(c, conn)) {
            conns.remove(&handle);
        }
    }

    async fn conn<T: Fn(SocketAddr, &str) -> bool>(
        &self,
        handle: ClientHandle,
        dial: &Dial<T>,
    ) -> Result<Rc<Conn>, String> {
        let cached = || {
            let conns = self.conns.borrow();
            conns.get(&handle).filter(|c| !c.closed()).cloned()
        };
        if let Some(conn) = cached() {
            return Ok(conn);
        }
        let lock = self.dialing.borrow_mut().entry(handle).or_default().clone();
        let _dialing = lock.lock().await;
        if let Some(conn) = cached() {
            return Ok(conn);
        }
        if dial.addrs.is_empty() {
            return Err("no address known for this client".into());
        }
        let connector = self
            .tls
            .connector(dial.policy.clone())
            .map_err(|e| e.to_string())?;
        let mut last_err = String::new();
        for &addr in &dial.addrs {
            match connect(&connector, addr).await {
                Ok(stream) => {
                    let conn = Rc::new(self.start(handle, addr, stream)?);
                    self.conns.borrow_mut().insert(handle, conn.clone());
                    return Ok(conn);
                }
                Err(e) => {
                    log::debug!("agent: {addr}: {e}");
                    last_err = format!("{addr}: {e}");
                }
            }
        }
        Err(last_err)
    }

    fn start(
        &self,
        handle: ClientHandle,
        addr: SocketAddr,
        stream: client::TlsStream<TcpStream>,
    ) -> Result<Conn, String> {
        let fingerprint =
            fingerprint(stream.get_ref().1.peer_certificates()).map_err(|e| e.to_string())?;
        let (reader_half, writer) = split(stream);
        let pending: Rc<RefCell<HashMap<u64, oneshot::Sender<Response>>>> = Default::default();
        let task = spawn_local(read_answers(
            reader_half,
            pending.clone(),
            handle,
            self.events.clone(),
        ));
        Ok(Conn {
            writer: Mutex::new(writer),
            pending,
            next_id: Cell::new(1),
            addr,
            fingerprint,
            trusted: Cell::new(false),
            reading: Aborting(task),
        })
    }
}

async fn connect(
    connector: &TlsConnector,
    addr: SocketAddr,
) -> io::Result<client::TlsStream<TcpStream>> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;
    let name = ServerName::try_from("mimouse").expect("valid server name");
    timeout(HANDSHAKE_TIMEOUT, connector.connect(name, stream)).await?
}

/// hands answers to their requests and reports sessions ending; pending
/// requests fail when the connection closes (their senders drop)
async fn read_answers(
    mut r: ReadHalf<client::TlsStream<TcpStream>>,
    pending: Rc<RefCell<HashMap<u64, oneshot::Sender<Response>>>>,
    handle: ClientHandle,
    events: mpsc::UnboundedSender<RemoteEnded>,
) {
    loop {
        match proto::read(&mut r).await {
            Ok(Some(Read::Message(Envelope::Response { id, response }))) => {
                if let Some(tx) = pending.borrow_mut().remove(&id) {
                    let _ = tx.send(response);
                }
            }
            Ok(Some(Read::Message(Envelope::Event {
                event: Event::Ended { reason },
            }))) => {
                let _ = events.send(RemoteEnded { handle, reason });
            }
            Ok(Some(Read::Message(Envelope::Request { .. }) | Read::Invalid { .. })) => {
                log::warn!("agent: client {handle} sent a frame that is no answer");
                break;
            }
            Ok(None) => break,
            Err(e) => {
                log::debug!("agent: client {handle}: {e}");
                break;
            }
        }
    }
    pending.borrow_mut().clear();
}

#[cfg(test)]
mod tests {
    use input_event::scancode::Linux;
    use tokio::{
        io::{AsyncWriteExt, DuplexStream, duplex},
        task::LocalSet,
    };

    use super::*;
    use crate::agent::{
        fake::{FakeDesk, allow_all},
        target::Policy as Grants,
    };

    const FP: &str = "peer-fingerprint";

    struct Peer {
        stream: DuplexStream,
        authorized: Arc<RwLock<HashMap<String, String>>>,
        serving: JoinHandle<EndReason>,
        audit: mpsc::UnboundedReceiver<Audited>,
    }

    fn serve_peer() -> Peer {
        let mut peers = HashMap::new();
        peers.insert(FP.to_owned(), allow_all());
        let grants = Grants {
            local: Default::default(),
            peers,
            idle_timeout: Duration::from_secs(60),
        };
        let (target, _events) = Target::new(FakeDesk::default(), grants);
        let authorized = Arc::new(RwLock::new(HashMap::from([(
            FP.to_owned(),
            "peer".to_owned(),
        )])));
        let policy = Policy::Authorized(authorized.clone());
        let (audit_tx, audit) = mpsc::unbounded_channel();
        let (ours, theirs) = duplex(64 * 1024);
        let serving = spawn_local(async move {
            let requester = Requester::Peer(FP.to_owned());
            serve_frames(&target, theirs, &requester, &policy, &audit_tx).await
        });
        Peer {
            stream: ours,
            authorized,
            serving,
            audit,
        }
    }

    async fn send_raw(stream: &mut DuplexStream, json: &str) {
        stream
            .write_all(&(json.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(json.as_bytes()).await.unwrap();
    }

    async fn response(stream: &mut DuplexStream) -> (u64, Response) {
        match proto::read(stream).await.unwrap() {
            Some(Read::Message(Envelope::Response { id, response })) => (id, response),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_request_gets_an_error_and_the_connection_stays() {
        LocalSet::new()
            .run_until(async {
                let mut peer = serve_peer();
                send_raw(&mut peer.stream, r#"{"id":7,"request":{"op":"teleport"}}"#).await;
                let (id, answer) = response(&mut peer.stream).await;
                assert_eq!(id, 7);
                assert!(
                    matches!(
                        answer,
                        Response::Error {
                            code: ErrorCode::BadRequest,
                            ..
                        }
                    ),
                    "{answer:?}"
                );
                let request = Envelope::Request {
                    id: 8,
                    request: Request::Key {
                        keys: vec![Linux::KeyA],
                        observe: None,
                    },
                };
                proto::write(&mut peer.stream, &request).await.unwrap();
                assert_eq!(
                    response(&mut peer.stream).await,
                    (8, Response::Ok { frame: None })
                );
                let audited = peer.audit.recv().await.unwrap();
                assert_eq!(audited.requester, Requester::Peer(FP.to_owned()));
                assert_eq!(audited.result, Ok(()));
                drop(peer.stream);
                assert_eq!(peer.serving.await.unwrap(), EndReason::Closed);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn removing_the_fingerprint_closes_the_connection() {
        LocalSet::new()
            .run_until(async {
                let mut peer = serve_peer();
                let hello = Envelope::Request {
                    id: 1,
                    request: Request::Hello,
                };
                proto::write(&mut peer.stream, &hello).await.unwrap();
                response(&mut peer.stream).await;
                peer.authorized.write().unwrap().remove(FP);
                assert_eq!(peer.serving.await.unwrap(), EndReason::Revoked);
                assert!(proto::read(&mut peer.stream).await.unwrap().is_none());
            })
            .await;
    }
}

use crate::proto::{MAX_EVENT_SIZE, ProtoEvent};
use futures::{Stream, StreamExt};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    collections::HashMap,
    future::{Future, ready},
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, JoinSet, spawn_local},
};
use webrtc_dtls::{conn::DTLSConn, content::ContentType, crypto::Certificate};
use webrtc_util::{
    Conn,
    conn::{Listener, conn_udp_listener::ListenConfig},
};

use crate::crypto::{self, peers};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// a peer that completed the handshake
#[derive(Clone)]
struct Peer {
    addr: SocketAddr,
    /// of the certificate it authenticated with
    fingerprint: String,
    conn: ArcConn,
}

type Peers = Rc<AsyncMutex<Vec<Peer>>>;

/// a handshake stalled this long is dropped (the client gives up after 5 s)
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// clients ping every 500 ms; a connection silent this long is closed
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// closing a UDP conn under a live DTLS conn makes the DTLS reader task spin,
/// so the UDP listener must forget idle remotes only after both timeouts
/// above had DTLS close them
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct MiMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Peers,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
}

/// a UDP listener on `port` that opens a conn per remote sending a DTLS
/// handshake record
async fn bind(port: u16) -> Result<impl Listener, webrtc_util::Error> {
    ListenConfig {
        accept_filter: Some(Box::new(
            |packet: &[u8]| -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> {
                let handshake = packet
                    .first()
                    .is_some_and(|&t| ContentType::from(t) == ContentType::Handshake);
                Box::pin(ready(handshake))
            },
        )),
        idle_timeout: UDP_IDLE_TIMEOUT,
        ..Default::default()
    }
    .listen(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))
    .await
}

impl MiMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();

        let mut listener = bind(port).await?;

        let conns: Peers = Rc::new(AsyncMutex::new(Vec::new()));

        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let conns = conns.clone();
            spawn_local(async move {
                // aborts the peer tasks when this task is aborted
                let mut peers = JoinSet::new();
                loop {
                    tokio::select! {
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                peers.spawn_local(serve(
                                    conn,
                                    addr,
                                    cert.clone(),
                                    authorized_keys.clone(),
                                    conns.clone(),
                                    listen_tx.clone(),
                                ));
                            }
                            Err(e) => log::warn!("accept: {e}"),
                        },
                        Some(_) = peers.join_next() => {}
                        port = request_port_change_rx.recv() => {
                            let port = port.expect("channel closed");
                            match bind(port).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    port_changed_tx.send(Ok(port)).expect("channel closed");
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    port_changed_tx.send(Err(e.into())).expect("channel closed");
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for peer in conns.iter() {
            let _ = peer.conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let conns = self.conns.lock().await;
        for peer in conns.iter() {
            if peer.addr == addr {
                let _ = peer.conn.send(&buf[..len]).await;
            }
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        let conns = self.conns.lock().await;
        let peer = conns.iter().find(|p| p.addr == addr)?;
        Some(peer.fingerprint.clone())
    }
}

impl Stream for MiMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

/// handshakes over `conn`, then forwards the peer's events until it is gone
/// or no longer authorized; forgets the remote in the UDP listener on every
/// exit
async fn serve(
    conn: ArcConn,
    addr: SocketAddr,
    cert: Certificate,
    authorized: Arc<RwLock<HashMap<String, String>>>,
    conns: Peers,
    listen_tx: Sender<ListenEvent>,
) {
    let policy = peers::Policy::Authorized(authorized);
    let (cfg, rejected) = peers::dtls_server_config(cert, policy.clone());
    let handshake = DTLSConn::new(conn.clone(), cfg, false, None);
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(dtls)) => {
            let certs = dtls.connection_state().await.peer_certificates;
            let dtls: ArcConn = Arc::new(dtls);
            if let Some(cert) = certs.first() {
                log::info!("dtls client connected, ip: {addr}");
                let peer = Peer {
                    addr,
                    fingerprint: crypto::generate_fingerprint(cert),
                    conn: dtls.clone(),
                };
                conns.lock().await.push(peer.clone());
                let fingerprint = peer.fingerprint.clone();
                if listen_tx
                    .send(ListenEvent::Accept { addr, fingerprint })
                    .is_ok()
                {
                    read_loop(conns, peer, &policy, listen_tx, PEER_IDLE_TIMEOUT).await;
                }
            } else {
                log::warn!("{addr}: no client certificate");
            }
            let _ = dtls.close().await;
        }
        Ok(Err(e)) => match rejected.get() {
            Some(fingerprint) => {
                let fingerprint = fingerprint.clone();
                let _ = listen_tx.send(ListenEvent::Rejected { fingerprint });
            }
            None => log::warn!("{addr}: handshake failed: {e}"),
        },
        Err(_) => log::warn!("{addr}: no handshake within {HANDSHAKE_TIMEOUT:?}"),
    }
    // also when closing the DTLS conn failed before closing this one
    let _ = conn.close().await;
}

/// forwards events from `peer` until it fails, stays silent for
/// `idle_timeout` or `policy` no longer trusts its fingerprint (a peer pings
/// every 500 ms, so a revoked one is dropped within that)
async fn read_loop(
    conns: Peers,
    peer: Peer,
    policy: &peers::Policy,
    dtls_tx: Sender<ListenEvent>,
    idle_timeout: Duration,
) {
    let Peer { addr, conn, .. } = &peer;
    let mut b = [0u8; MAX_EVENT_SIZE];

    loop {
        let n = match tokio::time::timeout(idle_timeout, conn.recv(&mut b)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break,
            Err(_) => {
                log::info!("{addr}: silent for {idle_timeout:?}");
                break;
            }
        };
        if let Err(e) = policy.check(&peer.fingerprint) {
            log::warn!("{addr}: closing, {e}");
            break;
        }
        // one corrupt datagram or an event id from a newer peer must not
        // drop the connection (upstream 82766cd, lan-mouse#307)
        match ProtoEvent::decode(&b[..n]) {
            Ok(event) => {
                let addr = *addr;
                if dtls_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    break;
                }
            }
            Err(e) => log::warn!("{addr}: skipping undecodable datagram: {e}"),
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    conns.lock().await.retain(|p| !Arc::ptr_eq(&p.conn, conn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::{net::UdpSocket, task::LocalSet};
    use webrtc_util::conn::conn_pipe::pipe;

    /// a peer on one end of a pipe, its list of peers, and the other end
    fn piped_peer() -> (Peer, Peers, impl Conn) {
        let (local, remote) = pipe();
        let peer = Peer {
            addr: "192.0.2.1:4242".parse().unwrap(),
            fingerprint: "aa:bb".into(),
            conn: Arc::new(local),
        };
        let conns = Rc::new(AsyncMutex::new(vec![peer.clone()]));
        (peer, conns, remote)
    }

    #[tokio::test]
    async fn read_loop_skips_undecodable_datagrams() {
        let (peer, conns, remote) = piped_peer();
        let addr = peer.addr;
        let (tx, mut rx) = channel();

        // unknown event id, `Enter` with an invalid position, then a Ping
        remote.send(&[0xee, 1, 2, 3]).await.unwrap();
        remote.send(&[8, 9]).await.unwrap();
        let (ping, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
        remote.send(&ping[..len]).await.unwrap();
        drop(remote);

        let any = peers::Policy::Any;
        read_loop(conns.clone(), peer, &any, tx, PEER_IDLE_TIMEOUT).await;

        let mut received = vec![];
        while let Some(event) = rx.recv().await {
            match event {
                ListenEvent::Msg { event, addr: from } => {
                    assert_eq!(from, addr);
                    received.push(event);
                }
                _ => panic!("unexpected listen event"),
            }
        }
        assert!(matches!(received[..], [ProtoEvent::Ping]), "{received:?}");
        // the connection is dropped only once the transport fails
        assert!(conns.lock().await.is_empty());
    }

    #[tokio::test]
    async fn read_loop_drops_silent_connections() {
        let (peer, conns, _remote) = piped_peer();
        let (tx, _rx) = channel();

        let any = peers::Policy::Any;
        let silent = read_loop(conns.clone(), peer, &any, tx, Duration::from_millis(100));
        tokio::time::timeout(Duration::from_secs(2), silent)
            .await
            .expect("silent connection kept");
        assert!(conns.lock().await.is_empty());
    }

    type Keys = Arc<RwLock<HashMap<String, String>>>;

    /// a listener on a free loopback port trusting `client`, and its
    /// authorized keys
    async fn listener(client: &Certificate) -> (MiMouseListener, SocketAddr, Keys) {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .and_then(|s| s.local_addr())
            .expect("free port")
            .port();
        let fingerprint = crypto::certificate_fingerprint(client);
        let authorized = Arc::new(RwLock::new(HashMap::from([(fingerprint, "test".into())])));
        let server = Certificate::generate_self_signed(["ignored".to_owned()]).expect("cert");
        let listener = MiMouseListener::new(port, server, authorized.clone())
            .await
            .expect("listener");
        (
            listener,
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            authorized,
        )
    }

    fn client_config(cert: Certificate) -> webrtc_dtls::config::Config {
        peers::dtls_client_config(cert, peers::Policy::Any).0
    }

    /// a socket on loopback sending to `addr`
    async fn socket(addr: SocketAddr) -> Arc<UdpSocket> {
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        socket.connect(addr).await.expect("connect");
        Arc::new(socket)
    }

    /// a DTLS handshake with the server at `addr`, `None` if it failed
    async fn dial(addr: SocketAddr, cert: Certificate) -> Option<DTLSConn> {
        let handshake = DTLSConn::new(socket(addr).await, client_config(cert), true, None);
        tokio::time::timeout(Duration::from_secs(8), handshake)
            .await
            .ok()?
            .ok()
    }

    /// the next listener event other than a message
    async fn next_peer_event(listener: &mut MiMouseListener) -> ListenEvent {
        let event = async {
            loop {
                match listener.next().await.expect("listener closed") {
                    ListenEvent::Msg { .. } => continue,
                    e => return e,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), event)
            .await
            .expect("no peer event")
    }

    /// the first datagram a DTLS client sends: a ClientHello
    async fn client_hello() -> Vec<u8> {
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = socket(sink.local_addr().unwrap()).await;
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        let client = spawn_local(DTLSConn::new(socket, client_config(cert), true, None));
        let mut hello = vec![0u8; 2048];
        let n = sink.recv(&mut hello).await.unwrap();
        client.abort();
        hello.truncate(n);
        hello
    }

    /// forwards datagrams between one client and `server`, holding the
    /// client's second datagram back for `delay`; returns the address for the
    /// client and the one the server sees
    async fn delaying_relay(server: SocketAddr, delay: Duration) -> (SocketAddr, SocketAddr) {
        let front = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let back = socket(server).await;
        let addrs = (front.local_addr().unwrap(), back.local_addr().unwrap());
        spawn_local(async move {
            let (mut up, mut down) = ([0u8; 4096], [0u8; 4096]);
            let mut client = None;
            let mut forwarded = 0;
            loop {
                tokio::select! {
                    Ok((n, from)) = front.recv_from(&mut up) => {
                        client = Some(from);
                        forwarded += 1;
                        if forwarded == 2 {
                            tokio::time::sleep(delay).await;
                        }
                        let _ = back.send(&up[..n]).await;
                    }
                    Ok(n) = back.recv(&mut down) => {
                        if let Some(client) = client {
                            let _ = front.send_to(&down[..n], client).await;
                        }
                    }
                    else => break,
                }
            }
        });
        addrs
    }

    /// B1: a handshake no longer blocks accepting other peers
    #[tokio::test]
    async fn silent_client_hello_does_not_delay_other_handshakes() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, addr, _) = listener(&cert).await;
                let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                silent.send_to(&client_hello().await, addr).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;

                let start = Instant::now();
                let (client, event) =
                    tokio::join!(dial(addr, cert.clone()), next_peer_event(&mut listener));
                assert!(client.is_some(), "client handshake failed");
                let ListenEvent::Accept { fingerprint, .. } = event else {
                    panic!("not accepted");
                };
                assert_eq!(fingerprint, crypto::certificate_fingerprint(&cert));
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "{:?}",
                    start.elapsed()
                );
                listener.terminate().await;
            })
            .await;
    }

    /// B1: a handshake may take longer than the old 2 s accept window
    #[tokio::test]
    async fn handshake_slower_than_2_s_completes() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, addr, _) = listener(&cert).await;
                let (relay, relayed) = delaying_relay(addr, Duration::from_secs(3)).await;

                let start = Instant::now();
                let (client, event) =
                    tokio::join!(dial(relay, cert), next_peer_event(&mut listener));
                assert!(client.is_some(), "client handshake failed");
                let ListenEvent::Accept { addr, .. } = event else {
                    panic!("not accepted");
                };
                assert_eq!(addr, relayed);
                assert!(start.elapsed() > Duration::from_secs(3));
                listener.terminate().await;
            })
            .await;
    }

    /// each rejection reports the fingerprint of its own handshake
    #[tokio::test]
    async fn unauthorized_clients_are_reported_with_their_fingerprint() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, addr, _) = listener(&cert).await;
                let strangers: Vec<_> = (0..2)
                    .map(|_| Certificate::generate_self_signed(["ignored".to_owned()]).unwrap())
                    .collect();
                for stranger in &strangers {
                    spawn_local(dial(addr, stranger.clone()));
                }
                let mut rejected = vec![];
                for _ in &strangers {
                    match next_peer_event(&mut listener).await {
                        ListenEvent::Rejected { fingerprint } => rejected.push(fingerprint),
                        _ => panic!("stranger accepted"),
                    }
                }
                let mut expected: Vec<_> = strangers
                    .iter()
                    .map(crypto::certificate_fingerprint)
                    .collect();
                expected.sort();
                rejected.sort();
                assert_eq!(rejected, expected);
                listener.terminate().await;
            })
            .await;
    }

    /// B2: removing a key closes the open connection authenticated with it
    /// and refuses its next handshake
    #[tokio::test]
    async fn revoked_key_closes_connection() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, addr, keys) = listener(&cert).await;
                let (client, event) =
                    tokio::join!(dial(addr, cert.clone()), next_peer_event(&mut listener));
                let client = client.expect("client handshake failed");
                let ListenEvent::Accept { addr: peer, .. } = event else {
                    panic!("not accepted");
                };

                keys.write().unwrap().clear();
                let (ping, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
                client.send(&ping[..len]).await.unwrap();
                let mut buf = [0u8; MAX_EVENT_SIZE];
                let closed = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
                    .await
                    .expect("revoked connection kept open");
                assert!(closed.is_err(), "{closed:?}");
                assert_eq!(listener.get_certificate_fingerprint(peer).await, None);
                assert!(
                    !matches!(
                        next_event(&mut listener).await,
                        Some(ListenEvent::Msg { .. })
                    ),
                    "event forwarded after revocation"
                );

                let (client, event) =
                    tokio::join!(dial(addr, cert), next_peer_event(&mut listener));
                assert!(client.is_none(), "revoked client accepted again");
                assert!(matches!(event, ListenEvent::Rejected { .. }));
                listener.terminate().await;
            })
            .await;
    }

    /// the next listener event if one is queued
    async fn next_event(listener: &mut MiMouseListener) -> Option<ListenEvent> {
        tokio::time::timeout(Duration::from_millis(50), listener.next())
            .await
            .ok()
            .flatten()
    }
}

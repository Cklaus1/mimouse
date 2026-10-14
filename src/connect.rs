use crate::client::ClientManager;
use crate::crypto::{self, peers::Policy};
use crate::dns::DnsRequester;
use crate::ipc::{ClientHandle, DEFAULT_PORT};
use crate::proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::{Mutex, Notify},
    task::{JoinSet, spawn_local},
};
use tokio_util::sync::CancellationToken;
use webrtc_dtls::{conn::DTLSConn, crypto::Certificate};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum MiMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
    #[error("server certificate {0} is not the one pinned for this client")]
    Untrusted(String),
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// a DTLS connection to `addr` whose server `policy` trusts, with the
/// server's certificate fingerprint
async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    policy: Policy,
) -> Result<(ArcConn, SocketAddr, String), (SocketAddr, MiMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let (config, rejected) = crypto::peers::dtls_client_config(cert, policy);
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    let dtls_conn = tokio::select! {
        _ = timeout => return Err((addr, MiMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => dtls_conn,
            Err(e) => {
                let e = match rejected.get() {
                    Some(fingerprint) => MiMouseConnectionError::Untrusted(fingerprint.clone()),
                    None => e.into(),
                };
                return Err((addr, e));
            }
        }
    };
    let state = dtls_conn.connection_state().await;
    let Some(fingerprint) = state
        .peer_certificates
        .first()
        .map(|c| crypto::generate_fingerprint(c))
    else {
        let _ = dtls_conn.close().await;
        return Err((addr, webrtc_dtls::Error::ErrNoCertificates.into()));
    };
    Ok((Arc::new(dtls_conn), addr, fingerprint))
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
    policy: Policy,
) -> Result<(ArcConn, SocketAddr, String), MiMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone(), policy.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(MiMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// what [`MiMouseConnection::recv`] reports about a client
#[derive(Debug)]
pub(crate) enum ConnEvent {
    /// an event sent by the client
    Received(ProtoEvent),
    /// the connection closed: ping timeout, recv or send error
    Closed,
}

/// outgoing connection state, shared by [`MiMouseConnection`] (owned by the
/// capture task) and the service, which keeps active clients connected
#[derive(Clone)]
pub(crate) struct Connector {
    cert: Certificate,
    client_manager: ClientManager,
    /// open connections by peer address, with the client each belongs to
    conns: Rc<Mutex<HashMap<SocketAddr, (ClientHandle, ArcConn)>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_tx: Sender<(ClientHandle, ConnEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// notified whenever a connection attempt ends or a connection closes
    changed: Rc<Notify>,
    /// cancels the reconnect task of each client kept connected
    keepers: Rc<RefCell<HashMap<ClientHandle, CancellationToken>>>,
    /// re-resolves host names before reconnect attempts, once set
    resolver: Rc<RefCell<Option<DnsRequester>>>,
}

pub(crate) struct MiMouseConnection {
    connector: Connector,
    recv_rx: Receiver<(ClientHandle, ConnEvent)>,
}

impl MiMouseConnection {
    pub(crate) fn new(cert: Certificate, client_manager: ClientManager) -> Self {
        let (recv_tx, recv_rx) = channel();
        let connector = Connector {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_tx,
            ping_response: Default::default(),
            changed: Default::default(),
            keepers: Default::default(),
            resolver: Default::default(),
        };
        Self { connector, recv_rx }
    }

    pub(crate) fn connector(&self) -> Connector {
        self.connector.clone()
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ConnEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), MiMouseConnectionError> {
        let c = &self.connector;
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = c.client_manager.active_addr(handle) {
            let conn = {
                let conns = c.conns.lock().await;
                conns
                    .get(&addr)
                    .filter(|(owner, _)| *owner == handle)
                    .map(|(_, conn)| conn.clone())
            };
            if let Some(conn) = conn {
                if !c.client_manager.alive(handle) {
                    return Err(MiMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        c.disconnect(handle, addr, &conn).await;
                        let _ = conn.close().await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // a client kept connected may be waiting out its backoff: try now
        let keeper = c.keepers.borrow().get(&handle).cloned();
        if let Some(token) = keeper {
            c.start_connect(handle, token).await;
        }
        Err(MiMouseConnectionError::NotConnected)
    }
}

/// delay before the next reconnect attempt: 1 s doubling up to 30 s
#[derive(Debug)]
pub(crate) struct Backoff(Duration);

impl Backoff {
    const MIN: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);

    pub(crate) fn next(&mut self) -> Duration {
        let delay = self.0;
        self.0 = (delay * 2).min(Self::MAX);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.0 = Self::MIN;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self(Self::MIN)
    }
}

impl Connector {
    /// connects to `handle` now and reconnects whenever the connection drops
    /// or an attempt fails (see [`Backoff`]), until [`Connector::stop`]
    pub(crate) fn keep_connected(&self, handle: ClientHandle) {
        let token = CancellationToken::new();
        if let Some(previous) = self.keepers.borrow_mut().insert(handle, token.clone()) {
            previous.cancel();
        }
        spawn_local(self.clone().reconnect_loop(handle, token));
    }

    /// stops reconnecting to `handle` and closes its connection
    pub(crate) fn stop(&self, handle: ClientHandle) {
        if let Some(token) = self.keepers.borrow_mut().remove(&handle) {
            token.cancel();
        }
        self.reconnect(handle);
    }

    /// closes the connection of `handle`, e.g. after its addresses changed;
    /// the reconnect loop then dials the current addresses
    pub(crate) fn reconnect(&self, handle: ClientHandle) {
        let this = self.clone();
        spawn_local(async move {
            let owned: Vec<_> = this
                .conns
                .lock()
                .await
                .iter()
                .filter(|(_, (owner, _))| *owner == handle)
                .map(|(addr, (_, conn))| (*addr, conn.clone()))
                .collect();
            for (addr, conn) in owned {
                this.disconnect(handle, addr, &conn).await;
                let _ = conn.close().await;
            }
        });
    }

    async fn reconnect_loop(self, handle: ClientHandle, token: CancellationToken) {
        let mut backoff = Backoff::default();
        let mut first_attempt = true;
        loop {
            // wait while connected or while an attempt is in flight
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_connected(handle).await {
                    backoff.reset();
                } else if !self.connecting.lock().await.contains(&handle) {
                    break;
                }
                tokio::select! {
                    _ = changed => {}
                    _ = token.cancelled() => return,
                }
            }
            if !first_attempt {
                // the name may resolve by now (e.g. the network came up
                // after the service) or point somewhere else
                self.resolve(handle);
                let delay = backoff.next();
                log::debug!("client {handle}: reconnecting in {delay:?}");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = token.cancelled() => return,
                }
            }
            first_attempt = false;
            // `send` may have connected meanwhile
            if !self.is_connected(handle).await {
                self.start_connect(handle, token.clone()).await;
            }
        }
    }

    /// lets reconnect attempts re-resolve the client's host name
    pub(crate) fn set_resolver(&self, resolver: DnsRequester) {
        self.resolver.replace(Some(resolver));
    }

    fn resolve(&self, handle: ClientHandle) {
        if let (Some(resolver), Some(hostname)) = (
            self.resolver.borrow().as_ref(),
            self.client_manager.get_hostname(handle),
        ) {
            resolver.resolve(handle, hostname);
        }
    }

    async fn is_connected(&self, handle: ClientHandle) -> bool {
        match self.client_manager.active_addr(handle) {
            Some(addr) => self
                .conns
                .lock()
                .await
                .get(&addr)
                .is_some_and(|(owner, _)| *owner == handle),
            None => false,
        }
    }

    /// starts a connection attempt in the background unless one is running;
    /// cancelling `token` (the keeper's) abandons it
    async fn start_connect(&self, handle: ClientHandle, token: CancellationToken) {
        if self.connecting.lock().await.insert(handle) {
            spawn_local(self.clone().connect_to_handle(handle, token));
        }
    }

    async fn connect_to_handle(self, handle: ClientHandle, token: CancellationToken) {
        if let Err(e) = self.establish(handle, &token).await {
            log::debug!("client {handle}: connection failed: {e}");
        }
        self.connecting.lock().await.remove(&handle);
        self.changed.notify_waiters();
    }

    async fn establish(
        &self,
        handle: ClientHandle,
        token: &CancellationToken,
    ) -> Result<(), MiMouseConnectionError> {
        log::info!("client {handle} connecting ...");
        let ips = self
            .client_manager
            .get_ips(handle)
            .ok_or(MiMouseConnectionError::NotConnected)?;
        let policy = self
            .client_manager
            .server_policy(handle)
            .ok_or(MiMouseConnectionError::NotConnected)?;
        let port = self.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = ips
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let (conn, addr, fingerprint) = tokio::select! {
            result = connect_any(&addrs, self.cert.clone(), policy) => result?,
            _ = token.cancelled() => return Err(MiMouseConnectionError::NotConnected),
        };
        let mut conns = self.conns.lock().await;
        // checked and registered without an await in between: `stop` cancels
        // the token before it closes the client's connections, so it either
        // closes this one or this attempt sees it was stopped. The client may
        // also have been moved while connecting.
        let current = !token.is_cancelled()
            && self
                .client_manager
                .get_ips(handle)
                .is_some_and(|ips| ips.contains(&addr.ip()))
            && self.client_manager.get_port(handle) == Some(addr.port());
        if !current {
            drop(conns);
            let _ = conn.close().await;
            return Err(MiMouseConnectionError::NotConnected);
        }
        if !self.client_manager.pin(handle, addr, &fingerprint) {
            drop(conns);
            let _ = conn.close().await;
            return Err(MiMouseConnectionError::Untrusted(fingerprint));
        }
        log::info!("client ({handle}) connected @ {addr}");
        self.client_manager.set_active_addr(handle, Some(addr));
        let replaced = conns.insert(addr, (handle, conn.clone()));
        drop(conns);
        if let Some((_, replaced)) = replaced {
            // e.g. the client was re-added on a config reload; its old
            // connection task ends once the closed connection fails
            let _ = replaced.close().await;
        }
        spawn_local(self.clone().run_connection(handle, addr, conn));
        Ok(())
    }

    /// pings and receives until either fails, then drops the connection
    async fn run_connection(self, handle: ClientHandle, addr: SocketAddr, conn: ArcConn) {
        tokio::select! {
            _ = ping_pong(addr, &conn, &self.ping_response) => {}
            _ = self.receive_loop(handle, addr, &conn) => {}
        }
        self.disconnect(handle, addr, &conn).await;
        let _ = conn.close().await;
    }

    async fn receive_loop(&self, handle: ClientHandle, addr: SocketAddr, conn: &ArcConn) {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        loop {
            let n = match conn.recv(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    log::warn!("{addr}: recv error `{e}`");
                    return;
                }
            };
            if let Ok(event) = ProtoEvent::decode(&buf[..n]) {
                log::trace!("{addr} <==<==<== {event}");
                // a connection closed by `stop` or replaced must not speak
                // for `handle`, which may belong to another client by now
                let current = self
                    .conns
                    .lock()
                    .await
                    .get(&addr)
                    .is_some_and(|(owner, c)| *owner == handle && Arc::ptr_eq(c, conn));
                if !current {
                    return;
                }
                match event {
                    ProtoEvent::Pong(b) => {
                        self.client_manager.set_active_addr(handle, Some(addr));
                        self.client_manager.set_alive(handle, b);
                        self.ping_response.borrow_mut().insert(addr);
                    }
                    event => self
                        .recv_tx
                        .send((handle, ConnEvent::Received(event)))
                        .expect("channel closed"),
                }
            }
        }
    }

    /// forgets `conn`, unless it was already replaced or removed
    async fn disconnect(&self, handle: ClientHandle, addr: SocketAddr, conn: &ArcConn) {
        let active: Vec<SocketAddr> = {
            let mut conns = self.conns.lock().await;
            if !conns.get(&addr).is_some_and(|(_, c)| Arc::ptr_eq(c, conn)) {
                return;
            }
            conns.remove(&addr);
            conns.keys().copied().collect()
        };
        log::warn!("client ({handle}) @ {addr} connection closed");
        if self.client_manager.active_addr(handle) == Some(addr) {
            self.client_manager.set_active_addr(handle, None);
        }
        log::info!("active connections: {active:?}");
        // the capture task gives control back if this client had it; it is
        // gone when the service shuts down
        let _ = self.recv_tx.send((handle, ConnEvent::Closed));
        self.changed.notify_waiters();
    }
}

/// pings go out every `PING_INTERVAL`; the connection is closed only once
/// `MAX_MISSED_PINGS` in a row went unanswered (4 s), so loss bursts shorter
/// than that, e.g. 3 of every 4 pings lost, keep it open (lan-mouse#307)
const PING_INTERVAL: Duration = Duration::from_millis(500);
const MAX_MISSED_PINGS: u32 = 8;

/// consecutive ping intervals without a Pong
#[derive(Debug, Default)]
struct MissedPings(u32);

impl MissedPings {
    /// records whether a Pong arrived during the last ping interval;
    /// true once the peer counts as gone
    fn record(&mut self, answered: bool) -> bool {
        self.0 = if answered { 0 } else { self.0 + 1 };
        self.0 >= MAX_MISSED_PINGS
    }
}

/// returns once the peer counts as gone
async fn ping_pong(addr: SocketAddr, conn: &ArcConn, ping_response: &RefCell<HashSet<SocketAddr>>) {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
    let mut missed = MissedPings::default();
    loop {
        if let Err(e) = conn.send(&buf[..len]).await {
            log::warn!("{addr}: send error `{e}`, closing connection");
            break;
        }
        log::trace!("PING >->->->->- {addr}");

        tokio::time::sleep(PING_INTERVAL).await;

        if missed.record(ping_response.borrow_mut().remove(&addr)) {
            log::warn!(
                "{addr} did not respond for {:?}, closing connection",
                PING_INTERVAL * MAX_MISSED_PINGS
            );
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use crate::listen::{ListenEvent, MiMouseListener};
    use futures::StreamExt;
    use std::{net::Ipv4Addr, sync::RwLock, time::Instant};
    use tokio::task::LocalSet;

    /// feeds per-interval Pong results, returns the interval (1-based) at
    /// which the connection would be closed
    fn closed_at(answered: impl IntoIterator<Item = bool>) -> Option<usize> {
        let mut missed = MissedPings::default();
        answered
            .into_iter()
            .position(|a| missed.record(a))
            .map(|i| i + 1)
    }

    #[test]
    fn ping_window_tolerates_loss_bursts() {
        // 3 of every 4 pings lost for a minute
        let lossy = [false, false, false, true].repeat(30);
        assert_eq!(closed_at(lossy), None);
        // 3.5 s of silence, then a Pong
        let burst = [true; 4].into_iter().chain([false; 7]).chain([true]);
        assert_eq!(closed_at(burst.clone().chain(burst)), None);
    }

    #[test]
    fn ping_window_closes_after_4_s_of_silence() {
        let dead = [true; 4].into_iter().chain([false; 20]);
        assert_eq!(closed_at(dead), Some(4 + 8));
        // a Pong resets the count
        let flapping = [false; 7].into_iter().chain([true]).chain([false; 8]);
        assert_eq!(closed_at(flapping), Some(16));
    }

    #[test]
    fn backoff_doubles_to_30_s_and_resets() {
        let mut backoff = Backoff::default();
        let secs: Vec<u64> = (0..8).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
        assert_eq!(backoff.next(), Duration::from_secs(2));
    }

    /// waits for the listener to accept a DTLS connection
    async fn accepted(listener: &mut MiMouseListener) -> SocketAddr {
        let accept = async {
            loop {
                match listener.next().await.expect("listener closed") {
                    ListenEvent::Accept { addr, .. } => return addr,
                    _ => continue,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .expect("client did not connect")
    }

    /// a listener on a free localhost port trusting `client`, and a client
    /// manager with one client pointing at it
    async fn peer(client: &Certificate) -> (MiMouseListener, ClientManager, ClientHandle) {
        let fingerprint = crypto::certificate_fingerprint(client);
        let authorized = Arc::new(RwLock::new(HashMap::from([(fingerprint, "test".into())])));
        let server = Certificate::generate_self_signed(["ignored".to_owned()]).expect("cert");
        let (listener, port) = MiMouseListener::on_free_port(server, authorized).await;
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        client_manager.set_fix_ips(handle, vec![Ipv4Addr::LOCALHOST.into()]);
        client_manager.set_port(handle, port);
        (listener, client_manager, handle)
    }

    #[tokio::test]
    async fn keep_connected_connects_without_input_and_reconnects() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, client_manager, handle) = peer(&cert).await;
                let mut conn = MiMouseConnection::new(cert, client_manager);
                conn.connector().keep_connected(handle);

                // connected before anything was sent
                let first = accepted(&mut listener).await;
                // nobody answers the pings here: the connection is dropped
                // after 4 s, the capture task is told ...
                let start = Instant::now();
                assert_eq!(closed(&mut conn).await, handle);
                let elapsed = start.elapsed();
                assert!(
                    (Duration::from_millis(3500)..Duration::from_millis(5500)).contains(&elapsed),
                    "closed after {elapsed:?}"
                );
                // ... and it is re-established 1 s later
                let second = accepted(&mut listener).await;
                assert_ne!(first, second);

                listener.terminate().await;
            })
            .await;
    }

    #[tokio::test]
    async fn peer_closing_the_connection_is_reported() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, client_manager, handle) = peer(&cert).await;
                let mut conn = MiMouseConnection::new(cert, client_manager);
                conn.connector().keep_connected(handle);
                accepted(&mut listener).await;

                // the peer shuts down (close_notify): reported at once, not
                // only after the ping window
                let start = Instant::now();
                listener.terminate().await;
                assert_eq!(closed(&mut conn).await, handle);
                assert!(
                    start.elapsed() < Duration::from_secs(2),
                    "{:?}",
                    start.elapsed()
                );
            })
            .await;
    }

    /// `stop` closes the connection and reports it, instead of leaving it
    /// open for a handle that may be reused
    #[tokio::test]
    async fn stop_closes_the_connection() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, client_manager, handle) = peer(&cert).await;
                let mut conn = MiMouseConnection::new(cert, client_manager.clone());
                let connector = conn.connector();
                connector.keep_connected(handle);
                accepted(&mut listener).await;
                while !connector.is_connected(handle).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let start = Instant::now();
                connector.stop(handle);
                assert_eq!(closed(&mut conn).await, handle);
                assert!(start.elapsed() < Duration::from_secs(2));
                assert!(!connector.is_connected(handle).await);
                assert_eq!(client_manager.active_addr(handle), None);
                listener.terminate().await;
            })
            .await;
    }

    /// a UDP relay on a free localhost port to `target` that holds the
    /// first datagram, and everything after it, until `release`; `held`
    /// fires once that datagram arrived
    async fn holding_relay(target: u16) -> (u16, tokio::sync::oneshot::Receiver<()>, Rc<Notify>) {
        let front = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let port = front.local_addr().expect("local addr").port();
        let back = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        back.connect(("127.0.0.1", target)).await.expect("connect");
        let (held, held_rx) = tokio::sync::oneshot::channel();
        let release = Rc::new(Notify::new());
        let released = release.clone();
        spawn_local(async move {
            let mut buf = [0u8; 4096];
            let mut back_buf = [0u8; 4096];
            let (n, client) = front.recv_from(&mut buf).await.expect("recv");
            let _ = held.send(());
            released.notified().await;
            let _ = back.send(&buf[..n]).await;
            loop {
                tokio::select! {
                    r = front.recv_from(&mut buf) => match r {
                        Ok((n, _)) => { let _ = back.send(&buf[..n]).await; }
                        Err(_) => return,
                    },
                    r = back.recv(&mut back_buf) => match r {
                        Ok(n) => { let _ = front.send_to(&back_buf[..n], client).await; }
                        Err(_) => return,
                    },
                }
            }
        });
        (port, held_rx, release)
    }

    /// C4: a handshake that completes after `stop` must not register a
    /// connection for the stopped client
    #[tokio::test]
    async fn stop_cancels_an_attempt_in_flight() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut listener, client_manager, handle) = peer(&cert).await;
                let target = client_manager.get_port(handle).unwrap();
                let (port, held, release) = holding_relay(target).await;
                client_manager.set_port(handle, port);
                let conn = MiMouseConnection::new(cert, client_manager.clone());
                let connector = conn.connector();
                connector.keep_connected(handle);
                // the ClientHello is waiting in the relay
                tokio::time::timeout(Duration::from_secs(10), held)
                    .await
                    .expect("no handshake started")
                    .expect("relay gone");
                connector.stop(handle);
                release.notify_one();
                // the server keeps answering, so an attempt that survived
                // `stop` completes its handshake here
                spawn_local(async move { while listener.next().await.is_some() {} });
                tokio::time::timeout(Duration::from_secs(10), async {
                    while connector.connecting.lock().await.contains(&handle) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("attempt still running");
                assert!(connector.conns.lock().await.is_empty());
                assert_eq!(client_manager.active_addr(handle), None);
            })
            .await;
    }

    /// A2: an unpinned client is pinned to the first server it reaches; a
    /// server with another certificate at its address is then refused
    #[tokio::test]
    async fn unpinned_client_is_pinned_on_first_use() {
        LocalSet::new()
            .run_until(async {
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let (mut first, client_manager, handle) = peer(&cert).await;
                let (mut other, other_manager, other_handle) = peer(&cert).await;
                let conn = MiMouseConnection::new(cert, client_manager.clone());
                let connector = conn.connector();
                connector.keep_connected(handle);
                accepted(&mut first).await;
                while !connector.is_connected(handle).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let pin = client_manager.get_state(handle).unwrap().0.fingerprint;
                assert!(pin.is_some(), "no pin recorded");

                // the client now points at a server with another certificate
                connector.stop(handle);
                client_manager.set_port(handle, other_manager.get_port(other_handle).unwrap());
                connector.keep_connected(handle);
                let refused = tokio::time::timeout(Duration::from_secs(3), async {
                    while let Some(event) = other.next().await {
                        assert!(
                            !matches!(event, ListenEvent::Accept { .. }),
                            "server with another certificate accepted"
                        );
                    }
                });
                assert!(refused.await.is_err(), "listener closed");
                assert!(!connector.is_connected(handle).await);
                assert_eq!(client_manager.get_state(handle).unwrap().0.fingerprint, pin);
                connector.stop(handle);
                first.terminate().await;
                other.terminate().await;
            })
            .await;
    }

    /// waits for a [`ConnEvent::Closed`], skipping received events
    async fn closed(conn: &mut MiMouseConnection) -> ClientHandle {
        let closed = async {
            loop {
                if let (handle, ConnEvent::Closed) = conn.recv().await {
                    return handle;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), closed)
            .await
            .expect("connection not reported closed")
    }
}

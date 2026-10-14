use core::sync::atomic::Ordering;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Weak;
use std::time::Duration;

use portable_atomic::AtomicBool;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::{Instant, MissedTickBehavior};

use super::*;
use crate::error::Error;
use crate::Buffer;

const RECEIVE_MTU: usize = 8192;
const DEFAULT_LISTEN_BACKLOG: usize = 128; // same as Linux default
const DEFAULT_MAX_CONNS: usize = 256;
const DEFAULT_CONN_BUFFER_SIZE: usize = 128 * 1024;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub type AcceptFilterFn =
    Box<dyn (Fn(&[u8]) -> Pin<Box<dyn Future<Output = bool> + Send + 'static>>) + Send + Sync>;

type AcceptDoneCh = (mpsc::Receiver<Arc<UdpConn>>, watch::Receiver<()>);

/// conns by remote, with the time of the last packet from it
type ConnMap = Mutex<HashMap<SocketAddr, (Arc<UdpConn>, Instant)>>;

/// listener is used in the [DTLS](https://github.com/webrtc-rs/dtls) and
/// [SCTP](https://github.com/webrtc-rs/sctp) transport to provide a connection-oriented
/// listener over a UDP.
struct ListenerImpl {
    pconn: Arc<dyn Conn + Send + Sync>,
    accepting: Arc<AtomicBool>,
    accept_ch_tx: Arc<Mutex<Option<mpsc::Sender<Arc<UdpConn>>>>>,
    done_ch_tx: Arc<Mutex<Option<watch::Sender<()>>>>,
    ch_rx: Arc<Mutex<AcceptDoneCh>>,
    conns: Arc<ConnMap>,
}

#[async_trait]
impl Listener for ListenerImpl {
    /// accept waits for and returns the next connection to the listener.
    async fn accept(&self) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr)> {
        let (accept_ch_rx, done_ch_rx) = &mut *self.ch_rx.lock().await;

        tokio::select! {
            c = accept_ch_rx.recv() =>{
                if let Some(c) = c{
                    let raddr = c.raddr;
                    Ok((c, raddr))
                }else{
                    Err(Error::ErrClosedListenerAcceptCh)
                }
            }
            _ = done_ch_rx.changed() =>  Err(Error::ErrClosedListener),
        }
    }

    /// close closes the listener.
    /// Any blocked Accept operations will be unblocked and return errors.
    async fn close(&self) -> Result<()> {
        if self.accepting.load(Ordering::SeqCst) {
            self.accepting.store(false, Ordering::SeqCst);
            {
                let mut done_ch = self.done_ch_tx.lock().await;
                done_ch.take();
            }
            {
                let mut accept_ch = self.accept_ch_tx.lock().await;
                accept_ch.take();
            }
        }

        Ok(())
    }

    /// Addr returns the listener's network address.
    async fn addr(&self) -> Result<SocketAddr> {
        self.pconn.local_addr()
    }
}

/// ListenConfig stores options for listening to an address.
#[derive(Default)]
pub struct ListenConfig {
    /// Backlog defines the maximum length of the queue of pending
    /// connections. It is equivalent of the backlog argument of
    /// POSIX listen function.
    /// If a connection request arrives when the queue is full,
    /// the request will be silently discarded, unlike TCP.
    /// Set zero to use default value 128 which is same as Linux default.
    pub backlog: usize,

    /// AcceptFilter determines whether the new conn should be made for
    /// the incoming packet. If not set, any packet creates new conn.
    pub accept_filter: Option<AcceptFilterFn>,

    /// MaxConns caps the number of remotes with a conn; packets from
    /// further remotes are discarded until a conn is closed or idles out.
    /// Set zero to use default value 256.
    pub max_conns: usize,

    /// ConnBufferSize caps the bytes buffered per conn; packets that do not
    /// fit are discarded. Set zero to use default value 128 KiB.
    pub conn_buffer_size: usize,

    /// IdleTimeout closes and forgets a conn whose remote sent nothing for
    /// this long. Set zero to use default value 30 s.
    pub idle_timeout: Duration,
}

#[derive(Clone, Copy)]
struct Limits {
    max_conns: usize,
    conn_buffer_size: usize,
    idle_timeout: Duration,
}

pub async fn listen<A: ToSocketAddrs>(laddr: A) -> Result<impl Listener> {
    ListenConfig::default().listen(laddr).await
}

impl ListenConfig {
    /// Listen creates a new listener based on the ListenConfig.
    pub async fn listen<A: ToSocketAddrs>(&mut self, laddr: A) -> Result<impl Listener> {
        if self.backlog == 0 {
            self.backlog = DEFAULT_LISTEN_BACKLOG;
        }
        let limits = Limits {
            max_conns: match self.max_conns {
                0 => DEFAULT_MAX_CONNS,
                n => n,
            },
            conn_buffer_size: match self.conn_buffer_size {
                0 => DEFAULT_CONN_BUFFER_SIZE,
                n => n,
            },
            idle_timeout: match self.idle_timeout {
                Duration::ZERO => DEFAULT_IDLE_TIMEOUT,
                t => t,
            },
        };

        let pconn = Arc::new(UdpSocket::bind(laddr).await?);
        let (accept_ch_tx, accept_ch_rx) = mpsc::channel(self.backlog);
        let (done_ch_tx, done_ch_rx) = watch::channel(());

        let l = ListenerImpl {
            pconn,
            accepting: Arc::new(AtomicBool::new(true)),
            accept_ch_tx: Arc::new(Mutex::new(Some(accept_ch_tx))),
            done_ch_tx: Arc::new(Mutex::new(Some(done_ch_tx))),
            ch_rx: Arc::new(Mutex::new((accept_ch_rx, done_ch_rx.clone()))),
            conns: Arc::new(Mutex::new(HashMap::new())),
        };

        let pconn = Arc::clone(&l.pconn);
        let accepting = Arc::clone(&l.accepting);
        let accept_filter = self.accept_filter.take();
        let accept_ch_tx = Arc::clone(&l.accept_ch_tx);
        let conns = Arc::clone(&l.conns);
        tokio::spawn(async move {
            ListenConfig::read_loop(
                done_ch_rx,
                pconn,
                accepting,
                accept_filter,
                accept_ch_tx,
                conns,
                limits,
            )
            .await;
        });

        Ok(l)
    }

    /// read_loop has to tasks:
    /// 1. Dispatching incoming packets to the correct Conn.
    ///    It can therefore not be ended until all Conns are closed.
    /// 2. Creating a new Conn when receiving from a new remote.
    /// 3. Closing Conns whose remote idled out.
    async fn read_loop(
        mut done_ch_rx: watch::Receiver<()>,
        pconn: Arc<dyn Conn + Send + Sync>,
        accepting: Arc<AtomicBool>,
        accept_filter: Option<AcceptFilterFn>,
        accept_ch_tx: Arc<Mutex<Option<mpsc::Sender<Arc<UdpConn>>>>>,
        conns: Arc<ConnMap>,
        limits: Limits,
    ) {
        let mut buf = vec![0u8; RECEIVE_MTU];
        let mut idle_check =
            tokio::time::interval((limits.idle_timeout / 4).max(Duration::from_millis(1)));
        idle_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = done_ch_rx.changed() => {
                    break;
                }
                _ = idle_check.tick() => {
                    ListenConfig::close_idle(&conns, limits.idle_timeout).await;
                }
                result = pconn.recv_from(&mut buf) => {
                    match result {
                        Ok((n, raddr)) => {
                            let udp_conn = match ListenConfig::get_udp_conn(
                                &pconn,
                                &accepting,
                                &accept_filter,
                                &accept_ch_tx,
                                &conns,
                                limits,
                                raddr,
                                &buf[..n],
                            )
                            .await
                            {
                                Ok(conn) => conn,
                                Err(_) => continue,
                            };

                            if let Some(conn) = udp_conn {
                                let _ = conn.buffer.write(&buf[..n]).await;
                            }
                        }
                        Err(err) => {
                            // On Windows, `recv_from` fails with `ErrorKind::ConnectionReset`
                            // (WSAECONNRESET) when a previously *sent* datagram was answered
                            // with an ICMP "Port Unreachable" message (e.g. because the remote
                            // closed its socket). This does not affect the listening socket
                            // itself, so it must not tear down the read loop. Backport of
                            // webrtc-rs/webrtc#812 (lan-mouse issue #262).
                            if matches!(&err, Error::Io(io_err) if io_err.0.kind() == std::io::ErrorKind::ConnectionReset) {
                                log::debug!("ListenConfig pconn.recv_from transient error: {}", err);
                                continue;
                            }
                            log::warn!("ListenConfig pconn.recv_from error: {}", err);
                            break;
                        }
                    };
                }
            }
        }
    }

    async fn close_idle(conns: &ConnMap, idle_timeout: Duration) {
        let now = Instant::now();
        let mut idle = vec![];
        conns.lock().await.retain(|_, (conn, last_recv)| {
            let active = now.duration_since(*last_recv) < idle_timeout;
            if !active {
                idle.push(Arc::clone(conn));
            }
            active
        });
        for conn in idle {
            conn.buffer.close().await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn get_udp_conn(
        pconn: &Arc<dyn Conn + Send + Sync>,
        accepting: &Arc<AtomicBool>,
        accept_filter: &Option<AcceptFilterFn>,
        accept_ch_tx: &Arc<Mutex<Option<mpsc::Sender<Arc<UdpConn>>>>>,
        conns: &Arc<ConnMap>,
        limits: Limits,
        raddr: SocketAddr,
        buf: &[u8],
    ) -> Result<Option<Arc<UdpConn>>> {
        {
            let mut m = conns.lock().await;
            if let Some((conn, last_recv)) = m.get_mut(&raddr) {
                *last_recv = Instant::now();
                return Ok(Some(conn.clone()));
            }
            // only this loop inserts, so the map cannot grow before the insert below
            if m.len() >= limits.max_conns {
                log::debug!("ListenConfig: {raddr} discarded, {} conns", m.len());
                return Ok(None);
            }
        }

        if !accepting.load(Ordering::SeqCst) {
            return Err(Error::ErrClosedListener);
        }

        if let Some(f) = accept_filter {
            if !(f(buf).await) {
                return Ok(None);
            }
        }

        let udp_conn = Arc::new(UdpConn::new(
            Arc::clone(pconn),
            raddr,
            Arc::downgrade(conns),
            limits.conn_buffer_size,
        ));
        {
            let accept_ch = accept_ch_tx.lock().await;
            if let Some(tx) = &*accept_ch {
                if tx.try_send(Arc::clone(&udp_conn)).is_err() {
                    return Err(Error::ErrListenQueueExceeded);
                }
            } else {
                return Err(Error::ErrClosedListenerAcceptCh);
            }
        }

        {
            let mut m = conns.lock().await;
            m.insert(raddr, (Arc::clone(&udp_conn), Instant::now()));
        }

        Ok(Some(udp_conn))
    }
}

/// UdpConn augments a connection-oriented connection over a UdpSocket
pub struct UdpConn {
    pconn: Arc<dyn Conn + Send + Sync>,
    raddr: SocketAddr,
    buffer: Buffer,
    conns: Weak<ConnMap>,
}

impl UdpConn {
    fn new(
        pconn: Arc<dyn Conn + Send + Sync>,
        raddr: SocketAddr,
        conns: Weak<ConnMap>,
        buffer_size: usize,
    ) -> Self {
        UdpConn {
            pconn,
            raddr,
            buffer: Buffer::new(0, buffer_size),
            conns,
        }
    }
}

#[async_trait]
impl Conn for UdpConn {
    async fn connect(&self, addr: SocketAddr) -> Result<()> {
        self.pconn.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.buffer.read(buf, None).await?)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let n = self.buffer.read(buf, None).await?;
        Ok((n, self.raddr))
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.pconn.send_to(buf, self.raddr).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        self.pconn.send_to(buf, target).await
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.pconn.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.raddr)
    }

    /// close forgets the conn, so the next packet from its remote creates a
    /// new one, and fails pending and later reads.
    async fn close(&self) -> Result<()> {
        if let Some(conns) = self.conns.upgrade() {
            let mut conns = conns.lock().await;
            // a newer conn from the same remote keeps its entry
            if conns
                .get(&self.raddr)
                .is_some_and(|(conn, _)| std::ptr::eq(Arc::as_ptr(conn), self))
            {
                conns.remove(&self.raddr);
            }
        }
        self.buffer.close().await;
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

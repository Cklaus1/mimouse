use std::{collections::HashMap, sync::Arc, time::Duration};
#[cfg(unix)]
use std::{io::ErrorKind, path::PathBuf};

use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc::{self, error::TrySendError},
    task::{AbortHandle, JoinHandle, spawn_local},
};

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;

use super::{FrontendEvent, IpcError, IpcListenerCreationError, Peer, Request};

#[cfg(unix)]
type IpcStream = UnixStream;
#[cfg(windows)]
type IpcStream = NamedPipeServer;

/// identifies one frontend connection
pub type ConnId = u64;

/// events queued per connection; a frontend that falls this far behind is
/// disconnected (it reconnects and gets the current state again)
const QUEUE_LEN: usize = 64;

pub enum IpcEvent {
    /// a frontend connected
    Connected(ConnId),
    /// a frontend sent a request
    Request(ConnId, Request),
}

enum Internal {
    Accepted(ConnId, Conn),
    Line(ConnId, Request),
    /// an undecodable request, with its `seq` if that much could be read
    Invalid(ConnId, Option<u64>, IpcError),
    Closed(ConnId),
}

/// a frontend connection; dropping it closes the connection
struct Conn {
    peer: Peer,
    queue: mpsc::Sender<Arc<str>>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

pub struct AsyncFrontendListener {
    rx: mpsc::UnboundedReceiver<Internal>,
    conns: HashMap<ConnId, Conn>,
    accept: JoinHandle<()>,
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        let (tx, rx) = mpsc::unbounded_channel();

        #[cfg(unix)]
        let (socket_path, accept) = {
            let socket_path = super::default_socket_path()?;

            log::debug!("remove socket: {socket_path:?}");
            if socket_path.exists() {
                // try to connect to see if some other instance
                // of mimouse is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> mimouse is already running
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    // mimouse is not running but a socket was left behind
                    Err(e) => {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other mimouse instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            (socket_path, spawn_local(accept_unix(listener, tx)))
        };

        #[cfg(windows)]
        let accept = {
            let pipe =
                super::windows::PipeSecurity::new().map_err(IpcListenerCreationError::Bind)?;
            let server = match pipe.create(true) {
                Ok(server) => server,
                // the pipe exists: another instance (or another account
                // squatting the name, which clients refuse)
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            spawn_local(accept_windows(server, pipe, tx))
        };

        Ok(Self {
            rx,
            conns: HashMap::new(),
            accept,
            #[cfg(unix)]
            socket_path,
        })
    }

    /// next connection or request (cancel safe)
    pub async fn next(&mut self) -> IpcEvent {
        loop {
            let Some(event) = self.rx.recv().await else {
                log::error!("control socket closed, no further requests");
                return std::future::pending().await;
            };
            match event {
                Internal::Accepted(id, conn) => {
                    log::debug!("frontend {id} connected: {:?}", conn.peer);
                    self.conns.insert(id, conn);
                    return IpcEvent::Connected(id);
                }
                Internal::Line(id, request) => return IpcEvent::Request(id, request),
                Internal::Invalid(id, seq, e) => {
                    log::warn!("invalid request from frontend {id}: {e}");
                    match seq {
                        Some(seq) => self.enqueue(
                            Some(id),
                            &FrontendEvent::Reply {
                                seq,
                                result: Err(format!("invalid request: {e}")),
                            },
                        ),
                        // nothing to answer to: close, so the client does not wait
                        None => {
                            self.conns.remove(&id);
                        }
                    }
                }
                Internal::Closed(id) => {
                    self.conns.remove(&id);
                }
            }
        }
    }

    /// the process on the other end of connection `id`
    pub fn peer(&self, id: ConnId) -> Option<&Peer> {
        self.conns.get(&id).map(|c| &c.peer)
    }

    /// send `event` to connection `to`, or to all connections
    pub async fn send(&mut self, to: Option<ConnId>, event: &FrontendEvent) {
        self.enqueue(to, event);
    }

    /// queue `event` without waiting for any frontend; a connection whose
    /// queue is full or whose writer has failed is dropped
    fn enqueue(&mut self, to: Option<ConnId>, event: &FrontendEvent) {
        let mut json = serde_json::to_string(event).expect("serializable event");
        json.push('\n');
        let line: Arc<str> = json.into();
        self.conns.retain(|id, conn| {
            if to.is_some_and(|to| to != *id) {
                return true;
            }
            match conn.queue.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    log::warn!("frontend {id} is not reading its events, disconnecting");
                    false
                }
                Err(TrySendError::Closed(_)) => false,
            }
        });
    }
}

impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        self.accept.abort();
        #[cfg(unix)]
        {
            log::debug!("remove socket: {:?}", self.socket_path);
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// delay after `failures` consecutive accept errors; at the open file limit
/// accept fails instantly, and retrying at once would spin
fn accept_backoff(failures: u32) -> Duration {
    Duration::from_millis(10)
        .saturating_mul(1 << failures.min(7))
        .min(Duration::from_secs(1))
}

#[cfg(unix)]
async fn accept_unix(listener: UnixListener, tx: mpsc::UnboundedSender<Internal>) {
    let mut failures = 0;
    for id in 0.. {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                log::warn!("control socket: accept failed: {e}");
                tokio::time::sleep(accept_backoff(failures)).await;
                failures = failures.saturating_add(1);
                continue;
            }
        };
        failures = 0;
        let peer = unix_peer(&stream);
        if !register(id, stream, peer, &tx) {
            return;
        }
    }
}

#[cfg(unix)]
fn unix_peer(stream: &UnixStream) -> Peer {
    let cred = stream.peer_cred().ok();
    let pid = cred
        .and_then(|c| c.pid())
        .and_then(|p| u32::try_from(p).ok());
    Peer {
        pid,
        uid: cred.map(|c| c.uid()),
        // Linux; elsewhere there is no /proc and `exe` stays empty
        exe: pid.and_then(|pid| {
            std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .map(|p| p.display().to_string())
        }),
    }
}

#[cfg(windows)]
async fn accept_windows(
    mut server: NamedPipeServer,
    pipe: super::windows::PipeSecurity,
    tx: mpsc::UnboundedSender<Internal>,
) {
    let mut failures = 0;
    for id in 0.. {
        let connected = server.connect().await;
        // keep an instance waiting before handing this one off
        let next = match pipe.create(false) {
            Ok(next) => next,
            Err(e) => return log::error!("control pipe: could not create instance: {e}"),
        };
        let stream = std::mem::replace(&mut server, next);
        if let Err(e) = connected {
            log::warn!("control pipe: connect failed: {e}");
            tokio::time::sleep(accept_backoff(failures)).await;
            failures = failures.saturating_add(1);
            continue;
        }
        failures = 0;
        let peer = pipe.client_peer(&stream);
        if !register(id, stream, peer, &tx) {
            return;
        }
    }
}

/// the `seq` of an undecodable request line, if it has one
fn request_seq(line: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Seq {
        seq: u64,
    }
    serde_json::from_str::<Seq>(line).ok().map(|s| s.seq)
}

/// spawn a reader for requests and a writer draining the connection's queue,
/// and hand the connection to the listener; false if the listener is gone
fn register(
    id: ConnId,
    stream: IpcStream,
    peer: Peer,
    tx: &mpsc::UnboundedSender<Internal>,
) -> bool {
    let (rx_half, mut tx_half) = tokio::io::split(stream);
    let (queue, mut queued) = mpsc::channel::<Arc<str>>(QUEUE_LEN);
    let writer = spawn_local(async move {
        while let Some(line) = queued.recv().await {
            if let Err(e) = tx_half.write_all(line.as_bytes()).await {
                log::debug!("frontend {id}: {e}");
                break;
            }
        }
    });
    let reader_tx = tx.clone();
    let reader = spawn_local(async move {
        let tx = reader_tx;
        let mut lines = BufReader::new(rx_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let event = match serde_json::from_str(&line) {
                        Ok(request) => Internal::Line(id, request),
                        Err(e) => Internal::Invalid(id, request_seq(&line), e.into()),
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::debug!("frontend {id}: {e}");
                    break;
                }
            }
        }
        let _ = tx.send(Internal::Closed(id));
    });
    let conn = Conn {
        peer,
        queue,
        reader: reader.abort_handle(),
        writer: writer.abort_handle(),
    };
    tx.send(Internal::Accepted(id, conn)).is_ok()
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::UnixStream,
        task::LocalSet,
        time::timeout,
    };

    use super::*;

    /// a listener without a socket, fed by `register`
    fn listener() -> (AsyncFrontendListener, mpsc::UnboundedSender<Internal>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let listener = AsyncFrontendListener {
            rx,
            conns: HashMap::new(),
            accept: spawn_local(std::future::pending()),
            socket_path: PathBuf::from("/nonexistent/mimouse-test.sock"),
        };
        (listener, tx)
    }

    async fn connect(
        listener: &mut AsyncFrontendListener,
        tx: &mpsc::UnboundedSender<Internal>,
        id: ConnId,
    ) -> UnixStream {
        let (server, client) = UnixStream::pair().unwrap();
        assert!(register(id, server, Peer::default(), tx));
        assert!(matches!(listener.next().await, IpcEvent::Connected(i) if i == id));
        client
    }

    async fn next_event(lines: &mut tokio::io::Lines<BufReader<UnixStream>>) -> FrontendEvent {
        let line = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("no event")
            .unwrap()
            .expect("connection closed");
        serde_json::from_str(&line).unwrap()
    }

    fn reply(seq: u64) -> FrontendEvent {
        FrontendEvent::Reply {
            seq,
            result: Err("x".repeat(64 * 1024)),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_reader_does_not_block_others() {
        LocalSet::new()
            .run_until(async {
                let (mut listener, tx) = listener();
                let mut stalled = connect(&mut listener, &tx, 0).await;
                let reader = connect(&mut listener, &tx, 1).await;
                // far more than the socket buffer and the queue hold
                let sending = async {
                    for seq in 0..1000 {
                        listener.send(Some(0), &reply(seq)).await;
                    }
                    listener.send(Some(1), &reply(7)).await;
                };
                timeout(Duration::from_secs(5), sending)
                    .await
                    .expect("send blocked on a stalled reader");
                let mut lines = BufReader::new(reader).lines();
                assert!(matches!(
                    next_event(&mut lines).await,
                    FrontendEvent::Reply { seq: 7, .. }
                ));
                // the overflowing connection was dropped: its client sees EOF
                assert!(listener.peer(0).is_none());
                let drained = timeout(
                    Duration::from_secs(5),
                    tokio::io::copy(&mut stalled, &mut tokio::io::sink()),
                )
                .await
                .expect("stalled connection not closed");
                assert!(drained.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn undecodable_request_gets_error_reply_or_close() {
        use tokio::io::AsyncWriteExt;
        LocalSet::new()
            .run_until(async {
                let (mut listener, tx) = listener();
                let (rx, mut wx) = connect(&mut listener, &tx, 0).await.into_split();
                let mut lines = BufReader::new(rx).lines();
                // e.g. a request type only a newer service knows
                wx.write_all(b"{\"seq\":7,\"request\":{\"from_the_future\":1}}\n")
                    .await
                    .unwrap();
                let event = tokio::select! {
                    _ = listener.next() => panic!("invalid request passed on"),
                    event = timeout(Duration::from_secs(5), lines.next_line()) => event,
                };
                let event: FrontendEvent =
                    serde_json::from_str(&event.expect("no reply").unwrap().unwrap()).unwrap();
                assert!(matches!(
                    event,
                    FrontendEvent::Reply {
                        seq: 7,
                        result: Err(_)
                    }
                ));
                // no seq to answer: the connection closes
                wx.write_all(b"garbage\n").await.unwrap();
                let line = tokio::select! {
                    _ = listener.next() => panic!("invalid request passed on"),
                    line = timeout(Duration::from_secs(5), lines.next_line()) => line,
                };
                assert!(line.expect("not closed").unwrap().is_none());
                assert!(listener.peer(0).is_none());
            })
            .await;
    }

    #[test]
    fn accept_errors_back_off() {
        assert_eq!(accept_backoff(0), Duration::from_millis(10));
        assert_eq!(accept_backoff(1), Duration::from_millis(20));
        assert_eq!(accept_backoff(7), Duration::from_secs(1));
        assert_eq!(accept_backoff(u32::MAX), Duration::from_secs(1));
    }
}

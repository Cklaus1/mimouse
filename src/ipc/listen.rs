use std::collections::HashMap;
#[cfg(unix)]
use std::{io::ErrorKind, path::PathBuf};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf},
    sync::mpsc,
    task::{JoinHandle, spawn_local},
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

pub enum IpcEvent {
    /// a frontend connected
    Connected(ConnId),
    /// a frontend sent a request
    Request(ConnId, Request),
}

enum Internal {
    Accepted(ConnId, Peer, WriteHalf<IpcStream>),
    Line(ConnId, Result<Request, IpcError>),
    Closed(ConnId),
}

struct Conn {
    peer: Peer,
    tx: WriteHalf<IpcStream>,
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
                Internal::Accepted(id, peer, tx) => {
                    log::debug!("frontend {id} connected: {peer:?}");
                    self.conns.insert(id, Conn { peer, tx });
                    return IpcEvent::Connected(id);
                }
                Internal::Line(id, Ok(request)) => return IpcEvent::Request(id, request),
                Internal::Line(id, Err(e)) => log::warn!("invalid request from frontend {id}: {e}"),
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
        let mut json = serde_json::to_string(event).expect("serializable event");
        json.push('\n');
        let mut failed = vec![];
        for (id, conn) in self.conns.iter_mut() {
            if to.is_some_and(|to| to != *id) {
                continue;
            }
            if conn.tx.write_all(json.as_bytes()).await.is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.conns.remove(&id);
        }
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

#[cfg(unix)]
async fn accept_unix(listener: UnixListener, tx: mpsc::UnboundedSender<Internal>) {
    for id in 0.. {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                log::warn!("control socket: accept failed: {e}");
                continue;
            }
        };
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
            continue;
        }
        let peer = pipe.client_peer(&stream);
        if !register(id, stream, peer, &tx) {
            return;
        }
    }
}

/// hand the write half to the listener and spawn a reader for requests;
/// false if the listener is gone
fn register(
    id: ConnId,
    stream: IpcStream,
    peer: Peer,
    tx: &mpsc::UnboundedSender<Internal>,
) -> bool {
    let (rx_half, tx_half) = tokio::io::split(stream);
    if tx.send(Internal::Accepted(id, peer, tx_half)).is_err() {
        return false;
    }
    let tx = tx.clone();
    spawn_local(async move {
        let mut lines = BufReader::new(rx_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let request = serde_json::from_str(&line).map_err(IpcError::from);
                    if tx.send(Internal::Line(id, request)).is_err() {
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
    true
}

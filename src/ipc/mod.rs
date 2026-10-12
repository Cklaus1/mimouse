use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

mod connect_async;
mod listen;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::current_user_sid;

pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::{AsyncFrontendListener, ConnId, IpcEvent};

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
    /// the control pipe exists but another account created it
    #[cfg(windows)]
    #[error("control pipe is owned by {0}, not the current user; refusing to connect")]
    NotOwner(String),
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind mimouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    #[serde(rename = "position")]
    pub pos: Position,
    /// enter hook
    #[serde(rename = "enter_hook")]
    pub cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
}

/// a client as reported to frontends: `{"id": .., <config>, <state>}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: ClientHandle,
    #[serde(flatten)]
    pub config: ClientConfig,
    #[serde(flatten)]
    pub state: ClientState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDirection {
    Sent,
    Received,
}

/// Events from the service, one JSON object per line, tagged by `event`.
/// `Reply` goes to the requesting connection only; a new connection first
/// receives `clients`, `capture`, `emulation`, `port`, `fingerprint` and
/// `authorized_keys`; everything else is broadcast. This is also the
/// `mimouse cli watch --json` schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FrontendEvent {
    /// result of the request with sequence number `seq`
    Reply {
        seq: u64,
        result: Result<Reply, String>,
    },
    /// all configured clients
    Clients { clients: Vec<ClientInfo> },
    /// a client was added
    Created { client: ClientInfo },
    /// config or state of a client changed
    State { client: ClientInfo },
    /// a client was removed
    Deleted { id: ClientHandle },
    /// listen port, with the reason if changing it failed
    Port { port: u16, error: Option<String> },
    /// input capture enabled / disabled
    Capture { enabled: bool },
    /// input emulation enabled / disabled
    Emulation { enabled: bool },
    /// authorized certificate fingerprints (fingerprint -> description)
    AuthorizedKeys { keys: HashMap<String, String> },
    /// certificate fingerprint of this machine
    Fingerprint { fingerprint: String },
    /// an authorized peer connected to this machine
    Connection {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// the cursor of a connected peer entered this machine
    Entered {
        addr: SocketAddr,
        fingerprint: String,
        position: Position,
    },
    /// a connected peer went away
    Disconnected { addr: SocketAddr },
    /// a peer with an unauthorized fingerprint tried to connect
    ConnectionAttempt { fingerprint: String },
    /// clipboard sent to or received from a peer; contents are never included
    Clipboard {
        direction: ClipboardDirection,
        peer: IpAddr,
        mime: Vec<String>,
        size: usize,
    },
}

/// successful result of a request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Done,
    Client(ClientInfo),
    Clients(Vec<ClientInfo>),
}

/// a request on the wire: `{"seq": n, "request": {...}}`, answered by a
/// `Reply` with the same `seq`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub seq: u64,
    pub request: FrontendRequest,
}

/// Requests from a frontend (the CLI). Serialized with the `action` tag,
/// which is also the action name in the audit log.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FrontendRequest {
    AddClient {
        hostname: Option<String>,
        port: u16,
        ips: Vec<IpAddr>,
        enter_hook: Option<String>,
    },
    RemoveClient {
        id: ClientHandle,
    },
    Activate {
        id: ClientHandle,
    },
    Deactivate {
        id: ClientHandle,
    },
    List,
    SetHost {
        id: ClientHandle,
        hostname: Option<String>,
    },
    SetPort {
        id: ClientHandle,
        port: u16,
    },
    SetPosition {
        id: ClientHandle,
        position: Position,
    },
    SetIps {
        id: ClientHandle,
        ips: Vec<IpAddr>,
    },
    SetEnterHook {
        id: ClientHandle,
        enter_hook: Option<String>,
    },
    /// change the listen port of this machine
    ChangePort {
        port: u16,
    },
    EnableCapture,
    EnableEmulation,
    AuthorizeKey {
        description: String,
        fingerprint: String,
    },
    RemoveAuthorizedKey {
        fingerprint: String,
    },
    SaveConfig,
}

impl FrontendRequest {
    /// read-only requests are not audited
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::List)
    }

    /// the client this request refers to, which must exist
    pub fn client(&self) -> Option<ClientHandle> {
        match self {
            Self::RemoveClient { id }
            | Self::Activate { id }
            | Self::Deactivate { id }
            | Self::SetHost { id, .. }
            | Self::SetPort { id, .. }
            | Self::SetPosition { id, .. }
            | Self::SetIps { id, .. }
            | Self::SetEnterHook { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// user id of a local process: uid on Unix, SID on Windows
#[cfg(unix)]
pub type Uid = u32;
#[cfg(windows)]
pub type Uid = String;

/// the local process on the other end of a control connection
#[derive(Debug, Clone, Default, Serialize)]
pub struct Peer {
    pub pid: Option<u32>,
    pub uid: Option<Uid>,
    pub exe: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const MIMOUSE_SOCKET_NAME: &str = "mimouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(MIMOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(MIMOUSE_SOCKET_NAME))
}

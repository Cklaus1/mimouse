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

use input_event::scancode;
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
    /// the part of the edge that switches to this client, in percent of its
    /// length (`None`: all of it)
    #[serde(default)]
    pub range: Option<[u8; 2]>,
    /// chord that switches control to this client
    #[serde(default)]
    pub hotkey: Option<Vec<scancode::Linux>>,
    /// sha256 fingerprint of the client's certificate: outgoing connections
    /// accept only this one (set by pairing, or on the first connection)
    #[serde(default)]
    pub fingerprint: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            range: None,
            hotkey: None,
            fingerprint: None,
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
/// receives `clients`, `emulation`, `capture`, `clipboard_status`, `port`,
/// `fingerprint` and `authorized_keys`; everything else is broadcast. This
/// is also the `mimouse cli watch --json` schema.
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
    /// input capture state; `enabled` is `status == "enabled"`; `backend`:
    /// the one in use while enabled (absent from older services)
    Capture {
        enabled: bool,
        status: Status,
        #[serde(default)]
        backend: Option<BackendInUse>,
    },
    /// input emulation state; `enabled` is `status == "enabled"`; `backend`:
    /// the one in use while enabled (absent from older services)
    Emulation {
        enabled: bool,
        status: Status,
        #[serde(default)]
        backend: Option<BackendInUse>,
    },
    /// clipboard sharing state: `{"status": .., "reason": ..}`
    ClipboardStatus(ClipboardStatus),
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
    /// a pairing reached the code comparison: compare `code` with the one
    /// the other machine shows, then confirm or cancel
    PairCode {
        code: String,
        /// host name the peer reported, or its address
        peer: String,
        fingerprint: String,
        /// where the peer will be placed relative to this machine
        position: Position,
    },
    /// both sides confirmed: the peer is trusted and configured as client
    /// `id`; `not_saved`: why the config could not be saved (pairing stays
    /// in effect until the service stops)
    Paired {
        id: ClientHandle,
        peer: String,
        fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_saved: Option<String>,
    },
    /// the pairing ended without trusting anyone
    PairFailed { reason: String },
}

/// successful result of a request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Done,
    Client(Box<ClientInfo>),
    Clients(Vec<ClientInfo>),
    /// the config in effect (`config`)
    Config(ConfigReport),
    /// telemetry state (`telemetry on|off|status`)
    Telemetry(TelemetryStatus),
    /// the next upload (`telemetry show`)
    TelemetryShow(TelemetryShow),
}

/// telemetry as the service runs it
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryStatus {
    /// `telemetry = true` in the config
    pub enabled: bool,
    /// random id of this installation; `None` while off
    #[serde(default)]
    pub install_id: Option<String>,
    /// base URL uploads go to
    #[serde(default)]
    pub endpoint: Option<String>,
    /// why there is no usable endpoint
    #[serde(default)]
    pub endpoint_error: Option<String>,
    /// why telemetry is enabled but not running
    #[serde(default)]
    pub error: Option<String>,
    /// completed days waiting for the next upload
    #[serde(default)]
    pub queued_days: usize,
    #[serde(default)]
    pub queued_crashes: usize,
    /// UTC date the current day's counts started
    #[serde(default)]
    pub counting_since: Option<String>,
    /// UTC date of the last upload attempt
    #[serde(default)]
    pub last_attempt: Option<String>,
    /// why the last upload failed
    #[serde(default)]
    pub last_error: Option<String>,
    /// crashes recorded while telemetry was off: never uploaded
    #[serde(default)]
    pub local_crashes: Vec<TelemetryCrash>,
}

/// the next telemetry upload
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryShow {
    pub enabled: bool,
    /// the exact body of the next upload; `None`: nothing to send
    #[serde(default)]
    pub batch: Option<String>,
    /// UTC date the current day's counts started (sent once it is over)
    #[serde(default)]
    pub counting_since: Option<String>,
    /// crashes recorded while telemetry was off: never uploaded
    #[serde(default)]
    pub local_crashes: Vec<TelemetryCrash>,
}

/// a crash record, as shown
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryCrash {
    /// `src/file.rs:line`
    pub location: String,
    pub message: String,
}

/// the capture or emulation backend in use: `{"name": .., "auto": ..}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInUse {
    pub name: String,
    /// picked from the backends that work here, since none is configured
    pub auto: bool,
}

impl Display for BackendInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let selection = if self.auto { "auto" } else { "configured" };
        write!(f, "{}, {selection}", self.name)
    }
}

/// where the value in effect of a config key comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// a command line flag of the service
    Flag,
    /// the config file
    File,
    /// the built-in default
    Default,
    /// `mimouse cli change-port`
    Cli,
}

impl Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Flag => "flag",
            Source::File => "file",
            Source::Default => "default",
            Source::Cli => "cli",
        })
    }
}

/// a config key in effect
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigValue {
    pub key: String,
    /// `null`: unset (e.g. no backend configured)
    pub value: serde_json::Value,
    pub source: Source,
    /// a changed value takes effect only when the service restarts
    pub restart: bool,
}

/// a restart-only key whose value in the file differs from the one in effect
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartPending {
    pub key: String,
    pub in_effect: serde_json::Value,
    pub on_disk: serde_json::Value,
}

/// the config of the running service
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigReport {
    /// the config file
    pub path: std::path::PathBuf,
    pub keys: Vec<ConfigValue>,
    /// keys changed in the file that need a restart to take effect
    pub awaiting_restart: Vec<RestartPending>,
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
    /// move control to a client, as if the cursor crossed into it
    Focus {
        id: ClientHandle,
    },
    /// pair with the machine at `host` (running `pair --accept`), placing it
    /// at `position`; `ips` are tried before resolving `host`
    Pair {
        host: String,
        ips: Vec<IpAddr>,
        port: u16,
        position: Position,
    },
    /// accept one pairing request during the next `timeout` seconds
    PairAccept {
        timeout: u64,
    },
    /// the user compared the pairing code and it matches
    PairConfirm {
        code: String,
    },
    /// abort the current pairing
    PairCancel,
    /// the config in effect, with the source of each key
    Config,
    /// turn telemetry on (saved to the config)
    TelemetryOn,
    /// turn telemetry off, deleting the install id and the queue
    TelemetryOff,
    TelemetryStatus,
    /// the next upload
    TelemetryShow,
    /// `command` (`doctor`, `discover`) ran without the service; only
    /// counted for telemetry
    CountCommand {
        command: String,
    },
}

impl FrontendRequest {
    /// read-only requests are not audited
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::List
                | Self::Config
                | Self::TelemetryStatus
                | Self::TelemetryShow
                | Self::CountCommand { .. }
        )
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
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
    /// a permission request (e.g. the portal dialog) waits for the user
    AwaitingApproval,
}

impl From<Status> for bool {
    /// whether it is enabled
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled | Status::AwaitingApproval => false,
        }
    }
}

impl Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Disabled => write!(f, "disabled"),
            Status::Enabled => write!(f, "enabled"),
            Status::AwaitingApproval => write!(f, "awaiting approval"),
        }
    }
}

/// state of clipboard sharing
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardStatus {
    pub status: ClipboardState,
    /// why sharing is disabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ClipboardStatus {
    pub fn new(status: ClipboardState) -> Self {
        Self {
            status,
            reason: None,
        }
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            status: ClipboardState::Disabled,
            reason: Some(reason.into()),
        }
    }
}

impl Display for ClipboardStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            ClipboardState::Off => write!(f, "off (`clipboard = false` in the config)")?,
            ClipboardState::Starting => write!(f, "starting")?,
            ClipboardState::AwaitingApproval => write!(f, "awaiting approval")?,
            ClipboardState::Enabled => write!(f, "enabled")?,
            ClipboardState::Disabled => write!(f, "disabled")?,
        }
        match &self.reason {
            Some(reason) => write!(f, ": {reason}"),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardState {
    /// turned off in the config
    #[default]
    Off,
    /// the clipboard backend has not reported yet
    Starting,
    /// the clipboard portal dialog waits for the user; received updates
    /// are kept until it is answered
    AwaitingApproval,
    Enabled,
    /// the backend failed (retrying if `reason` says so), was denied, or
    /// the platform has none; received updates are refused once it ended
    Disabled,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// T5: the backend in use decodes; an older service's event without it
    /// still does
    #[test]
    fn state_events_decode_with_and_without_backend() {
        let new = r#"{"event":"capture","enabled":true,"status":"enabled","backend":{"name":"input-capture-portal","auto":true}}"#;
        let libei = BackendInUse {
            name: "input-capture-portal".into(),
            auto: true,
        };
        match serde_json::from_str(new).unwrap() {
            FrontendEvent::Capture {
                status, backend, ..
            } => assert_eq!((status, backend), (Status::Enabled, Some(libei))),
            e => panic!("decoded as {e:?}"),
        }
        let old = r#"{"event":"emulation","enabled":true,"status":"enabled"}"#;
        match serde_json::from_str(old).unwrap() {
            FrontendEvent::Emulation {
                status, backend, ..
            } => assert_eq!((status, backend), (Status::Enabled, None)),
            e => panic!("decoded as {e:?}"),
        }
    }

    /// T5: an older CLI, which knows the event without `backend`, decodes
    /// the newer one
    #[test]
    fn older_cli_decodes_state_event_with_backend() {
        #[derive(Deserialize)]
        #[serde(tag = "event", rename_all = "snake_case")]
        enum OldEvent {
            Emulation { enabled: bool, status: Status },
        }
        let event = FrontendEvent::Emulation {
            enabled: true,
            status: Status::Enabled,
            backend: Some(BackendInUse {
                name: "libei".into(),
                auto: false,
            }),
        };
        let line = serde_json::to_string(&event).unwrap();
        let OldEvent::Emulation { enabled, status } = serde_json::from_str(&line).unwrap();
        assert!(enabled);
        assert_eq!(status, Status::Enabled);
    }
}

//! Agent requests and responses: the one request set behind the agent
//! channel (TLS, ALPN [`ALPN`]), the IPC `agent` request and `mimouse mcp`.
//!
//! On the agent channel every message is one frame: `len: u32 BE | JSON`.
//! The requester sends [`Envelope::Request`]s; the target answers each with
//! an [`Envelope::Response`] carrying the same `id`, and may send an
//! [`Envelope::Event`] at any time. Binary data (images, clipboard
//! contents) travels base64-encoded in [`Blob`]s.

use std::{fmt, io};

use base64::{Engine, engine::general_purpose::STANDARD};
use input_event::scancode;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// TLS ALPN protocol of the agent channel
pub const ALPN: &[u8] = b"mimouse-agent/1";

/// protocol version reported in [`Hello`]
pub const VERSION: u32 = 1;

/// Largest frame: a full-resolution screenshot or a `MAX_CLIPBOARD_SIZE`
/// clipboard item, base64-encoded, plus the JSON around it.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

const _: () = assert!(crate::clipboard::MAX_CLIPBOARD_SIZE * 4 / 3 + 4096 < MAX_FRAME);

/// a request from an agent: `{"op": "...", ...}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// capabilities, grants and state of the target; opens no session
    Hello,
    /// Opens a session (actions open one implicitly). The target shows a
    /// notice naming the requester and `reason`.
    Open {
        #[serde(default)]
        reason: Option<String>,
    },
    /// ends the session, releasing everything it holds
    Close,
    /// Presses `keys` in order, then releases them in reverse (a chord such
    /// as `[KeyLeftCtrl, KeyL]`).
    Key {
        keys: Vec<scancode::Linux>,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// types `text` in the target's own keyboard layout
    Type {
        text: String,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// moves the pointer to `x`,`y` in desktop pixels ([`Geometry`])
    PointerTo {
        x: i32,
        y: i32,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// moves the pointer relatively, in desktop pixels
    PointerMove {
        dx: i32,
        dy: i32,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// presses and releases `button` `count` times (2 = double click)
    Click {
        #[serde(default)]
        button: Button,
        #[serde(default = "one")]
        count: u8,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// presses or releases `button` (drag)
    Button {
        button: Button,
        pressed: bool,
        #[serde(default)]
        observe: Option<Observe>,
    },
    /// scrolls by wheel notches; positive `dy` scrolls down, positive `dx`
    /// right
    Scroll {
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
        #[serde(default)]
        observe: Option<Observe>,
    },
    Screenshot(Shot),
    /// the target's clipboard contents in `mime`, else its first format
    ClipGet {
        #[serde(default)]
        mime: Option<String>,
    },
    /// replaces the target's clipboard with `data` as `mime`
    ClipSet {
        mime: String,
        data: Blob,
    },
}

fn one() -> u8 {
    1
}

impl Request {
    /// what the request needs from the target's grants; `None`: nothing
    pub fn scope(&self) -> Option<Scope> {
        match self {
            Request::Hello | Request::Open { .. } | Request::Close => None,
            Request::Key { .. }
            | Request::Type { .. }
            | Request::PointerTo { .. }
            | Request::PointerMove { .. }
            | Request::Click { .. }
            | Request::Button { .. }
            | Request::Scroll { .. } => Some(Scope::Input),
            Request::Screenshot(_) => Some(Scope::Screen),
            Request::ClipGet { .. } | Request::ClipSet { .. } => Some(Scope::Clipboard),
        }
    }

    /// the frame to return after the action, if asked for
    pub fn observe(&self) -> Option<&Observe> {
        match self {
            Request::Key { observe, .. }
            | Request::Type { observe, .. }
            | Request::PointerTo { observe, .. }
            | Request::PointerMove { observe, .. }
            | Request::Click { observe, .. }
            | Request::Button { observe, .. }
            | Request::Scroll { observe, .. } => observe.as_ref(),
            _ => None,
        }
    }

    /// The request as recorded in the audit log: sizes and counts, never
    /// typed text, clipboard contents, coordinates' targets or pixels.
    pub fn audit(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Request::Hello => json!({"op": "hello"}),
            Request::Open { reason } => {
                json!({"op": "open", "reason_len": reason.as_ref().map(|r| r.chars().count())})
            }
            Request::Close => json!({"op": "close"}),
            Request::Key { keys, .. } => json!({"op": "key", "keys": keys.len()}),
            Request::Type { text, .. } => json!({"op": "type", "chars": text.chars().count()}),
            Request::PointerTo { .. } => json!({"op": "pointer_to"}),
            Request::PointerMove { .. } => json!({"op": "pointer_move"}),
            Request::Click { button, count, .. } => {
                json!({"op": "click", "button": button, "count": count})
            }
            Request::Button {
                button, pressed, ..
            } => json!({"op": "button", "button": button, "pressed": pressed}),
            Request::Scroll { .. } => json!({"op": "scroll"}),
            Request::Screenshot(_) => json!({"op": "screenshot"}),
            Request::ClipGet { mime } => json!({"op": "clip_get", "mime": mime}),
            Request::ClipSet { mime, data } => {
                json!({"op": "clip_set", "mime": mime, "size": data.0.len()})
            }
        }
    }
}

/// what a grant covers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Input,
    Screen,
    Clipboard,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::Input => "input",
            Scope::Screen => "screen",
            Scope::Clipboard => "clipboard",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

/// a screenshot request
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shot {
    /// part of the desktop, in desktop pixels; `None`: all of it
    #[serde(default)]
    pub region: Option<Region>,
    /// longest edge of the returned image in pixels; larger images are
    /// scaled down (see [`Frame::scale`]); `None`: full resolution
    #[serde(default)]
    pub max_edge: Option<u32>,
    #[serde(default)]
    pub format: Format,
}

/// a rectangle in desktop pixels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Png,
    Jpeg,
}

/// Act, then observe (K2): after the action the target waits until the
/// screen has not changed for `settle_ms` (at most `timeout_ms`) and returns
/// a screenshot taken as `shot`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observe {
    #[serde(default)]
    pub settle_ms: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u32>,
    #[serde(default)]
    pub shot: Shot,
}

impl Observe {
    pub const DEFAULT_SETTLE_MS: u32 = 300;
    pub const DEFAULT_TIMEOUT_MS: u32 = 3000;
    /// longest wait a requester may ask for
    pub const MAX_TIMEOUT_MS: u32 = 30_000;
}

/// a target's answer: `{"result": "...", ...}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// the action was carried out; `frame` if it was observed
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<Frame>,
    },
    Hello(Hello),
    /// `text` was typed except the `unmapped` characters, which the
    /// target's layout cannot produce
    Typed {
        unmapped: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<Frame>,
    },
    Frame(Frame),
    /// clipboard contents as `mime`; `available`: every format offered
    Clip {
        mime: String,
        data: Blob,
        available: Vec<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl Response {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }

    /// attaches an observed frame to a successful action
    pub fn with_frame(self, observed: Frame) -> Self {
        match self {
            Response::Ok { .. } => Response::Ok {
                frame: Some(observed),
            },
            Response::Typed { unmapped, .. } => Response::Typed {
                unmapped,
                frame: Some(observed),
            },
            other => other,
        }
    }
}

/// why a request failed, for the agent to act on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// the target's grants do not allow it
    Denied,
    /// a person used the target; the session ended
    Preempted,
    /// the target can't now (locked, secure desktop, display off)
    Unavailable,
    /// the target can never do this (see [`Capabilities`])
    Unsupported,
    /// another requester holds the target's session
    Busy,
    /// the target is controlling another machine (it is sending input)
    Capturing,
    /// input emulation is off on the target
    EmulationOff,
    /// clipboard sharing is off or has no backend on the target
    ClipboardUnavailable,
    /// the clipboard holds nothing in a shareable format
    Empty,
    /// malformed or out-of-range request
    BadRequest,
    /// no such target
    UnknownTarget,
    /// the target could not be reached or the channel failed
    Unreachable,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self).expect("unit variant");
        f.write_str(s.as_str().expect("string"))
    }
}

/// a screenshot
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub format: Format,
    /// size of the image in pixels
    pub width: u32,
    pub height: u32,
    /// desktop pixels per image pixel (>= 1; > 1 when scaled down)
    pub scale: f64,
    /// desktop pixel of the image's top-left corner
    pub x: i32,
    pub y: i32,
    pub data: Blob,
}

impl Frame {
    /// the desktop pixel shown at image pixel `ix`,`iy`
    pub fn to_desktop(&self, ix: f64, iy: f64) -> (i32, i32) {
        (
            self.x + (ix * self.scale).round() as i32,
            self.y + (iy * self.scale).round() as i32,
        )
    }
}

/// what a target can do and lets this requester do, and its state (K1)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    /// `linux`, `windows`, `macos`
    pub os: String,
    pub hostname: String,
    #[serde(default)]
    pub emulation_backend: Option<String>,
    pub capabilities: Capabilities,
    pub grants: Grants,
    pub state: State,
    /// `None` when the desktop can't be measured (no screen access)
    #[serde(default)]
    pub geometry: Option<Geometry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// input can be emulated now
    pub input: bool,
    /// `pointer_to` places the pointer exactly
    pub absolute_pointer: bool,
    pub text: TextSupport,
    pub screen: bool,
    pub clipboard: bool,
    pub ui_tree: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSupport {
    /// any Unicode text
    Exact,
    /// characters the active layout produces; others are reported unmapped
    Layout,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    #[default]
    Deny,
    Allow,
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Grant::Deny => "deny",
            Grant::Allow => "allow",
        })
    }
}

/// the grants one requester has on a target
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grants {
    #[serde(default)]
    pub input: Grant,
    #[serde(default)]
    pub screen: Grant,
    #[serde(default)]
    pub clipboard: Grant,
}

impl Grants {
    pub fn get(&self, scope: Scope) -> Grant {
        match scope {
            Scope::Input => self.input,
            Scope::Screen => self.screen,
            Scope::Clipboard => self.clipboard,
        }
    }

    pub fn set(&mut self, scope: Scope, grant: Grant) {
        match scope {
            Scope::Input => self.input = grant,
            Scope::Screen => self.screen = grant,
            Scope::Clipboard => self.clipboard = grant,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// screen locked or secure desktop shown; `None`: unknown
    #[serde(default)]
    pub locked: Option<bool>,
    /// the target controls another machine and refuses agent input
    pub capturing: bool,
    /// time since the last input on the target; `None`: unknown
    #[serde(default)]
    pub idle_ms: Option<u64>,
    pub session: SessionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    None,
    /// this requester holds the session
    Yours,
    /// another requester holds it
    Other,
}

/// the target's desktop in desktop pixels: the space of screenshots and
/// `pointer_to`, origin at the top-left corner of the desktop's bounding box
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub monitors: Vec<Region>,
}

/// target → requester, unasked
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// the session ended
    Ended { reason: EndReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// the requester closed it
    Closed,
    /// no request for `agent.idle_timeout`
    Idle,
    /// a person used the target
    Preempted,
    /// the target started controlling another machine
    Capture,
    /// the requester's grants or trust were withdrawn
    Revoked,
    /// the target's service stopped or emulation ended
    Shutdown,
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self).expect("unit variant");
        f.write_str(s.as_str().expect("string"))
    }
}

/// a message on the agent channel
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Envelope {
    Request { id: u64, request: Request },
    Response { id: u64, response: Response },
    Event { event: Event },
}

/// bytes, base64 in JSON
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Blob(pub Vec<u8>);

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map(Blob)
            .map_err(serde::de::Error::custom)
    }
}

/// writes one frame
pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, message: &Envelope) -> io::Result<()> {
    let json = serde_json::to_vec(message)?;
    if json.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("agent frame of {} bytes exceeds {MAX_FRAME}", json.len()),
        ));
    }
    w.write_all(&(json.len() as u32).to_be_bytes()).await?;
    w.write_all(&json).await?;
    w.flush().await
}

/// What reading a frame produced: an undecodable frame is answered with an
/// error, not a dropped connection, when its `id` can be read.
#[derive(Debug)]
pub enum Read {
    Message(Envelope),
    /// valid JSON that is no known message; `id` if it had one
    Invalid {
        id: Option<u64>,
        error: String,
    },
}

/// reads one frame; `Ok(None)` at a clean end of stream
pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Read>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("agent frame of {len} bytes exceeds {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(decode(&buf)?))
}

fn decode(buf: &[u8]) -> io::Result<Read> {
    let value: serde_json::Value =
        serde_json::from_slice(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let id = value.get("id").and_then(serde_json::Value::as_u64);
    Ok(match serde_json::from_value::<Envelope>(value) {
        Ok(message) => Read::Message(message),
        Err(e) => Read::Invalid {
            id,
            error: e.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(message: Envelope) -> Envelope {
        let mut buf = Vec::new();
        write(&mut buf, &message).await.unwrap();
        match read(&mut buf.as_slice()).await.unwrap().unwrap() {
            Read::Message(m) => m,
            Read::Invalid { error, .. } => panic!("{error}"),
        }
    }

    #[tokio::test]
    async fn requests_responses_and_events_round_trip() {
        let request = Envelope::Request {
            id: 7,
            request: Request::ClipSet {
                mime: "image/png".into(),
                data: Blob(vec![0, 255, 7]),
            },
        };
        assert_eq!(round_trip(request.clone()).await, request);
        let response = Envelope::Response {
            id: 7,
            response: Response::error(ErrorCode::Denied, "no input grant"),
        };
        assert_eq!(round_trip(response.clone()).await, response);
        let event = Envelope::Event {
            event: Event::Ended {
                reason: EndReason::Preempted,
            },
        };
        assert_eq!(round_trip(event.clone()).await, event);
    }

    #[tokio::test]
    async fn an_unknown_request_keeps_its_id_for_the_error_reply() {
        let frame = br#"{"id": 9, "request": {"op": "format_disk"}}"#;
        let mut buf = (frame.len() as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(frame);
        match read(&mut buf.as_slice()).await.unwrap().unwrap() {
            Read::Invalid { id, .. } => assert_eq!(id, Some(9)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_reading_it() {
        let buf = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let e = read(&mut buf.as_slice()).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_audit_record_never_holds_text_or_contents() {
        let typed = Request::Type {
            text: "hunter2".into(),
            observe: None,
        };
        let set = Request::ClipSet {
            mime: "text/plain".into(),
            data: Blob(b"secret".to_vec()),
        };
        for request in [typed, set] {
            let record = request.audit().to_string();
            assert!(
                !record.contains("hunter2") && !record.contains("secret"),
                "{record}"
            );
        }
    }

    #[test]
    fn frame_maps_image_pixels_back_to_the_desktop() {
        let frame = Frame {
            format: Format::Png,
            width: 1568,
            height: 823,
            scale: 4880.0 / 1568.0,
            x: 100,
            y: 50,
            data: Blob::default(),
        };
        assert_eq!(frame.to_desktop(0., 0.), (100, 50));
        assert_eq!(frame.to_desktop(1568., 0.), (100 + 4880, 50));
    }
}

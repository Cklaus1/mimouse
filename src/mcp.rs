//! `mimouse mcp`: a Model Context Protocol server on stdio for AI agents.
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout (logs go to stderr). Tool
//! calls become agent requests ([`proto::Request`]) sent to the local
//! service over IPC, which runs them on this machine or forwards them to a
//! peer. The model works in image pixels of the last screenshot of each
//! target; the server keeps that frame's scale and origin and maps the
//! model's coordinates to desktop pixels ([`Frame::to_desktop`]).

use std::{collections::HashMap, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Args;
use futures::StreamExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{
    agent::{
        keys,
        proto::{Blob, Button, ErrorCode, Format, Frame, Observe, Region, Request, Response, Shot},
    },
    ipc::{
        AgentTarget, AsyncFrontendRequestWriter, FrontendEvent, FrontendRequest, Reply,
        connect_async,
    },
};

mod tools;

#[cfg(test)]
mod tests;

/// the protocol version this server prefers
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// versions `initialize` accepts; answered with the client's own if listed
const SUPPORTED_VERSIONS: [&str; 3] = [PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

/// default longest edge of screenshots: what vision models take unscaled
pub const DEFAULT_MAX_EDGE: u32 = 1568;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// how long the service gets to connect
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Agent requests may wait for the screen to settle (`Observe`) and cross
/// the network to a peer.
const REPLY_TIMEOUT: Duration = Duration::from_millis(Observe::MAX_TIMEOUT_MS as u64 + 30_000);

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct McpArgs {
    /// longest edge of screenshots in pixels; larger screens are scaled down
    #[arg(long, default_value_t = DEFAULT_MAX_EDGE, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_edge: u32,
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("stdio: {0}")]
    Io(#[from] std::io::Error),
}

/// serves MCP on stdin/stdout until stdin closes
pub async fn run(args: McpArgs) -> Result<(), McpError> {
    let mut server = Server::new(IpcService::default(), args.max_edge);
    serve(&mut server, tokio::io::stdin(), tokio::io::stdout()).await
}

/// answers each line of `input` on `output`
async fn serve<S: Service>(
    server: &mut Server<S>,
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> Result<(), McpError> {
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(answer) = server.handle(&line).await {
            let mut out = serde_json::to_vec(&answer).expect("JSON value");
            out.push(b'\n');
            output.write_all(&out).await?;
            output.flush().await?;
        }
    }
}

/// what the tools need from the service
pub(crate) trait Service {
    /// an agent request to `target`; `Err`: the service could not be asked
    /// or refused the request itself
    async fn agent(&mut self, target: &str, request: Request) -> Result<Response, String>;
    /// the machines agent requests can go to
    async fn targets(&mut self) -> Result<Vec<AgentTarget>, String>;
}

/// the local service, over the IPC socket the CLI uses
#[derive(Default)]
struct IpcService {
    conn: Option<Connection>,
}

struct Connection {
    tx: AsyncFrontendRequestWriter,
    /// replies by `seq`; closes when the connection does
    replies: mpsc::UnboundedReceiver<(u64, Result<Reply, String>)>,
    /// Reads the connection all the time: the service drops connections
    /// whose outbound queue fills, and it sends state events unasked.
    reader: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl IpcService {
    async fn request(&mut self, request: FrontendRequest) -> Result<Reply, String> {
        match self.exchange(request).await {
            Ok(result) => result,
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    /// outer `Err`: the connection failed and must be replaced
    async fn exchange(
        &mut self,
        request: FrontendRequest,
    ) -> Result<Result<Reply, String>, String> {
        if self.conn.is_none() {
            self.conn = Some(Self::connect().await?);
        }
        let conn = self.conn.as_mut().expect("connected");
        let seq = conn
            .tx
            .request(request)
            .await
            .map_err(|e| format!("lost the connection to the MiMouse service: {e}"))?;
        let wait = async {
            while let Some((s, result)) = conn.replies.recv().await {
                if s == seq {
                    return Ok(result);
                }
            }
            Err("the MiMouse service closed the connection".to_owned())
        };
        tokio::time::timeout(REPLY_TIMEOUT, wait)
            .await
            .map_err(|_| {
                format!(
                    "the MiMouse service did not answer within {} s",
                    REPLY_TIMEOUT.as_secs()
                )
            })?
    }

    async fn connect() -> Result<Connection, String> {
        let (mut rx, tx) = connect_async(Some(CONNECT_TIMEOUT)).await.map_err(|e| {
            format!("cannot reach the MiMouse service ({e}); is `mimouse` running on this machine?")
        })?;
        let (replies_tx, replies) = mpsc::unbounded_channel();
        let reader = tokio::task::spawn_local(async move {
            while let Some(event) = rx.next().await {
                match event {
                    Ok(FrontendEvent::Reply { seq, result }) => {
                        if replies_tx.send((seq, result)).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("service connection: {e}");
                        return;
                    }
                }
            }
        });
        Ok(Connection {
            tx,
            replies,
            reader,
        })
    }
}

impl Service for IpcService {
    async fn agent(&mut self, target: &str, request: Request) -> Result<Response, String> {
        let request = FrontendRequest::Agent {
            target: target.to_owned(),
            request,
        };
        match self.request(request).await? {
            Reply::Agent(response) => Ok(*response),
            other => Err(format!("unexpected reply from the service: {other:?}")),
        }
    }

    async fn targets(&mut self) -> Result<Vec<AgentTarget>, String> {
        match self.request(FrontendRequest::AgentTargets).await? {
            Reply::AgentTargets(targets) => Ok(targets),
            other => Err(format!("unexpected reply from the service: {other:?}")),
        }
    }
}

type RpcError = (i64, String);

/// a tool's content items, or the text of a tool error
type Outcome = Result<Vec<Value>, String>;

pub(crate) struct Server<S> {
    service: S,
    max_edge: u32,
    /// per target, the last frame the model saw (without pixels)
    frames: HashMap<String, Frame>,
}

impl<S: Service> Server<S> {
    pub(crate) fn new(service: S, max_edge: u32) -> Self {
        Self {
            service,
            max_edge,
            frames: HashMap::new(),
        }
    }

    /// the answer to one JSON-RPC message; `None` for notifications
    async fn handle(&mut self, line: &[u8]) -> Option<Value> {
        let message: Value = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(e) => return Some(error(Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
        };
        let Value::Object(mut message) = message else {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "expected a JSON-RPC request object".into(),
            ));
        };
        let id = message.remove("id");
        let Some(Value::String(method)) = message.remove("method") else {
            // responses need no answer (this server sends no requests)
            if message.contains_key("result") || message.contains_key("error") {
                return None;
            }
            return Some(error(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "missing method".into(),
            ));
        };
        let params = message.remove("params").unwrap_or(Value::Null);
        let Some(id) = id else {
            log::debug!("notification {method}");
            return None;
        };
        Some(match self.call(&method, params).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error(id, code, message),
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => initialize(params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::list(self.max_edge)})),
            "tools/call" => self.tools_call(params).await,
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        }
    }

    async fn tools_call(&mut self, params: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct Call {
            name: String,
            #[serde(default)]
            arguments: Option<Value>,
        }
        let call: Call = serde_json::from_value(params)
            .map_err(|e| (INVALID_PARAMS, format!("invalid tools/call params: {e}")))?;
        let arguments = call.arguments.unwrap_or_else(|| json!({}));
        let outcome = match call.name.as_str() {
            "list_targets" => self.list_targets().await,
            "session_open" => self.session_open(arguments).await,
            "session_close" => self.session_close(arguments).await,
            "screenshot" => self.screenshot(arguments).await,
            "pointer_to" => self.pointer_to(arguments).await,
            "click" => self.click(arguments).await,
            "key" => self.key(arguments).await,
            "type" => self.type_text(arguments).await,
            "scroll" => self.scroll(arguments).await,
            "clip_get" => self.clip_get(arguments).await,
            "clip_set" => self.clip_set(arguments).await,
            name => return Err((INVALID_PARAMS, format!("unknown tool: {name}"))),
        };
        Ok(match outcome {
            Ok(content) => json!({"content": content, "isError": false}),
            Err(message) => json!({"content": [text(message)], "isError": true}),
        })
    }

    async fn list_targets(&mut self) -> Outcome {
        let targets = self.service.targets().await.map_err(service_error)?;
        let mut out = Vec::with_capacity(targets.len());
        for t in targets {
            let mut entry = json!({"target": t.target, "name": t.name});
            if let Some(fingerprint) = t.fingerprint {
                entry["fingerprint"] = json!(fingerprint);
            }
            match self.service.agent(&t.target, Request::Hello).await {
                Ok(Response::Hello(hello)) => entry["hello"] = json!(hello),
                Ok(Response::Error { code, message }) => {
                    entry["error"] = json!({"code": code, "message": message})
                }
                Ok(other) => {
                    entry["error"] =
                        json!({"code": ErrorCode::Internal, "message": unexpected(&other)})
                }
                Err(e) => entry["error"] = json!({"code": "service_error", "message": e}),
            }
            out.push(entry);
        }
        Ok(vec![text(pretty(&out))])
    }

    async fn session_open(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            reason: Option<String>,
        }
        let a: A = parse(arguments)?;
        let target = a.target;
        self.act(&target, Request::Open { reason: a.reason }).await
    }

    async fn session_close(&mut self, arguments: Value) -> Outcome {
        let a: TargetOnly = parse(arguments)?;
        self.act(&a.target, Request::Close).await
    }

    async fn screenshot(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            region: Option<Area>,
            #[serde(default)]
            coords: Coords,
            #[serde(default)]
            max_edge: Option<u32>,
            #[serde(default)]
            format: Format,
        }
        #[derive(Deserialize)]
        struct Area {
            x: f64,
            y: f64,
            width: f64,
            height: f64,
        }
        let a: A = parse(arguments)?;
        let region = match a.region {
            None => None,
            Some(r) => {
                if !(r.width > 0.0 && r.height > 0.0) {
                    return Err(bad_request("region width and height must be positive"));
                }
                let (x, y) = self.to_desktop(&a.target, r.x, r.y, a.coords)?;
                let scale = match a.coords {
                    Coords::Image => self.frames[&a.target].scale,
                    Coords::Desktop => 1.0,
                };
                Some(Region {
                    x,
                    y,
                    width: (r.width * scale).round().max(1.0) as u32,
                    height: (r.height * scale).round().max(1.0) as u32,
                })
            }
        };
        let shot = Shot {
            region,
            max_edge: Some(a.max_edge.unwrap_or(self.max_edge)),
            format: a.format,
        };
        match self.request(&a.target, Request::Screenshot(shot)).await? {
            Response::Frame(frame) => Ok(self.frame_content(&a.target, frame)),
            other => Err(unexpected(&other)),
        }
    }

    async fn pointer_to(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            x: f64,
            y: f64,
            #[serde(default)]
            coords: Coords,
            #[serde(default)]
            observe: Option<ObserveArg>,
        }
        let a: A = parse(arguments)?;
        let (x, y) = self.to_desktop(&a.target, a.x, a.y, a.coords)?;
        let observe = self.observe(a.observe);
        self.act(&a.target, Request::PointerTo { x, y, observe })
            .await
    }

    async fn click(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            button: Button,
            #[serde(default = "one")]
            count: u8,
            #[serde(default)]
            x: Option<f64>,
            #[serde(default)]
            y: Option<f64>,
            #[serde(default)]
            coords: Coords,
            #[serde(default)]
            observe: Option<ObserveArg>,
        }
        let a: A = parse(arguments)?;
        if !(1..=3).contains(&a.count) {
            return Err(bad_request("count must be 1, 2 or 3"));
        }
        match (a.x, a.y) {
            (Some(x), Some(y)) => {
                let (x, y) = self.to_desktop(&a.target, x, y, a.coords)?;
                let moved = Request::PointerTo {
                    x,
                    y,
                    observe: None,
                };
                self.request(&a.target, moved).await?;
            }
            (None, None) => {}
            _ => return Err(bad_request("pass both x and y, or neither")),
        }
        let observe = self.observe(a.observe);
        let click = Request::Click {
            button: a.button,
            count: a.count,
            observe,
        };
        self.act(&a.target, click).await
    }

    async fn key(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            keys: String,
            #[serde(default)]
            observe: Option<ObserveArg>,
        }
        let a: A = parse(arguments)?;
        let keys = keys::parse_chord(&a.keys).map_err(|e| bad_request(&e))?;
        let observe = self.observe(a.observe);
        self.act(&a.target, Request::Key { keys, observe }).await
    }

    async fn type_text(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            text: String,
            #[serde(default)]
            observe: Option<ObserveArg>,
        }
        let a: A = parse(arguments)?;
        let observe = self.observe(a.observe);
        self.act(
            &a.target,
            Request::Type {
                text: a.text,
                observe,
            },
        )
        .await
    }

    async fn scroll(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            dx: i32,
            #[serde(default)]
            dy: i32,
            #[serde(default)]
            observe: Option<ObserveArg>,
        }
        let a: A = parse(arguments)?;
        let observe = self.observe(a.observe);
        let scroll = Request::Scroll {
            dx: a.dx,
            dy: a.dy,
            observe,
        };
        self.act(&a.target, scroll).await
    }

    async fn clip_get(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            mime: Option<String>,
        }
        let a: A = parse(arguments)?;
        let mime = a.mime.unwrap_or_else(|| "text/plain".into());
        let request = Request::ClipGet { mime: Some(mime) };
        let (mime, data, available) = match self.request(&a.target, request).await? {
            Response::Clip {
                mime,
                data,
                available,
            } => (mime, data, available),
            other => return Err(unexpected(&other)),
        };
        let about = text(format!(
            "clipboard of {}: {mime} ({} bytes); formats on offer: {}",
            a.target,
            data.0.len(),
            available.join(", ")
        ));
        let contents = if mime.starts_with("text/") {
            text(String::from_utf8_lossy(&data.0).into_owned())
        } else if mime.starts_with("image/") {
            json!({"type": "image", "data": STANDARD.encode(&data.0), "mimeType": mime})
        } else {
            json!({
                "type": "resource",
                "resource": {
                    "uri": format!("mimouse://{}/clipboard", a.target),
                    "mimeType": mime,
                    "blob": STANDARD.encode(&data.0),
                }
            })
        };
        Ok(vec![about, contents])
    }

    async fn clip_set(&mut self, arguments: Value) -> Outcome {
        #[derive(Deserialize)]
        struct A {
            #[serde(default = "self_target")]
            target: String,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            mime: Option<String>,
            #[serde(default)]
            data_base64: Option<String>,
        }
        let a: A = parse(arguments)?;
        let (mime, data) = match (a.text, a.mime, a.data_base64) {
            (Some(text), None, None) => (crate::clipboard::MIME_TEXT.to_owned(), text.into_bytes()),
            (None, Some(mime), Some(data)) => {
                let data = STANDARD
                    .decode(data.as_bytes())
                    .map_err(|e| bad_request(&format!("data_base64 is not base64: {e}")))?;
                (mime, data)
            }
            _ => {
                return Err(bad_request(
                    "pass either `text`, or `mime` and `data_base64`",
                ));
            }
        };
        let request = Request::ClipSet {
            mime,
            data: Blob(data),
        };
        self.act(&a.target, request).await
    }

    /// sends an action and reports its result, with the observed frame
    async fn act(&mut self, target: &str, request: Request) -> Outcome {
        match self.request(target, request).await? {
            Response::Ok { frame } => Ok(self.with_frame(target, vec![text("ok".into())], frame)),
            Response::Typed { unmapped, frame } => {
                let result = if unmapped.is_empty() {
                    "typed".to_owned()
                } else {
                    let unmapped: Vec<char> = unmapped.chars().collect();
                    format!(
                        "typed, except these characters the target's keyboard layout cannot produce \
                         (unmapped): {unmapped:?}"
                    )
                };
                Ok(self.with_frame(target, vec![text(result)], frame))
            }
            other => Err(unexpected(&other)),
        }
    }

    /// the target's answer; agent errors become the tool error text
    async fn request(&mut self, target: &str, request: Request) -> Result<Response, String> {
        match self.service.agent(target, request).await {
            Ok(Response::Error { code, message }) => Err(agent_error(code, &message)),
            Ok(response) => Ok(response),
            Err(e) => Err(service_error(e)),
        }
    }

    fn with_frame(
        &mut self,
        target: &str,
        mut content: Vec<Value>,
        frame: Option<Frame>,
    ) -> Vec<Value> {
        if let Some(frame) = frame {
            content.extend(self.frame_content(target, frame));
        }
        content
    }

    /// remembers `frame` as the target's coordinate reference and returns
    /// its description and image
    fn frame_content(&mut self, target: &str, frame: Frame) -> Vec<Value> {
        let mime = match frame.format {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
        };
        let about = text(format!(
            "screenshot of {target}: {}x{} image pixels; scale {:.4} desktop px per image px; \
             top-left at desktop ({}, {}). Pass pixels of this image as x/y.",
            frame.width, frame.height, frame.scale, frame.x, frame.y
        ));
        let image =
            json!({"type": "image", "data": STANDARD.encode(&frame.data.0), "mimeType": mime});
        self.frames.insert(
            target.to_owned(),
            Frame {
                data: Blob::default(),
                ..frame
            },
        );
        vec![about, image]
    }

    /// `x`,`y` in desktop pixels
    fn to_desktop(
        &self,
        target: &str,
        x: f64,
        y: f64,
        coords: Coords,
    ) -> Result<(i32, i32), String> {
        if !(x.is_finite() && y.is_finite()) {
            return Err(bad_request("x and y must be numbers"));
        }
        match coords {
            Coords::Desktop => Ok((x.round() as i32, y.round() as i32)),
            Coords::Image => {
                let Some(frame) = self.frames.get(target) else {
                    return Err(format!(
                        "no_screenshot: there is no screenshot of `{target}` yet. Take a screenshot \
                         first (x/y are pixels of the last screenshot), or pass coords=\"desktop\"."
                    ));
                };
                if x < 0.0 || y < 0.0 || x >= frame.width as f64 || y >= frame.height as f64 {
                    return Err(bad_request(&format!(
                        "({x}, {y}) is outside the last screenshot of `{target}` ({}x{} pixels)",
                        frame.width, frame.height
                    )));
                }
                Ok(frame.to_desktop(x, y))
            }
        }
    }

    fn observe(&self, arg: Option<ObserveArg>) -> Option<Observe> {
        let (settle_ms, timeout_ms) = match arg? {
            ObserveArg::Flag(false) => return None,
            ObserveArg::Flag(true) => (None, None),
            ObserveArg::Settings {
                settle_ms,
                timeout_ms,
            } => (settle_ms, timeout_ms),
        };
        Some(Observe {
            settle_ms,
            timeout_ms,
            shot: Shot {
                region: None,
                max_edge: Some(self.max_edge),
                format: Format::Png,
            },
        })
    }
}

fn initialize(params: Value) -> Result<Value, RpcError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        protocol_version: String,
    }
    let params: Params = serde_json::from_value(params)
        .map_err(|e| (INVALID_PARAMS, format!("invalid initialize params: {e}")))?;
    let version = SUPPORTED_VERSIONS
        .into_iter()
        .find(|v| *v == params.protocol_version)
        .unwrap_or(PROTOCOL_VERSION);
    Ok(json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "mimouse", "title": "MiMouse", "version": env!("CARGO_PKG_VERSION")},
        "instructions": tools::INSTRUCTIONS,
    }))
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Coords {
    #[default]
    Image,
    Desktop,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ObserveArg {
    Flag(bool),
    Settings {
        #[serde(default)]
        settle_ms: Option<u32>,
        #[serde(default)]
        timeout_ms: Option<u32>,
    },
}

#[derive(Deserialize)]
struct TargetOnly {
    #[serde(default = "self_target")]
    target: String,
}

fn self_target() -> String {
    "self".into()
}

fn one() -> u8 {
    1
}

fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| bad_request(&format!("invalid arguments: {e}")))
}

fn bad_request(message: &str) -> String {
    format!("{}: {message}", ErrorCode::BadRequest)
}

fn service_error(message: String) -> String {
    format!("service_error: {message}")
}

fn unexpected(response: &Response) -> String {
    format!(
        "{}: unexpected answer from the target: {response:?}",
        ErrorCode::Internal
    )
}

/// `code: message`, plus what the model should do about it
fn agent_error(code: ErrorCode, message: &str) -> String {
    let advice = match code {
        ErrorCode::Preempted => {
            "A person is using the target, which ended this session. Stop and ask the user \
             before doing anything else on this target; do not retry on your own."
        }
        ErrorCode::Denied => {
            "The target's owner has not granted this to the agent. Tell the user; only they can \
             grant it."
        }
        ErrorCode::Busy => "Another agent holds the target's session. Tell the user.",
        ErrorCode::Unavailable => {
            "The target is locked, shows a secure desktop or its display is off. Ask the user to \
             unlock it."
        }
        ErrorCode::Capturing => {
            "The target is controlling another machine right now. Ask the user to switch back."
        }
        ErrorCode::EmulationOff => "Input emulation is off on the target. Tell the user.",
        ErrorCode::Unreachable => {
            "The target cannot be reached. Check list_targets or ask the user."
        }
        ErrorCode::UnknownTarget => "Use a target from list_targets.",
        _ => "",
    };
    match advice {
        "" => format!("{code}: {message}"),
        advice => format!("{code}: {message}. {advice}"),
    }
}

fn text(text: String) -> Value {
    json!({"type": "text", "text": text})
}

fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).expect("JSON value")
}

fn error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

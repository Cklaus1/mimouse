//! `mimouse cli agent <verb> [-t target]`: agent requests from the shell.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{CliError, REPLY_TIMEOUT, to_json, wait_for_reply};
use crate::{
    agent::{
        IPC_REPLY_TIMEOUT, keys,
        proto::{
            Blob, Button, Format, Frame, Grant, Hello, Observe, Region, Request, Response, Shot,
        },
    },
    ipc::{
        AgentGrantInfo, AgentTarget, AsyncFrontendEventReader, AsyncFrontendRequestWriter,
        FrontendRequest, Reply,
    },
};

#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[command(allow_negative_numbers = true)]
pub(super) struct AgentArgs {
    /// the machine to act on: `self`, a client id or host name
    #[arg(short, long, default_value = "self", global = true)]
    target: String,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
enum Verb {
    /// list the machines agent requests can go to
    Targets,
    /// set what a requester (`self`, a fingerprint or an authorized key's
    /// name) may do on this machine; unset scopes keep their value
    Grant {
        requester: String,
        #[arg(long, value_parser = parse::<Grant>)]
        input: Option<Grant>,
        #[arg(long, value_parser = parse::<Grant>)]
        screen: Option<Grant>,
        #[arg(long, value_parser = parse::<Grant>)]
        clipboard: Option<Grant>,
    },
    /// the target's capabilities, your grants there and its state
    Hello,
    /// open a session (actions open one too); the target shows `--reason`
    Open {
        #[arg(long)]
        reason: Option<String>,
    },
    /// end the session, releasing everything it holds
    Close,
    /// press a key chord such as `ctrl+l` or `shift+tab`
    Key {
        chord: String,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// type text in the target's keyboard layout
    Type {
        text: String,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// move the pointer to desktop pixel X,Y (the screenshot's pixels)
    PointerTo {
        x: i32,
        y: i32,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// move the pointer by DX,DY desktop pixels
    PointerMove {
        dx: i32,
        dy: i32,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// click a button (`--count 2`: double click)
    Click {
        #[arg(long, default_value = "left", value_parser = parse::<Button>)]
        button: Button,
        #[arg(long, default_value_t = 1)]
        count: u8,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// press or release a button (drag)
    Button {
        #[arg(value_parser = parse::<Button>)]
        button: Button,
        #[arg(value_parser = ["down", "up"])]
        state: String,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// scroll by wheel notches; positive DY scrolls down, positive DX right
    Scroll {
        #[arg(long, default_value_t = 0)]
        dx: i32,
        #[arg(long, default_value_t = 0)]
        dy: i32,
        #[command(flatten)]
        observe: ObserveArg,
    },
    /// save a screenshot of the target's desktop
    Screenshot {
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// part of the desktop: X,Y,WIDTH,HEIGHT in desktop pixels
        #[arg(long, value_parser = parse_region)]
        region: Option<Region>,
        /// scale the image down to this longest edge
        #[arg(long)]
        max_edge: Option<u32>,
        #[arg(long)]
        jpeg: bool,
    },
    /// print (or save) the target's clipboard
    ClipGet {
        #[arg(long)]
        mime: Option<String>,
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// replace the target's clipboard with TEXT or a file's contents
    ClipSet {
        #[arg(required_unless_present = "file")]
        text: Option<String>,
        #[arg(long, conflicts_with = "text")]
        file: Option<PathBuf>,
        #[arg(long, default_value = "text/plain")]
        mime: String,
    },
}

#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
struct ObserveArg {
    /// after the action, wait for the screen to settle and save a
    /// screenshot to FILE
    #[arg(long, value_name = "FILE")]
    observe: Option<PathBuf>,
}

impl ObserveArg {
    fn request(&self) -> Option<Observe> {
        self.observe.as_ref().map(|path| Observe {
            shot: Shot {
                format: format_of(path),
                ..Default::default()
            },
            ..Default::default()
        })
    }
}

/// a value the way the protocol spells it (`allow`, `right`)
fn parse<T: DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(s.to_owned())).map_err(|e| e.to_string())
}

fn parse_region(s: &str) -> Result<Region, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let [x, y, width, height] = parts[..] else {
        return Err("need X,Y,WIDTH,HEIGHT".into());
    };
    let bad = |v: &str| format!("`{v}` is not a number");
    let at = |v: &str| v.parse::<i32>().map_err(|_| bad(v));
    let size = |v: &str| v.parse::<u32>().map_err(|_| bad(v));
    Ok(Region {
        x: at(x)?,
        y: at(y)?,
        width: size(width)?,
        height: size(height)?,
    })
}

fn format_of(path: &Path) -> Format {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg") => Format::Jpeg,
        _ => Format::Png,
    }
}

pub(super) async fn run(
    args: AgentArgs,
    rx: AsyncFrontendEventReader,
    mut tx: AsyncFrontendRequestWriter,
    json: bool,
) -> Result<(), CliError> {
    let target = args.target;
    let (request, files) = match args.verb {
        Verb::Targets => {
            let seq = tx.request(FrontendRequest::AgentTargets).await?;
            let (result, _) = wait_for_reply(rx, seq, REPLY_TIMEOUT).await?;
            if let Reply::AgentTargets(targets) = result.map_err(CliError::Request)? {
                print_targets(&targets, json);
            }
            return Ok(());
        }
        Verb::Grant {
            requester,
            input,
            screen,
            clipboard,
        } => {
            let request = FrontendRequest::AgentGrant {
                requester,
                input,
                screen,
                clipboard,
            };
            let seq = tx.request(request).await?;
            let (result, _) = wait_for_reply(rx, seq, REPLY_TIMEOUT).await?;
            if let Reply::AgentGrant(info) = result.map_err(CliError::Request)? {
                print_grants(&info, json);
            }
            return Ok(());
        }
        verb => request(verb).await?,
    };
    let seq = tx
        .request(FrontendRequest::Agent { target, request })
        .await?;
    let (result, _) = wait_for_reply(rx, seq, IPC_REPLY_TIMEOUT).await?;
    let Reply::Agent(response) = result.map_err(CliError::Request)? else {
        return Err(CliError::Request("unexpected reply".into()));
    };
    answer(*response, files, json).await
}

/// where an answer's blobs go
#[derive(Default)]
struct Files {
    /// the observed frame, or the screenshot
    frame: Option<PathBuf>,
    /// clipboard contents
    clip: Option<PathBuf>,
}

async fn request(verb: Verb) -> Result<(Request, Files), CliError> {
    let observed = |observe: &ObserveArg| Files {
        frame: observe.observe.clone(),
        clip: None,
    };
    let request = match verb {
        Verb::Targets | Verb::Grant { .. } => unreachable!("handled in `run`"),
        Verb::Hello => (Request::Hello, Files::default()),
        Verb::Open { reason } => (Request::Open { reason }, Files::default()),
        Verb::Close => (Request::Close, Files::default()),
        Verb::Key { chord, observe } => (
            Request::Key {
                keys: keys::parse_chord(&chord).map_err(CliError::Request)?,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::Type { text, observe } => (
            Request::Type {
                text,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::PointerTo { x, y, observe } => (
            Request::PointerTo {
                x,
                y,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::PointerMove { dx, dy, observe } => (
            Request::PointerMove {
                dx,
                dy,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::Click {
            button,
            count,
            observe,
        } => (
            Request::Click {
                button,
                count,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::Button {
            button,
            state,
            observe,
        } => (
            Request::Button {
                button,
                pressed: state == "down",
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::Scroll { dx, dy, observe } => (
            Request::Scroll {
                dx,
                dy,
                observe: observe.request(),
            },
            observed(&observe),
        ),
        Verb::Screenshot {
            output,
            region,
            max_edge,
            jpeg,
        } => {
            let format = match (&output, jpeg) {
                (_, true) => Format::Jpeg,
                (Some(path), false) => format_of(path),
                (None, false) => Format::Png,
            };
            let shot = Shot {
                region,
                max_edge,
                format,
            };
            let files = Files {
                frame: output,
                clip: None,
            };
            (Request::Screenshot(shot), files)
        }
        Verb::ClipGet { mime, output } => (
            Request::ClipGet { mime },
            Files {
                frame: None,
                clip: output,
            },
        ),
        Verb::ClipSet { text, file, mime } => {
            let data = match (text, file) {
                (Some(text), _) => text.into_bytes(),
                (None, Some(file)) => tokio::fs::read(&file)
                    .await
                    .map_err(|e| CliError::Request(format!("{}: {e}", file.display())))?,
                (None, None) => unreachable!("clap requires one"),
            };
            (
                Request::ClipSet {
                    mime,
                    data: Blob(data),
                },
                Files::default(),
            )
        }
    };
    Ok(request)
}

async fn answer(response: Response, files: Files, json: bool) -> Result<(), CliError> {
    let mut shown = serde_json::to_value(&response).expect("serializable");
    let saved_frame = match &response {
        Response::Ok { frame: Some(frame) }
        | Response::Typed {
            frame: Some(frame), ..
        }
        | Response::Frame(frame) => save(files.frame.as_deref(), &frame.data).await?,
        _ => None,
    };
    if let Some(path) = &saved_frame {
        let frame = match shown.get_mut("frame") {
            Some(frame) => frame,
            None => &mut shown,
        };
        frame["data"] = serde_json::json!({ "file": path });
    }
    if let Response::Clip { data, .. } = &response {
        if let Some(path) = save(files.clip.as_deref(), data).await? {
            shown["data"] = serde_json::json!({ "file": path });
        }
    }
    if json {
        println!("{shown}");
    }
    match response {
        Response::Error { code, message } => Err(CliError::Request(format!("{code}: {message}"))),
        _ if json => Ok(()),
        Response::Hello(hello) => {
            print_hello(&hello);
            Ok(())
        }
        Response::Ok { frame } => {
            println!("ok");
            print_frame(frame.as_ref(), saved_frame.as_deref());
            Ok(())
        }
        Response::Typed { unmapped, frame } => {
            match unmapped.is_empty() {
                true => println!("typed"),
                false => println!("typed; the layout cannot produce: {unmapped}"),
            }
            print_frame(frame.as_ref(), saved_frame.as_deref());
            Ok(())
        }
        Response::Frame(frame) => match saved_frame {
            Some(path) => {
                print_frame(Some(&frame), Some(&path));
                Ok(())
            }
            None => Err(CliError::Request(
                "give `-o FILE` to save the screenshot (or `--json` for base64)".into(),
            )),
        },
        Response::Clip {
            mime,
            data,
            available,
        } => {
            if files.clip.is_some() {
                eprintln!(
                    "{mime}, {} bytes (offered: {})",
                    data.0.len(),
                    available.join(", ")
                );
                return Ok(());
            }
            match String::from_utf8(data.0) {
                Ok(text) if mime.starts_with("text/") => {
                    print!("{text}");
                    Ok(())
                }
                _ => Err(CliError::Request(format!(
                    "the clipboard holds {mime}; give `-o FILE` to save it"
                ))),
            }
        }
    }
}

/// writes `data` to `path`; `None` without a path
async fn save(path: Option<&Path>, data: &Blob) -> Result<Option<String>, CliError> {
    let Some(path) = path else { return Ok(None) };
    tokio::fs::write(path, &data.0)
        .await
        .map_err(|e| CliError::Request(format!("{}: {e}", path.display())))?;
    Ok(Some(path.display().to_string()))
}

fn print_frame(frame: Option<&Frame>, saved: Option<&str>) {
    let (Some(frame), Some(path)) = (frame, saved) else {
        return;
    };
    println!(
        "{path}: {}x{} image of desktop pixels from {},{}, {} desktop px per image px",
        frame.width, frame.height, frame.x, frame.y, frame.scale
    );
}

fn print_hello(hello: &Hello) {
    let c = &hello.capabilities;
    let g = &hello.grants;
    let s = &hello.state;
    println!(
        "{} ({}, protocol {})",
        hello.hostname, hello.os, hello.version
    );
    println!(
        "  input: {} (backend {}, absolute pointer {}, text {})",
        c.input,
        hello.emulation_backend.as_deref().unwrap_or("none"),
        c.absolute_pointer,
        to_json(&c.text).trim_matches('"'),
    );
    println!("  screen: {}, clipboard: {}", c.screen, c.clipboard);
    println!(
        "  your grants: input {}, screen {}, clipboard {}",
        g.input, g.screen, g.clipboard
    );
    let unknown = || "unknown".to_owned();
    println!(
        "  locked: {}, capturing: {}, idle: {}, session: {}",
        s.locked.map_or_else(unknown, |l| l.to_string()),
        s.capturing,
        s.idle_ms
            .map_or_else(unknown, |ms| format!("{} s", ms / 1000)),
        to_json(&s.session).trim_matches('"'),
    );
    if let Some(geometry) = &hello.geometry {
        let monitors: Vec<String> = geometry
            .monitors
            .iter()
            .map(|m| format!("{}x{}+{}+{}", m.width, m.height, m.x, m.y))
            .collect();
        println!(
            "  desktop: {}x{} ({})",
            geometry.width,
            geometry.height,
            monitors.join(", ")
        );
    }
}

fn print_targets(targets: &[AgentTarget], json: bool) {
    if json {
        println!("{}", to_json(&targets));
        return;
    }
    for t in targets {
        match &t.fingerprint {
            Some(fingerprint) => println!("{}\t{}\t{fingerprint}", t.target, t.name),
            None => println!("{}\t{}", t.target, t.name),
        }
    }
}

fn print_grants(info: &AgentGrantInfo, json: bool) {
    if json {
        println!("{}", to_json(info));
        return;
    }
    let g = &info.grants;
    println!(
        "{}: input {}, screen {}, clipboard {}",
        info.requester, g.input, g.screen, g.clipboard
    );
}

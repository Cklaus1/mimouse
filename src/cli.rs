use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use crate::ipc::{
    AsyncFrontendEventReader, ClientHandle, ClientInfo, ClipboardDirection, ConnectionError,
    DEFAULT_PORT, FrontendEvent, FrontendRequest, IpcError, Position, Reply, Status, connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect to the service: {0}")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    /// the service rejected or failed the request
    #[error("{0}")]
    Request(String),
    #[error("the service closed the connection")]
    Closed,
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "mimouse-cli", about = "MiMouse CLI interface")]
pub struct CliArgs {
    /// machine-readable output: one JSON value per result or event
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// set (or clear) the command run when the cursor enters a client
    SetEnterHook {
        id: ClientHandle,
        command: Option<String>,
    },
    /// change the port this machine listens on
    ChangePort { port: u16 },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// move control to a client now, as if the cursor crossed into it
    /// (bind it to a desktop shortcut)
    Focus { id: ClientHandle },
    /// print the current state, then stream events until interrupted
    Watch,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    let (rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    let request = match args.command {
        CliSubcommand::Watch => return watch(rx, args.json).await,
        cmd => request(cmd),
    };
    let seq = tx.request(request).await?;
    let (result, state) = wait_for_reply(rx, seq).await?;
    match result {
        Ok(reply) => {
            print_reply(&reply, &state, args.json);
            Ok(())
        }
        Err(e) => {
            if args.json {
                println!("{}", serde_json::json!({ "error": e }));
            }
            Err(CliError::Request(e))
        }
    }
}

fn request(cmd: CliSubcommand) -> FrontendRequest {
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
        }) => FrontendRequest::AddClient {
            hostname,
            port: port.unwrap_or(DEFAULT_PORT),
            ips: ips.unwrap_or_default(),
            enter_hook,
        },
        CliSubcommand::RemoveClient { id } => FrontendRequest::RemoveClient { id },
        CliSubcommand::Activate { id } => FrontendRequest::Activate { id },
        CliSubcommand::Deactivate { id } => FrontendRequest::Deactivate { id },
        CliSubcommand::List => FrontendRequest::List,
        CliSubcommand::SetHost { id, host } => FrontendRequest::SetHost { id, hostname: host },
        CliSubcommand::SetPort { id, port } => FrontendRequest::SetPort { id, port },
        CliSubcommand::SetPosition { id, pos } => {
            FrontendRequest::SetPosition { id, position: pos }
        }
        CliSubcommand::SetIps { id, ips } => FrontendRequest::SetIps { id, ips },
        CliSubcommand::SetEnterHook { id, command } => FrontendRequest::SetEnterHook {
            id,
            enter_hook: command,
        },
        CliSubcommand::ChangePort { port } => FrontendRequest::ChangePort { port },
        CliSubcommand::EnableCapture => FrontendRequest::EnableCapture,
        CliSubcommand::EnableEmulation => FrontendRequest::EnableEmulation,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => FrontendRequest::AuthorizeKey {
            description,
            fingerprint: sha256_fingerprint,
        },
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            FrontendRequest::RemoveAuthorizedKey {
                fingerprint: sha256_fingerprint,
            }
        }
        CliSubcommand::SaveConfig => FrontendRequest::SaveConfig,
        CliSubcommand::Focus { id } => FrontendRequest::Focus { id },
        CliSubcommand::Watch => unreachable!("handled in `run`"),
    }
}

/// capture / emulation state from the events the service sends first
#[derive(Debug, Default)]
struct ServiceState {
    capture: Status,
    emulation: Status,
}

/// skip state events until the reply to `seq` arrives
async fn wait_for_reply(
    mut rx: AsyncFrontendEventReader,
    seq: u64,
) -> Result<(Result<Reply, String>, ServiceState), CliError> {
    let mut state = ServiceState::default();
    while let Some(event) = rx.next().await {
        match event? {
            FrontendEvent::Reply { seq: s, result } if s == seq => return Ok((result, state)),
            FrontendEvent::Capture { status, .. } => state.capture = status,
            FrontendEvent::Emulation { status, .. } => state.emulation = status,
            _ => {}
        }
    }
    Err(CliError::Closed)
}

fn print_reply(reply: &Reply, state: &ServiceState, json: bool) {
    match (reply, json) {
        (Reply::Done, true) => println!("{}", serde_json::json!({ "ok": true })),
        (Reply::Done, false) => {}
        (Reply::Client(client), true) => println!("{}", to_json(client)),
        (Reply::Client(client), false) => println!("{}", client_line(client)),
        (Reply::Clients(clients), true) => println!(
            "{}",
            serde_json::json!({
                "clients": clients,
                "capture": state.capture,
                "emulation": state.emulation,
            })
        ),
        (Reply::Clients(clients), false) => {
            for client in clients {
                println!("{}", client_line(client));
            }
            println!("capture: {}", state.capture);
            println!("emulation: {}", state.emulation);
        }
    }
}

async fn watch(mut rx: AsyncFrontendEventReader, json: bool) -> Result<(), CliError> {
    while let Some(event) = rx.next().await {
        let event = event?;
        if json {
            println!("{}", to_json(&event));
        } else {
            print_event(&event);
        }
    }
    Err(CliError::Closed)
}

fn print_event(event: &FrontendEvent) {
    match event {
        FrontendEvent::Reply { .. } => {}
        FrontendEvent::Clients { clients } => {
            for client in clients {
                println!("{}", client_line(client));
            }
        }
        FrontendEvent::Created { client } | FrontendEvent::State { client } => {
            println!("{}", client_line(client))
        }
        FrontendEvent::Deleted { id } => println!("id {id}: removed"),
        FrontendEvent::Port { port, error: None } => println!("listening on port {port}"),
        FrontendEvent::Port {
            port,
            error: Some(e),
        } => println!("port change failed, still on {port}: {e}"),
        FrontendEvent::Capture { status, .. } => println!("capture: {status}"),
        FrontendEvent::Emulation { status, .. } => println!("emulation: {status}"),
        FrontendEvent::AuthorizedKeys { keys } => {
            for (fingerprint, description) in keys {
                println!("authorized: {description} {fingerprint}");
            }
        }
        FrontendEvent::Fingerprint { fingerprint } => println!("fingerprint: {fingerprint}"),
        FrontendEvent::Connection { addr, fingerprint } => {
            println!("connection from {addr} ({fingerprint})")
        }
        FrontendEvent::Entered { addr, position, .. } => println!("{addr} entered ({position})"),
        FrontendEvent::Disconnected { addr } => println!("{addr} disconnected"),
        FrontendEvent::ConnectionAttempt { fingerprint } => {
            println!("unauthorized connection attempt: {fingerprint}")
        }
        FrontendEvent::Clipboard {
            direction,
            peer,
            mime,
            size,
        } => {
            let direction = match direction {
                ClipboardDirection::Sent => "sent to",
                ClipboardDirection::Received => "received from",
            };
            println!(
                "clipboard {direction} {peer}: {size} bytes ({})",
                mime.join(", ")
            )
        }
    }
}

fn client_line(client: &ClientInfo) -> String {
    let host = client.config.hostname.as_deref().unwrap_or("unknown");
    format!(
        "id {}: {host}:{} ({}) active: {}, ips: {:?}",
        client.id, client.config.port, client.config.pos, client.state.active, client.state.ips
    )
}

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("serializable")
}

use clap::{Args, Parser, Subcommand};
use futures::{Stream, StreamExt};
use std::{io::BufRead, net::IpAddr, time::Duration};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    config::Config,
    discover::{self, DiscoverError},
    doctor,
    ipc::{
        AsyncFrontendEventReader, AsyncFrontendRequestWriter, BackendInUse, ClientHandle,
        ClientInfo, ClipboardDirection, ClipboardStatus, ConfigReport, ConnectionError,
        DEFAULT_PORT, FrontendEvent, FrontendRequest, IpcError, Position, Reply, Status,
        TelemetryCrash, TelemetryShow, TelemetryStatus, connect_async,
    },
    telemetry,
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
    /// e.g. a service of another version that cannot decode the request
    #[error("no reply from the service within {} s", REPLY_TIMEOUT.as_secs())]
    Timeout,
    #[error(transparent)]
    Discover(#[from] DiscoverError),
    #[error("{0} check(s) failed")]
    ChecksFailed(usize),
}

impl CliError {
    /// process exit code: 3 service not running, 4 request rejected,
    /// 5 doctor checks failed, 1 anything else
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::ServiceNotRunning(_) => 3,
            CliError::Request(_) => 4,
            CliError::ChecksFailed(_) => 5,
            _ => 1,
        }
    }
}

/// how long to wait for the reply to a request; longer than a `focus` takes
/// (`FOCUS_TIMEOUT` in capture.rs)
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

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

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Pair {
    /// host name, Tailscale name or address of the machine running `pair --accept`
    #[arg(
        required_unless_present_any = ["accept", "confirm", "cancel"],
        conflicts_with_all = ["accept", "confirm", "cancel"],
        requires = "position"
    )]
    host: Option<String>,
    /// where that machine is, relative to this one
    #[arg(long, requires = "host")]
    position: Option<Position>,
    /// its MiMouse port (default: the port of this machine)
    #[arg(long, requires = "host")]
    port: Option<u16>,
    /// wait for a pairing request from another machine
    #[arg(long, conflicts_with_all = ["confirm", "cancel"])]
    accept: bool,
    /// seconds to wait for the request (with --accept)
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// confirm the code of a running pairing (instead of answering its prompt)
    #[arg(long, value_name = "CODE", conflicts_with = "cancel")]
    confirm: Option<String>,
    /// abort a running pairing
    #[arg(long)]
    cancel: bool,
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
    /// list online Tailscale machines that answer on the MiMouse port
    Discover {
        /// port to probe (default: this machine's port)
        #[arg(long)]
        port: Option<u16>,
    },
    /// check the service, config, key, firewall and every active client
    Doctor,
    /// print the config in effect: each key's value and its source (flag,
    /// file, default), and the changes in the file that await a restart
    Config,
    /// pair with another machine: `pair --accept` there, `pair <host>
    /// --position <side>` here, then confirm the code both show
    Pair(Pair),
    /// anonymous usage statistics, off unless turned on
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
}

#[derive(Clone, Copy, Subcommand, Debug, PartialEq, Eq)]
enum TelemetryAction {
    /// send usage statistics once a day (lists what is collected)
    On,
    /// stop sending; deletes the install id and everything queued
    Off,
    /// whether it is on, the install id, the endpoint and the queue
    Status,
    /// print the exact JSON the next upload sends
    Show,
}

pub async fn run(args: CliArgs, config: Config) -> Result<(), CliError> {
    let json = args.json;
    let result = run_command(args, config).await;
    if let (Err(e), true) = (&result, json) {
        println!("{}", serde_json::json!({ "error": e.to_string() }));
    }
    result
}

async fn run_command(args: CliArgs, config: Config) -> Result<(), CliError> {
    match args.command {
        CliSubcommand::Discover { port } => {
            count_command("discover");
            return Ok(discover::run(&config, port, args.json).await?);
        }
        CliSubcommand::Doctor => {
            count_command("doctor");
            return match doctor::run(&config, args.json).await {
                0 => Ok(()),
                failed => Err(CliError::ChecksFailed(failed)),
            };
        }
        _ => {}
    }
    let (rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    let request = match args.command {
        CliSubcommand::Watch => return watch(rx, args.json).await,
        CliSubcommand::Pair(Pair {
            confirm: Some(code),
            ..
        }) => FrontendRequest::PairConfirm { code },
        CliSubcommand::Pair(Pair { cancel: true, .. }) => FrontendRequest::PairCancel,
        CliSubcommand::Pair(Pair {
            accept: true,
            timeout,
            ..
        }) => {
            eprintln!(
                "waiting {timeout} s for `mimouse cli pair <this machine> --position <side>` on the other machine"
            );
            return pair(rx, tx, FrontendRequest::PairAccept { timeout }, args.json).await;
        }
        CliSubcommand::Pair(Pair {
            host: Some(host),
            position: Some(position),
            port,
            ..
        }) => {
            let ips = match host.parse::<IpAddr>() {
                Ok(_) => vec![],
                Err(_) => discover::addresses(&config, &host).await,
            };
            let port = port.unwrap_or(config.port());
            let request = FrontendRequest::Pair {
                host,
                ips,
                port,
                position,
            };
            return pair(rx, tx, request, args.json).await;
        }
        cmd => request(cmd),
    };
    let turned_on = request == FrontendRequest::TelemetryOn;
    let seq = tx.request(request).await?;
    let (result, state) = wait_for_reply(rx, seq, REPLY_TIMEOUT).await?;
    let reply = result.map_err(CliError::Request)?;
    if turned_on && !args.json {
        println!("{}\n", telemetry::COLLECTED);
    }
    print_reply(&reply, &state, args.json);
    Ok(())
}

/// how long `doctor` and `discover` try to reach the service to have the
/// command counted
const COUNT_TIMEOUT: Duration = Duration::from_millis(500);

/// counts `command`, which runs without the service, for telemetry: in the
/// background, best effort, never delaying the command
fn count_command(command: &str) {
    let request = FrontendRequest::CountCommand {
        command: command.to_owned(),
    };
    tokio::task::spawn_local(async move {
        if let Ok((_rx, mut tx)) = connect_async(Some(COUNT_TIMEOUT)).await {
            let _ = tx.request(request).await;
        }
    });
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
        CliSubcommand::Config => FrontendRequest::Config,
        CliSubcommand::Telemetry { action } => match action {
            TelemetryAction::On => FrontendRequest::TelemetryOn,
            TelemetryAction::Off => FrontendRequest::TelemetryOff,
            TelemetryAction::Status => FrontendRequest::TelemetryStatus,
            TelemetryAction::Show => FrontendRequest::TelemetryShow,
        },
        CliSubcommand::Watch
        | CliSubcommand::Discover { .. }
        | CliSubcommand::Doctor
        | CliSubcommand::Pair(_) => {
            unreachable!("handled in `run`")
        }
    }
}

/// lines typed on stdin; a dedicated thread, because a blocking read must not
/// keep the runtime from shutting down
fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Start a pairing with `request` and follow it: show the code, ask whether
/// the other machine shows the same, report the outcome. Without terminal
/// input the code can be confirmed with `pair --confirm`.
async fn pair(
    mut rx: AsyncFrontendEventReader,
    mut tx: AsyncFrontendRequestWriter,
    request: FrontendRequest,
    json: bool,
) -> Result<(), CliError> {
    let mut pending = vec![tx.request(request).await?];
    let mut answers = stdin_lines();
    let mut stdin_open = true;
    // the code waiting for the user's answer
    let mut asking: Option<String> = None;
    loop {
        tokio::select! {
            event = rx.next() => {
                let event = event.ok_or(CliError::Closed)??;
                match &event {
                    FrontendEvent::Reply { seq, result } if pending.contains(seq) => {
                        pending.retain(|s| s != seq);
                        if let Err(e) = result {
                            return Err(CliError::Request(e.clone()));
                        }
                    }
                    FrontendEvent::PairCode { code, peer, fingerprint, position } => {
                        if json {
                            println!("{}", to_json(&event));
                        } else {
                            println!("pairing with {peer}");
                            println!("  fingerprint: {fingerprint}");
                            println!("  it will be the {position} neighbour of this machine");
                            println!("  code: {code}");
                        }
                        if stdin_open {
                            eprint!("Does {peer} show the code {code}? [y/N] ");
                        } else {
                            eprintln!("confirm with `mimouse cli pair --confirm {code}` or abort with `mimouse cli pair --cancel`");
                        }
                        asking = Some(code.clone());
                    }
                    FrontendEvent::Paired { id, peer, not_saved, .. } => {
                        if json {
                            println!("{}", to_json(&event));
                        } else {
                            println!("paired with {peer}; it is client {id} (`mimouse cli list`)");
                        }
                        if let Some(e) = not_saved {
                            eprintln!("paired, not saved: {e}; fix the config, then run `mimouse cli save-config`");
                        }
                        return Ok(());
                    }
                    FrontendEvent::PairFailed { reason } => {
                        if json {
                            println!("{}", to_json(&event));
                        }
                        return Err(CliError::Request(format!("pairing failed: {reason}")));
                    }
                    _ => {}
                }
            }
            answer = answers.recv(), if stdin_open && asking.is_some() => {
                let code = asking.take().expect("asking");
                let request = match answer {
                    Some(answer) if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") => {
                        eprintln!("waiting for the other machine to confirm ...");
                        FrontendRequest::PairConfirm { code }
                    }
                    Some(_) => FrontendRequest::PairCancel,
                    None => {
                        stdin_open = false;
                        eprintln!();
                        eprintln!("no terminal input: confirm with `mimouse cli pair --confirm {code}` or abort with `mimouse cli pair --cancel`");
                        continue;
                    }
                };
                pending.push(tx.request(request).await?);
            }
        }
    }
}

/// capture / emulation / clipboard state from the events the service sends
/// first
#[derive(Debug, Default)]
struct ServiceState {
    capture: Status,
    emulation: Status,
    /// the backends in use; `None` while disabled, or from services that
    /// do not report them
    capture_backend: Option<BackendInUse>,
    emulation_backend: Option<BackendInUse>,
    /// `None` from services that do not report it
    clipboard: Option<ClipboardStatus>,
}

/// skip state events until the reply to `seq` arrives, at most `limit`
async fn wait_for_reply(
    mut rx: impl Stream<Item = Result<FrontendEvent, IpcError>> + Unpin,
    seq: u64,
    limit: Duration,
) -> Result<(Result<Reply, String>, ServiceState), CliError> {
    let wait = async {
        let mut state = ServiceState::default();
        while let Some(event) = rx.next().await {
            match event? {
                FrontendEvent::Reply { seq: s, result } if s == seq => {
                    return Ok((result, state));
                }
                FrontendEvent::Capture {
                    status, backend, ..
                } => {
                    state.capture = status;
                    state.capture_backend = backend;
                }
                FrontendEvent::Emulation {
                    status, backend, ..
                } => {
                    state.emulation = status;
                    state.emulation_backend = backend;
                }
                FrontendEvent::ClipboardStatus(status) => state.clipboard = Some(status),
                _ => {}
            }
        }
        Err(CliError::Closed)
    };
    tokio::time::timeout(limit, wait)
        .await
        .map_err(|_| CliError::Timeout)?
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
                "capture_backend": state.capture_backend,
                "emulation": state.emulation,
                "emulation_backend": state.emulation_backend,
                "clipboard": state.clipboard,
            })
        ),
        (Reply::Clients(clients), false) => {
            for client in clients {
                println!("{}", client_line(client));
            }
            println!(
                "capture: {}",
                status_line(state.capture, state.capture_backend.as_ref())
            );
            println!(
                "emulation: {}",
                status_line(state.emulation, state.emulation_backend.as_ref())
            );
            if let Some(clipboard) = &state.clipboard {
                println!("clipboard: {clipboard}");
            }
        }
        (Reply::Config(report), true) => println!("{}", to_json(report)),
        (Reply::Config(report), false) => print!("{}", config_text(report)),
        (Reply::Telemetry(status), true) => println!("{}", to_json(status)),
        (Reply::Telemetry(status), false) => print!("{}", telemetry_text(status)),
        (Reply::TelemetryShow(show), true) => println!("{}", to_json(show)),
        (Reply::TelemetryShow(show), false) => {
            // stdout: exactly the bytes the upload sends
            if let Some(batch) = &show.batch {
                print!("{batch}");
            }
            eprint!("{}", show_notes(show));
        }
        (Reply::Agent(response), _) => println!("{}", to_json(response)),
        (Reply::AgentTargets(targets), _) => println!("{}", to_json(targets)),
        (Reply::AgentGrant(info), _) => println!("{}", to_json(info)),
    }
}

/// `telemetry status` for reading
fn telemetry_text(status: &TelemetryStatus) -> String {
    let mut text = match status.enabled {
        true => "telemetry: on\n".to_owned(),
        false => "telemetry: off (nothing is collected or sent)\n".to_owned(),
    };
    if let Some(error) = &status.error {
        text += &format!("not running: {error}\n");
    }
    if let Some(id) = &status.install_id {
        text += &format!("install id: {id}\n");
    }
    match (&status.endpoint, &status.endpoint_error) {
        (Some(endpoint), _) => text += &format!("endpoint: {endpoint}\n"),
        (None, Some(error)) => text += &format!("endpoint: none ({error})\n"),
        (None, None) => {}
    }
    if status.install_id.is_some() {
        text += &format!(
            "queued: {} completed day(s), {} crash record(s)\n",
            status.queued_days, status.queued_crashes
        );
    }
    if let Some(date) = &status.counting_since {
        text += &format!("counting since: {date} (UTC)\n");
    }
    match (&status.last_attempt, &status.last_error) {
        (Some(date), Some(error)) => text += &format!("last upload: {date}, failed: {error}\n"),
        (Some(date), None) => text += &format!("last upload: {date}\n"),
        _ => {}
    }
    text += &local_crashes_text(&status.local_crashes);
    text
}

/// what `telemetry show` adds on stderr around the batch
fn show_notes(show: &TelemetryShow) -> String {
    let mut text = match (show.enabled, &show.batch) {
        (false, _) => "telemetry is off: nothing is sent\n".to_owned(),
        (true, None) => "nothing to send yet: only completed days are sent\n".to_owned(),
        (true, Some(_)) => String::new(),
    };
    if let (true, Some(date)) = (show.enabled, &show.counting_since) {
        text += &format!("counting {date} (UTC), sent once the day is over\n");
    }
    text += &local_crashes_text(&show.local_crashes);
    text
}

fn local_crashes_text(crashes: &[TelemetryCrash]) -> String {
    if crashes.is_empty() {
        return String::new();
    }
    let mut text = "crashes recorded while telemetry was off (never uploaded):\n".to_owned();
    for crash in crashes {
        text += &format!("  {} ({})\n", crash.location, crash.message);
    }
    text
}

/// `enabled (libei, auto)`; the backend only where the service reports one
fn status_line(status: Status, backend: Option<&BackendInUse>) -> String {
    match backend {
        Some(backend) => format!("{status} ({backend})"),
        None => status.to_string(),
    }
}

/// `key = value (source)` per key, then the changes awaiting a restart
fn config_text(report: &ConfigReport) -> String {
    let mut text = format!("config file: {}\n", report.path.display());
    for key in &report.keys {
        let restart = if key.restart { ", restart-only" } else { "" };
        text += &format!(
            "{} = {} ({}{restart})\n",
            key.key,
            value_text(&key.value),
            key.source
        );
    }
    if !report.awaiting_restart.is_empty() {
        text += "changed in the file, applied at the next start:\n";
    }
    for pending in &report.awaiting_restart {
        text += &format!(
            "  {}: {} in the file, {} in effect\n",
            pending.key,
            value_text(&pending.on_disk),
            value_text(&pending.in_effect)
        );
    }
    text
}

/// a config value for reading: `none` for unset, a count for tables and
/// lists of tables (`cli list` shows clients and keys)
fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "none".to_owned(),
        serde_json::Value::Object(entries) => format!("{} entries", entries.len()),
        serde_json::Value::Array(items) if items.iter().any(|i| i.is_object()) => {
            format!("{} entries", items.len())
        }
        value => value.to_string(),
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
        FrontendEvent::Capture {
            status, backend, ..
        } => println!("capture: {}", status_line(*status, backend.as_ref())),
        FrontendEvent::Emulation {
            status, backend, ..
        } => println!("emulation: {}", status_line(*status, backend.as_ref())),
        FrontendEvent::ClipboardStatus(status) => println!("clipboard: {status}"),
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
        FrontendEvent::PairCode {
            code,
            peer,
            fingerprint,
            position,
        } => println!("pairing with {peer} ({position}, {fingerprint}): code {code}"),
        FrontendEvent::Paired {
            id,
            peer,
            fingerprint,
            not_saved: None,
        } => println!("paired with {peer} ({fingerprint}): client {id}"),
        FrontendEvent::Paired {
            id,
            peer,
            fingerprint,
            not_saved: Some(e),
        } => println!("paired with {peer} ({fingerprint}): client {id}; not saved: {e}"),
        FrontendEvent::PairFailed { reason } => println!("pairing failed: {reason}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn reply_wait_times_out() {
        // a service that never answers, e.g. one that could not decode the request
        let silent = futures::stream::pending::<Result<FrontendEvent, IpcError>>();
        let result = wait_for_reply(silent, 1, Duration::from_millis(50)).await;
        assert!(matches!(result, Err(CliError::Timeout)));
    }

    #[test]
    fn exit_codes_are_distinct() {
        let codes = [
            CliError::ServiceNotRunning(ConnectionError::Timeout).exit_code(),
            CliError::Request("rejected".into()).exit_code(),
            CliError::ChecksFailed(1).exit_code(),
            CliError::Closed.exit_code(),
        ];
        assert_eq!(codes, [3, 4, 5, 1]);
    }
}

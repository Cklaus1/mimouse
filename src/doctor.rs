//! `mimouse cli doctor`: checks the local setup and every active client,
//! printing pass/fail with a one-line fix. Exits non-zero if a check fails.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt, future::join_all};
use hickory_resolver::TokioResolver;
use rustls_pki_types::ServerName;
use serde::Serialize;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, UdpSocket},
    process::Command,
    time::timeout,
};
use tokio_rustls::TlsConnector;
use webrtc_dtls::{conn::DTLSConn, crypto::Certificate};

use crate::{
    config::{self, Config},
    crypto::{self, peers::Policy},
    ipc::{ClientInfo, FrontendEvent, connect_async},
};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    /// could not be checked, or a transient state
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Outcome,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Outcome::Pass,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Outcome::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Outcome::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// state of the running service, from the events every new IPC connection
/// receives first
#[derive(Default)]
struct ServiceState {
    clients: Option<Vec<ClientInfo>>,
    capture: Option<String>,
    emulation: Option<String>,
    port: Option<u16>,
    fingerprint: Option<String>,
    keys: Option<HashMap<String, String>>,
}

impl ServiceState {
    fn complete(&self) -> bool {
        self.clients.is_some()
            && self.capture.is_some()
            && self.emulation.is_some()
            && self.port.is_some()
            && self.fingerprint.is_some()
            && self.keys.is_some()
    }
}

/// `"enabled"`, `"disabled"` or `"awaiting_approval"` of a capture or
/// emulation event (as sent over IPC): its `status` field if it has one,
/// else its `enabled` flag
fn status_of(event: &serde_json::Value) -> Option<String> {
    if let Some(status) = event.get("status").and_then(|s| s.as_str()) {
        return Some(status.to_owned());
    }
    let enabled = event.get("enabled")?.as_bool()?;
    Some(if enabled { "enabled" } else { "disabled" }.to_owned())
}

async fn service_state() -> Result<ServiceState, String> {
    let (mut rx, _tx) = connect_async(Some(Duration::from_millis(500)))
        .await
        .map_err(|e| e.to_string())?;
    let mut state = ServiceState::default();
    let read = async {
        while let Some(event) = rx.next().await {
            let event = event.map_err(|e| e.to_string())?;
            match &event {
                FrontendEvent::Clients { clients } => state.clients = Some(clients.clone()),
                FrontendEvent::Capture { .. } => {
                    state.capture = serde_json::to_value(&event)
                        .ok()
                        .and_then(|v| status_of(&v))
                }
                FrontendEvent::Emulation { .. } => {
                    state.emulation = serde_json::to_value(&event)
                        .ok()
                        .and_then(|v| status_of(&v))
                }
                FrontendEvent::Port { port, .. } => state.port = Some(*port),
                FrontendEvent::Fingerprint { fingerprint } => {
                    state.fingerprint = Some(fingerprint.clone())
                }
                FrontendEvent::AuthorizedKeys { keys } => state.keys = Some(keys.clone()),
                _ => {}
            }
            if state.complete() {
                return Ok(());
            }
        }
        Err("the service closed the connection".to_owned())
    };
    match timeout(IPC_TIMEOUT, read).await {
        Ok(r) => r.map(|_| state),
        Err(_) => Err("no state from the service".to_owned()),
    }
}

fn start_service_fix() -> &'static str {
    #[cfg(windows)]
    return "start it: `schtasks /run /tn MiMouse` or `mimouse.exe daemon`";
    #[cfg(target_os = "macos")]
    return "start it: `mimouse daemon`";
    #[cfg(not(any(windows, target_os = "macos")))]
    return "start it: `systemctl --user start mimouse` or `mimouse daemon`";
}

fn status_check(name: &str, status: &str, enable: &str) -> Check {
    match status {
        "enabled" => Check::pass(name, "enabled"),
        "awaiting_approval" => Check::warn(
            name,
            "waiting for approval of the desktop portal dialog",
            "approve the remote input / input capture dialog on this machine",
        ),
        other => Check::fail(
            name,
            other,
            format!("`mimouse cli {enable}`; the service log names the cause"),
        ),
    }
}

fn key_check(path: &Path) -> Check {
    const NAME: &str = "key file";
    match crypto::key_file_is_private(path) {
        Ok(true) => Check::pass(NAME, format!("{} is private", path.display())),
        Ok(false) => {
            #[cfg(unix)]
            let fix = format!(
                "`chmod 600 {}` (the service also repairs it at startup)",
                path.display()
            );
            #[cfg(not(unix))]
            let fix = format!(
                "restart the service (it repairs the file at startup) or `icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%:F\"`",
                path.display()
            );
            Check::fail(
                NAME,
                format!("{} is readable by other users", path.display()),
                fix,
            )
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Check::fail(
            NAME,
            format!("{} does not exist", path.display()),
            "start the service once; it creates the key",
        ),
        Err(e) => Check::fail(
            NAME,
            format!("{}: {e}", path.display()),
            "check the file's ownership",
        ),
    }
}

/// the config file, and while it is invalid, the last good copy the service
/// runs on instead
fn config_check(path: &Path) -> Check {
    const NAME: &str = "config";
    let Err(e) = config::check_file(path) else {
        return Check::pass(NAME, format!("{} parses", path.display()));
    };
    let last_good = config::last_good_path(path);
    let fix = match config::check_file(&last_good) {
        Ok(()) => format!(
            "fix the file or restore the last good copy {}: `{}`; until then the service runs on that copy and refuses to save changes",
            last_good.display(),
            config::restore_command(path)
        ),
        Err(_) => "fix the file; until then the service runs on defaults and refuses to save changes (no last good copy)".into(),
    };
    Check::fail(NAME, format!("{}: {e}", path.display()), fix)
}

/// firewall state for the MiMouse port: UDP (input) and TCP (clipboard, pairing)
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Firewall {
    /// no filtering, with the reason
    Off(String),
    /// descriptions of the rules allowing the port; empty: blocked
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Rules { udp: Vec<String>, tcp: Vec<String> },
    /// the rules could not be read, with the reason
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proto {
    Udp,
    Tcp,
    Any,
}

impl Proto {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "udp" => Some(Self::Udp),
            "tcp" => Some(Self::Tcp),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    fn udp(self) -> bool {
        matches!(self, Self::Udp | Self::Any)
    }

    fn tcp(self) -> bool {
        matches!(self, Self::Tcp | Self::Any)
    }
}

/// `4242`, `4000:4300` / `4000-4300`, comma separated lists
fn ports_contain(list: &str, port: u16) -> Option<bool> {
    let mut found = false;
    for part in list.split(',') {
        let (lo, hi) = match part.split_once([':', '-']) {
            Some((lo, hi)) => (
                lo.trim().parse::<u16>().ok()?,
                hi.trim().parse::<u16>().ok()?,
            ),
            None => {
                let p = part.trim().parse::<u16>().ok()?;
                (p, p)
            }
        };
        found |= (lo..=hi).contains(&port);
    }
    Some(found)
}

/// `port[/proto]` with port lists and ranges; `None` if `s` is not a port spec
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn port_spec(s: &str, port: u16) -> Option<(bool, Proto)> {
    let (ports, proto) = match s.split_once('/') {
        Some((ports, proto)) => (ports, Proto::parse(proto)?),
        None => (s, Proto::Any),
    };
    Some((ports_contain(ports, port)?, proto))
}

/// `ufw status verbose` (or `ufw status`)
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_ufw(output: &str, port: u16) -> Firewall {
    let (mut udp, mut tcp) = (vec![], vec![]);
    let mut rules = false;
    for line in output.lines() {
        if let Some(status) = line.strip_prefix("Status:") {
            if status.trim() != "active" {
                return Firewall::Off("ufw is inactive".into());
            }
            continue;
        }
        if let Some(defaults) = line.strip_prefix("Default:") {
            if defaults.split(',').any(|d| d.trim() == "allow (incoming)") {
                return Firewall::Off("ufw allows incoming connections by default".into());
            }
            continue;
        }
        if line.starts_with("--") {
            rules = true;
            continue;
        }
        // columns are padded but not always separated by more than one
        // space, so split at the action: `To ACTION [IN|OUT|FWD] From [# comment]`
        let line = line.split_once('#').map_or(line, |(rule, _)| rule);
        let tokens: Vec<&str> = line.split_whitespace().filter(|t| *t != "(v6)").collect();
        let Some(action) = tokens
            .iter()
            .position(|t| matches!(*t, "ALLOW" | "DENY" | "REJECT" | "LIMIT"))
        else {
            continue;
        };
        let mut rest = &tokens[action + 1..];
        let direction = rest
            .first()
            .copied()
            .filter(|d| matches!(*d, "IN" | "OUT" | "FWD"));
        if direction.is_some() {
            rest = &rest[1..];
        }
        if !rules || tokens[action] != "ALLOW" || matches!(direction, Some("OUT" | "FWD")) {
            continue;
        }
        let from = rest.join(" ");
        let to = &tokens[..action];
        let (target, iface) = match to.iter().position(|t| *t == "on") {
            Some(on) => (&to[..on], to.get(on + 1).copied()),
            None => (to, None),
        };
        // `Anywhere`, an address, `port[/proto]` or `address port[/proto]`;
        // anything else is an application profile
        let mut proto = Some(Proto::Any);
        for token in target {
            let address = token.split('/').next().unwrap_or_default();
            if *token == "Anywhere" || address.parse::<IpAddr>().is_ok() {
                continue;
            }
            proto = match port_spec(token, port) {
                Some((true, p)) => Some(p),
                _ => None,
            };
        }
        let Some(proto) = proto else { continue };
        let mut desc = vec![];
        if from != "Anywhere" {
            desc.push(format!("from {from}"));
        }
        if let Some(iface) = iface {
            desc.push(format!("on {iface}"));
        }
        let desc = if desc.is_empty() {
            "from anywhere".to_owned()
        } else {
            desc.join(" ")
        };
        for (allowed, list) in [(proto.udp(), &mut udp), (proto.tcp(), &mut tcp)] {
            if allowed && !list.contains(&desc) {
                list.push(desc.clone());
            }
        }
    }
    Firewall::Rules { udp, tcp }
}

/// `ENABLED=no` in /etc/ufw/ufw.conf
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn ufw_disabled(conf: &str) -> bool {
    conf.lines()
        .any(|l| l.trim().eq_ignore_ascii_case("ENABLED=no"))
}

/// `firewall-cmd --list-ports` (space separated `port[-port]/proto`);
/// for `--info-service` output, the value of its `ports:` line
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn firewalld_ports(list: &str, port: u16) -> (bool, bool) {
    let (mut udp, mut tcp) = (false, false);
    for spec in list.split_whitespace() {
        if let Some((true, proto)) = port_spec(spec, port) {
            udp |= proto.udp();
            tcp |= proto.tcp();
        }
    }
    (udp, tcp)
}

/// the `ports:` line of `firewall-cmd --info-service=<name>`
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn firewalld_service_ports(info: &str) -> &str {
    info.lines()
        .find_map(|l| l.trim().strip_prefix("ports:"))
        .unwrap_or_default()
}

/// `netsh advfirewall show currentprofile state`: (profile, enabled)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_netsh_state(output: &str) -> Option<(String, bool)> {
    let profile = output
        .lines()
        .find_map(|l| l.trim().strip_suffix(" Profile Settings:"))?
        .to_owned();
    let on = output.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next() == Some("State")).then(|| words.next() == Some("ON"))
    })?;
    Some((profile, on))
}

/// `netsh advfirewall firewall show rule name=all dir=in verbose`: enabled
/// inbound allow rules for `port` in `profile` that apply to mimouse;
/// `None` if no rule could be parsed at all (e.g. localized output)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_netsh_rules(
    output: &str,
    port: u16,
    profile: &str,
) -> Option<(Vec<String>, Vec<String>)> {
    let mut rules: Vec<HashMap<&str, &str>> = vec![];
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key == "Rule Name" {
            rules.push(HashMap::new());
        }
        if let Some(rule) = rules.last_mut() {
            rule.insert(key, value);
        }
    }
    if rules.is_empty() {
        return None;
    }
    let (mut udp, mut tcp) = (vec![], vec![]);
    for rule in &rules {
        let get = |k: &str| rule.get(k).copied().unwrap_or_default();
        let applies = get("Enabled") == "Yes"
            && get("Direction") == "In"
            && get("Action") == "Allow"
            && (get("Profiles") == "Any"
                || get("Profiles")
                    .split(',')
                    .any(|p| p.trim().eq_ignore_ascii_case(profile)))
            && (get("LocalPort") == "Any" || ports_contain(get("LocalPort"), port) == Some(true))
            && match rule.get("Program") {
                None | Some(&"Any") => true,
                Some(program) => program
                    .rsplit(['\\', '/'])
                    .next()
                    .is_some_and(|exe| exe.eq_ignore_ascii_case("mimouse.exe")),
            };
        let Some(proto) = Proto::parse(get("Protocol")).filter(|_| applies) else {
            continue;
        };
        let name = format!("rule \"{}\"", get("Rule Name"));
        for (allowed, list) in [(proto.udp(), &mut udp), (proto.tcp(), &mut tcp)] {
            if allowed && !list.contains(&name) {
                list.push(name.clone());
            }
        }
    }
    Some((udp, tcp))
}

/// `socketfilterfw --getglobalstate`: whether the application firewall is on
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_socketfilterfw(output: &str) -> Option<bool> {
    if output.contains("enabled") {
        Some(true)
    } else if output.contains("disabled") {
        Some(false)
    } else {
        None
    }
}

/// stdout of a successful command
async fn output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(any(windows, target_os = "macos")))]
async fn firewall(port: u16) -> (&'static str, Firewall, String) {
    let ufw_conf = std::fs::read_to_string("/etc/ufw/ufw.conf");
    if let Ok(conf) = ufw_conf {
        let fix = format!("sudo ufw allow {port}/udp && sudo ufw allow {port}/tcp");
        if ufw_disabled(&conf) {
            return ("ufw", Firewall::Off("ufw is disabled".into()), fix);
        }
        let status = match output("ufw", &["status", "verbose"]).await {
            Some(s) => Some(s),
            None => output("sudo", &["-n", "ufw", "status", "verbose"]).await,
        };
        let fw = match status {
            Some(status) => parse_ufw(&status, port),
            None => Firewall::Unknown(
                "ufw is enabled; reading its rules needs root (`sudo ufw status`)".into(),
            ),
        };
        return ("ufw", fw, fix);
    }
    let fix = format!(
        "sudo firewall-cmd --permanent --add-port={port}/udp --add-port={port}/tcp && sudo firewall-cmd --reload"
    );
    match output("firewall-cmd", &["--state"]).await {
        Some(_) => {}
        None if Command::new("firewall-cmd")
            .arg("--version")
            .output()
            .await
            .is_ok() =>
        {
            return (
                "firewalld",
                Firewall::Off("firewalld is not running".into()),
                fix,
            );
        }
        None => {
            return (
                "firewall",
                Firewall::Unknown("neither ufw nor firewalld found".into()),
                format!("make sure nftables/iptables accept {port}/udp and {port}/tcp"),
            );
        }
    }
    let (Some(ports), Some(services)) = (
        output("firewall-cmd", &["--list-ports"]).await,
        output("firewall-cmd", &["--list-services"]).await,
    ) else {
        let fw = Firewall::Unknown("could not read the firewalld zone".into());
        return ("firewalld", fw, fix);
    };
    let mut sources = vec![("port rule".to_owned(), ports)];
    for service in services.split_whitespace() {
        let info = output("firewall-cmd", &[&format!("--info-service={service}")])
            .await
            .unwrap_or_default();
        let ports = firewalld_service_ports(&info).to_owned();
        sources.push((format!("service {service}"), ports));
    }
    let (mut udp, mut tcp) = (vec![], vec![]);
    for (source, ports) in sources {
        let (u, t) = firewalld_ports(&ports, port);
        if u {
            udp.push(source.clone());
        }
        if t {
            tcp.push(source);
        }
    }
    ("firewalld", Firewall::Rules { udp, tcp }, fix)
}

#[cfg(windows)]
async fn firewall(port: u16) -> (&'static str, Firewall, String) {
    let fix = format!(
        "in an admin shell: `netsh advfirewall firewall add rule name=MiMouse dir=in action=allow protocol=UDP localport={port}` and the same with protocol=TCP"
    );
    let state = output("netsh", &["advfirewall", "show", "currentprofile", "state"]).await;
    let Some((profile, on)) = state.as_deref().and_then(parse_netsh_state) else {
        return (
            "Windows Firewall",
            Firewall::Unknown("could not read the firewall state".into()),
            fix,
        );
    };
    if !on {
        let fw = Firewall::Off(format!("Windows Firewall is off ({profile} profile)"));
        return ("Windows Firewall", fw, fix);
    }
    let rules = output(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "show",
            "rule",
            "name=all",
            "dir=in",
            "verbose",
        ],
    )
    .await
    .unwrap_or_default();
    let fw = match parse_netsh_rules(&rules, port, &profile) {
        Some((udp, tcp)) => Firewall::Rules { udp, tcp },
        None => Firewall::Unknown("could not parse the firewall rules".into()),
    };
    ("Windows Firewall", fw, fix)
}

#[cfg(target_os = "macos")]
async fn firewall(_port: u16) -> (&'static str, Firewall, String) {
    let fix =
        "System Settings > Network > Firewall > Options: allow incoming connections for mimouse"
            .to_owned();
    let state = output(
        "/usr/libexec/ApplicationFirewall/socketfilterfw",
        &["--getglobalstate"],
    )
    .await;
    let fw = match state.as_deref().and_then(parse_socketfilterfw) {
        Some(false) => Firewall::Off("the application firewall is off".into()),
        Some(true) => Firewall::Unknown(
            "the application firewall is on; per-app rules are not readable".into(),
        ),
        None => Firewall::Unknown("could not read the firewall state".into()),
    };
    ("firewall", fw, fix)
}

fn firewall_check(frontend: &str, fw: Firewall, port: u16, fix: String) -> Check {
    let name = "firewall";
    match fw {
        Firewall::Off(why) => Check::pass(name, why),
        Firewall::Unknown(why) => Check::warn(name, why, fix),
        Firewall::Rules { udp, tcp } => {
            let missing: Vec<_> = [("udp", &udp), ("tcp", &tcp)]
                .iter()
                .filter(|(_, by)| by.is_empty())
                .map(|(p, _)| format!("{port}/{p}"))
                .collect();
            if missing.is_empty() {
                Check::pass(
                    name,
                    format!(
                        "{frontend} allows {port}/udp {} and {port}/tcp {}",
                        udp.join(", "),
                        tcp.join(", ")
                    ),
                )
            } else {
                Check::fail(
                    name,
                    format!("{frontend} has no rule allowing {}", missing.join(" or ")),
                    fix,
                )
            }
        }
    }
}

/// outcome of a DTLS handshake with a peer's input port
#[derive(Debug)]
enum Dtls {
    /// the peer accepted our certificate; its fingerprint
    Accepted(Option<String>),
    /// the peer answered but refused our certificate
    Rejected,
    NoAnswer(String),
}

async fn dtls_probe(addr: SocketAddr, cert: Certificate, wait: Duration) -> Dtls {
    let local: IpAddr = match addr {
        SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = match UdpSocket::bind((local, 0)).await {
        Ok(s) => s,
        Err(e) => return Dtls::NoAnswer(e.to_string()),
    };
    if let Err(e) = socket.connect(addr).await {
        return Dtls::NoAnswer(e.to_string());
    }
    // any certificate: the probe reports the fingerprint and the caller
    // decides whether it is trusted
    let (config, _) = crypto::peers::dtls_client_config(cert, Policy::Any);
    match timeout(wait, DTLSConn::new(Arc::new(socket), config, true, None)).await {
        Err(_) => Dtls::NoAnswer("timed out".into()),
        Ok(Ok(conn)) => {
            let fingerprint = conn
                .connection_state()
                .await
                .peer_certificates
                .first()
                .map(|c| crypto::generate_fingerprint(c));
            let _ = conn.close().await;
            Dtls::Accepted(fingerprint)
        }
        Ok(Err(webrtc_dtls::Error::ErrAlertFatalOrClose)) => Dtls::Rejected,
        Ok(Err(e)) => Dtls::NoAnswer(e.to_string()),
    }
}

fn tls_connector(cert: &Certificate) -> Result<TlsConnector, rustls::Error> {
    let config = crypto::peers::tls_client_config(cert, Policy::Any)?;
    Ok(TlsConnector::from(Arc::new(config)))
}

/// why a TLS probe got no certificate
enum TlsFailure {
    /// the TCP port is open but the handshake failed
    Refused(String),
    /// no TCP connection
    NoAnswer(String),
}

/// TLS handshake with a peer's clipboard / pairing port: its fingerprint
async fn tls_probe(addr: SocketAddr, connector: TlsConnector) -> Result<String, TlsFailure> {
    let stream = match timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(TlsFailure::NoAnswer(e.to_string())),
        Err(_) => return Err(TlsFailure::NoAnswer("timed out".into())),
    };
    let handshake = async {
        let name = ServerName::try_from("mimouse").expect("valid server name");
        let mut stream = connector
            .connect(name, stream)
            .await
            .map_err(|e| e.to_string())?;
        let fingerprint = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| crypto::generate_fingerprint(c))
            .ok_or("no certificate".to_owned());
        let _ = stream.shutdown().await;
        fingerprint
    };
    timeout(PROBE_TIMEOUT, handshake)
        .await
        .unwrap_or(Err("timed out".into()))
        .map_err(TlsFailure::Refused)
}

/// a client to check, from the service or (if it is not running) the config
struct Target {
    label: String,
    hostname: Option<String>,
    fix_ips: Vec<IpAddr>,
    port: u16,
    /// the certificate fingerprint outgoing connections accept
    pin: Option<String>,
}

async fn resolve(resolver: &Option<TokioResolver>, host: &str) -> Result<Vec<IpAddr>, String> {
    let Some(resolver) = resolver else {
        return Err("no DNS resolver".into());
    };
    resolver
        .lookup_ip(host)
        .await
        .map(|ips| ips.iter().collect())
        .map_err(|e| e.to_string())
}

/// fingerprint of `target`'s certificate as reported by a DTLS or TLS
/// handshake, if the handshakes agree
fn peer_fingerprint<'a>(
    dtls: &'a [(SocketAddr, Dtls)],
    tls: &'a [(SocketAddr, Result<String, TlsFailure>)],
) -> Option<&'a str> {
    dtls.iter()
        .find_map(|(_, d)| match d {
            Dtls::Accepted(Some(fp)) => Some(fp.as_str()),
            _ => None,
        })
        .or_else(|| {
            tls.iter()
                .find_map(|(_, r)| r.as_ref().ok().map(String::as_str))
        })
}

async fn client_checks(
    target: Target,
    resolver: &Option<TokioResolver>,
    cert: Option<&Certificate>,
    connector: Option<&TlsConnector>,
    own_fingerprint: Option<&str>,
    authorized: &HashMap<String, String>,
) -> Vec<Check> {
    let mut checks = vec![];
    let label = &target.label;
    let mut ips = target.fix_ips.clone();
    match &target.hostname {
        Some(host) => match resolve(resolver, host).await {
            Ok(resolved) => {
                checks.push(Check::pass(
                    format!("{label}: resolves"),
                    format!("{host} -> {resolved:?}"),
                ));
                ips.extend(resolved);
            }
            Err(e) if ips.is_empty() => checks.push(Check::fail(
                format!("{label}: resolves"),
                format!("{host}: {e}"),
                format!(
                    "check DNS for {host}, or pin its address: `mimouse cli set-ips <id> <ip>`"
                ),
            )),
            Err(e) => checks.push(Check::warn(
                format!("{label}: resolves"),
                format!("{host}: {e}; using the fixed ips {ips:?}"),
                format!("check DNS for {host}"),
            )),
        },
        None if ips.is_empty() => checks.push(Check::fail(
            format!("{label}: resolves"),
            "no hostname and no ips",
            "`mimouse cli set-host <id> <host>` or `mimouse cli set-ips <id> <ip>`",
        )),
        None => {}
    }
    let mut seen = std::collections::HashSet::new();
    ips.retain(|ip| seen.insert(*ip));
    if ips.is_empty() {
        return checks;
    }
    let addrs: Vec<SocketAddr> = ips
        .iter()
        .map(|ip| SocketAddr::new(*ip, target.port))
        .collect();
    let (Some(cert), Some(connector)) = (cert, connector) else {
        checks.push(Check::warn(
            format!("{label}: reachable"),
            "not checked: no local key",
            "start the service once; it creates the key",
        ));
        return checks;
    };
    let dtls = join_all(
        addrs
            .iter()
            .map(|&a| async move { (a, dtls_probe(a, cert.clone(), PROBE_TIMEOUT).await) }),
    )
    .await;
    let tls = join_all(
        addrs
            .iter()
            .map(|&a| async move { (a, tls_probe(a, connector.clone()).await) }),
    )
    .await;

    let answered: Vec<_> = dtls
        .iter()
        .filter(|(_, d)| !matches!(d, Dtls::NoAnswer(_)))
        .map(|(a, _)| a.to_string())
        .collect();
    if answered.is_empty() {
        let errors = dtls
            .iter()
            .map(|(a, d)| match d {
                Dtls::NoAnswer(e) => format!("{a}: {e}"),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        checks.push(Check::fail(
            format!("{label}: DTLS (input)"),
            format!("no answer from {errors}"),
            format!(
                "is mimouse running on {}? its firewall must allow {}/udp",
                target.hostname.as_deref().unwrap_or("the client"),
                target.port
            ),
        ));
    } else {
        checks.push(Check::pass(
            format!("{label}: DTLS (input)"),
            format!("answers on {}", answered.join(", ")),
        ));
    }
    let refused = tls.iter().find_map(|(a, r)| match r {
        Err(TlsFailure::Refused(e)) => Some((a, e)),
        _ => None,
    });
    match (tls.iter().find(|(_, r)| r.is_ok()), refused) {
        (Some((a, _)), _) => checks.push(Check::pass(
            format!("{label}: TLS (clipboard, pairing)"),
            format!("answers on {a}"),
        )),
        // a listener without clipboard sharing serves pairing only and
        // closes other connections
        (None, Some((a, e))) => checks.push(Check::pass(
            format!("{label}: TLS (clipboard, pairing)"),
            format!(
                "port open on {a}, handshake refused ({e}): clipboard sharing is off there or not authorized"
            ),
        )),
        (None, None) => {
            let errors = tls
                .iter()
                .filter_map(|(a, r)| match r {
                    Err(TlsFailure::NoAnswer(e)) => Some(format!("{a}: {e}")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", ");
            // older MiMouse versions open the TCP port only with clipboard
            // sharing on, so silence is not an error
            checks.push(Check::warn(
                format!("{label}: TLS (clipboard, pairing)"),
                format!("no answer from {errors}"),
                format!(
                    "needed for clipboard sharing and pairing: its firewall must allow {}/tcp (MiMouse before pairing support listens only with `clipboard = true`)",
                    target.port
                ),
            ));
        }
    }

    let name = format!("{label}: trusts this machine");
    if dtls.iter().any(|(_, d)| matches!(d, Dtls::Accepted(_))) {
        checks.push(Check::pass(name, "our certificate is authorized"));
    } else if dtls.iter().any(|(_, d)| matches!(d, Dtls::Rejected)) {
        checks.push(Check::fail(
            name,
            "our certificate was rejected",
            format!(
                "on {}: `mimouse cli authorize-key <name> {}` (or pair again)",
                target.hostname.as_deref().unwrap_or("the client"),
                own_fingerprint.unwrap_or("<fingerprint>")
            ),
        ));
    } else {
        checks.push(Check::warn(
            name,
            "unknown: no DTLS answer",
            "fix reachability first",
        ));
    }

    let name = format!("{label}: trusted here");
    let peer = peer_fingerprint(&dtls, &tls);
    match peer {
        Some(fp) => match authorized.get(fp) {
            Some(desc) => checks.push(Check::pass(name, format!("authorized as \"{desc}\""))),
            None => checks.push(Check::fail(
                name,
                format!("its fingerprint {fp} is not authorized"),
                format!(
                    "after comparing it with `mimouse cli watch` on the client: `mimouse cli authorize-key {} {fp}`",
                    target.hostname.as_deref().unwrap_or("<name>")
                ),
            )),
        },
        None => checks.push(Check::warn(
            name,
            "unknown: no handshake revealed its certificate",
            "fix reachability first",
        )),
    }
    checks.push(pin_check(label, target.pin.as_deref(), peer));
    checks
}

/// whether the certificate `peer` presented is the one outgoing connections
/// to the client are pinned to
fn pin_check(label: &str, pin: Option<&str>, peer: Option<&str>) -> Check {
    let name = format!("{label}: pinned certificate");
    match (pin, peer) {
        (Some(pin), Some(fp)) if pin == fp => Check::pass(name, format!("{pin} matches")),
        (Some(pin), Some(fp)) => Check::fail(
            name,
            format!("it presents {fp}, but {pin} is pinned: input and clipboard to it are refused"),
            "if it got a new key, compare the fingerprint with `mimouse cli watch` on the client, then pair again",
        ),
        (Some(pin), None) => Check::warn(
            name,
            format!("{pin}; unchecked: no handshake revealed its certificate"),
            "fix reachability first",
        ),
        (None, Some(fp)) => Check::warn(
            name,
            format!("none yet; its certificate {fp} is pinned on the first connection"),
            "activate it, or pair with it, to pin it",
        ),
        (None, None) => Check::warn(
            name,
            "none yet: pinned on the first connection",
            "fix reachability first",
        ),
    }
}

/// run all checks
pub async fn checks(config: &Config) -> Vec<Check> {
    let mut checks = vec![];
    let state = service_state().await;
    match &state {
        Ok(s) => checks.push(Check::pass(
            "service",
            format!("running, port {}", s.port.unwrap_or_default()),
        )),
        Err(e) => checks.push(Check::fail(
            "service",
            format!("not reachable over IPC: {e}"),
            start_service_fix(),
        )),
    }
    let state = state.ok();

    checks.push(config_check(config.config_path()));

    checks.push(key_check(config.cert_path()));

    let port = state.as_ref().and_then(|s| s.port).unwrap_or(config.port());
    let (frontend, fw, fix) = firewall(port).await;
    checks.push(firewall_check(frontend, fw, port, fix));

    if let Some(s) = &state {
        if let Some(capture) = &s.capture {
            checks.push(status_check("capture", capture, "enable-capture"));
        }
        if let Some(emulation) = &s.emulation {
            checks.push(status_check("emulation", emulation, "enable-emulation"));
        }
    }

    let cert = crypto::load_certificate(config.cert_path()).ok();
    let own_fingerprint = cert.as_ref().map(crypto::certificate_fingerprint);
    let connector = cert.as_ref().and_then(|c| tls_connector(c).ok());
    let authorized = state
        .as_ref()
        .and_then(|s| s.keys.clone())
        .unwrap_or_else(|| config.authorized_fingerprints());
    let targets: Vec<Target> = match state.as_ref().and_then(|s| s.clients.clone()) {
        Some(clients) => clients
            .into_iter()
            .filter(|c| c.state.active)
            .map(|c| Target {
                label: format!(
                    "client {} ({})",
                    c.id,
                    c.config.hostname.as_deref().unwrap_or("no hostname")
                ),
                hostname: c.config.hostname,
                fix_ips: c.config.fix_ips,
                port: c.config.port,
                pin: c.config.fingerprint,
            })
            .collect(),
        None => config
            .clients()
            .into_iter()
            .filter(|c| c.active)
            .map(|c| Target {
                label: format!(
                    "client {}",
                    c.hostname.as_deref().unwrap_or("without hostname")
                ),
                hostname: c.hostname,
                fix_ips: c.ips.into_iter().collect(),
                port: c.port,
                pin: c.fingerprint,
            })
            .collect(),
    };
    if targets.is_empty() {
        checks.push(Check::warn(
            "clients",
            "no active client",
            "add one: `mimouse cli pair <host> --position <side>`",
        ));
    }
    let resolver = TokioResolver::builder_tokio().ok().map(|b| b.build());
    let per_client = join_all(targets.into_iter().map(|t| {
        client_checks(
            t,
            &resolver,
            cert.as_ref(),
            connector.as_ref(),
            own_fingerprint.as_deref(),
            &authorized,
        )
    }))
    .await;
    checks.extend(per_client.into_iter().flatten());
    checks
}

/// `mimouse cli doctor`; the number of failed checks
pub async fn run(config: &Config, json: bool) -> usize {
    let checks = checks(config).await;
    let failed = checks.iter().filter(|c| c.status == Outcome::Fail).count();
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": failed == 0, "checks": checks })
        );
        return failed;
    }
    for check in &checks {
        let tag = match check.status {
            Outcome::Pass => "ok  ",
            Outcome::Warn => "warn",
            Outcome::Fail => "FAIL",
        };
        println!("{tag}  {}: {}", check.name, check.detail);
        if let Some(fix) = check.fix.as_ref().filter(|_| check.status != Outcome::Pass) {
            println!("      fix: {fix}");
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    const UFW: &str = "\
Status: active
Logging: on (low)
Default: deny (incoming), allow (outgoing), deny (routed)
New profiles: skip

To                         Action      From
--                         ------      ----
Anywhere on tailscale0     ALLOW IN    Anywhere                   # tailnet only
41641/udp                  ALLOW IN    Anywhere                   # tailscale direct connections
4242/udp                   ALLOW IN    192.168.50.0/24            # MiMouse
4242/tcp                   ALLOW IN    192.168.50.0/24            # MiMouse clipboard
Anywhere (v6) on tailscale0 ALLOW IN    Anywhere (v6)              # tailnet only
41641/udp (v6)             ALLOW IN    Anywhere (v6)              # tailscale direct connections
";

    #[test]
    fn ufw_rules() {
        assert_eq!(
            parse_ufw(UFW, 4242),
            Firewall::Rules {
                udp: vec!["on tailscale0".into(), "from 192.168.50.0/24".into()],
                tcp: vec!["on tailscale0".into(), "from 192.168.50.0/24".into()],
            }
        );
        // another port: only the interface rule allows it
        assert_eq!(
            parse_ufw(UFW, 4243),
            Firewall::Rules {
                udp: vec!["on tailscale0".into()],
                tcp: vec!["on tailscale0".into()],
            }
        );
        let plain = "\
Status: active

To                         Action      From
--                         ------      ----
22/tcp                     ALLOW       Anywhere
OpenSSH                    ALLOW       Anywhere
4000:4300/udp              ALLOW       10.0.0.0/8
4242                       DENY        Anywhere
192.168.1.5 4242/tcp       ALLOW       Anywhere
4242/udp                   ALLOW OUT   Anywhere
";
        assert_eq!(
            parse_ufw(plain, 4242),
            Firewall::Rules {
                udp: vec!["from 10.0.0.0/8".into()],
                tcp: vec!["from anywhere".into()],
            }
        );
        assert_eq!(
            parse_ufw(plain, 22),
            Firewall::Rules {
                udp: vec![],
                tcp: vec!["from anywhere".into()],
            }
        );
        assert!(matches!(
            parse_ufw("Status: inactive\n", 4242),
            Firewall::Off(_)
        ));
        let open =
            "Status: active\nDefault: allow (incoming), allow (outgoing), disabled (routed)\n";
        assert!(matches!(parse_ufw(open, 4242), Firewall::Off(_)));
        assert!(ufw_disabled("# comment\nENABLED=no\nLOGLEVEL=low\n"));
        assert!(!ufw_disabled("ENABLED=yes\n"));
    }

    #[test]
    fn firewalld_lists() {
        assert_eq!(firewalld_ports("4242/udp 4242/tcp", 4242), (true, true));
        assert_eq!(firewalld_ports("4000-4300/udp 80/tcp", 4242), (true, false));
        assert_eq!(firewalld_ports("42420/udp 424/tcp", 4242), (false, false));
        assert_eq!(firewalld_ports("", 4242), (false, false));
        let info = "mimouse\n  ports: 4242/udp 4242/tcp\n  protocols: \n  source-ports: 4242/udp\n";
        assert_eq!(firewalld_service_ports(info), " 4242/udp 4242/tcp");
        assert_eq!(
            firewalld_ports(firewalld_service_ports(info), 4242),
            (true, true)
        );
        assert_eq!(firewalld_service_ports("ssh\n  protocols: \n"), "");
    }

    const NETSH: &str = "
Rule Name:                            MiMouse
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Private,Public
Grouping:                             
LocalIP:                              Any
RemoteIP:                             LocalSubnet
Protocol:                             UDP
LocalPort:                            4242
RemotePort:                           Any
Edge traversal:                       No
Program:                              Any
Action:                               Allow

Rule Name:                            mimouse.exe
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Private
Grouping:                             
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             TCP
LocalPort:                            Any
RemotePort:                           Any
Edge traversal:                       Defer to user
Program:                              C:\\Users\\me\\bin\\MiMouse.exe
Action:                               Allow

Rule Name:                            other.exe
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Domain,Private,Public
Protocol:                             Any
LocalPort:                            Any
Program:                              C:\\Program Files\\other\\other.exe
Action:                               Allow

Rule Name:                            disabled
----------------------------------------------------------------------
Enabled:                              No
Direction:                            In
Profiles:                             Domain,Private,Public
Protocol:                             TCP
LocalPort:                            4000-4300
Action:                               Allow
Ok.
";

    #[test]
    fn netsh_rules() {
        assert_eq!(
            parse_netsh_rules(NETSH, 4242, "Private"),
            Some((
                vec!["rule \"MiMouse\"".to_owned()],
                vec!["rule \"mimouse.exe\"".to_owned()]
            ))
        );
        // the program rule does not cover the Public profile
        assert_eq!(
            parse_netsh_rules(NETSH, 4242, "Public"),
            Some((vec!["rule \"MiMouse\"".to_owned()], vec![]))
        );
        assert_eq!(
            parse_netsh_rules(NETSH, 4243, "Public"),
            Some((vec![], vec![]))
        );
        // localized output: nothing parsed is not "blocked"
        assert_eq!(
            parse_netsh_rules("Regelname: x\nOk.\n", 4242, "Public"),
            None
        );

        let state = "\nPrivate Profile Settings: \n----------------------------------------------------------------------\nState                                 ON\nOk.\n\n";
        assert_eq!(parse_netsh_state(state), Some(("Private".into(), true)));
        let off = "Public Profile Settings:\n------\nState                                 OFF\n";
        assert_eq!(parse_netsh_state(off), Some(("Public".into(), false)));
    }

    #[test]
    fn socketfilterfw_state() {
        assert_eq!(
            parse_socketfilterfw("Firewall is enabled. (State = 1)"),
            Some(true)
        );
        assert_eq!(
            parse_socketfilterfw("Firewall is disabled. (State = 0)"),
            Some(false)
        );
        assert_eq!(parse_socketfilterfw(""), None);
    }

    #[test]
    fn capture_status_from_events() {
        let enabled = serde_json::json!({"event": "capture", "enabled": true});
        assert_eq!(status_of(&enabled).as_deref(), Some("enabled"));
        let disabled = serde_json::json!({"event": "emulation", "enabled": false});
        assert_eq!(status_of(&disabled).as_deref(), Some("disabled"));
        let waiting = serde_json::json!({"event": "capture", "status": "awaiting_approval"});
        assert_eq!(status_of(&waiting).as_deref(), Some("awaiting_approval"));
        assert_eq!(
            status_check("capture", "awaiting_approval", "enable-capture").status,
            Outcome::Warn
        );
        assert_eq!(
            status_check("capture", "disabled", "enable-capture").status,
            Outcome::Fail
        );
    }

    #[test]
    fn invalid_config_names_the_last_good_copy() {
        let dir = std::env::temp_dir().join(format!("mimouse-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let last_good = config::last_good_path(&path);

        std::fs::write(&path, "port = 4242\n").unwrap();
        assert_eq!(config_check(&path).status, Outcome::Pass);

        std::fs::write(&path, "[[clients]]\nrange = [80, 20]\n").unwrap();
        let check = config_check(&path);
        assert_eq!(check.status, Outcome::Fail);
        assert!(check.detail.contains("invalid range"), "{}", check.detail);
        assert!(check.fix.unwrap().contains("no last good copy"));

        std::fs::write(&last_good, "port = 4242\n").unwrap();
        let fix = config_check(&path).fix.unwrap();
        assert!(fix.contains(&config::restore_command(&path)), "{fix}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A2: doctor shows the pin and fails on a mismatch
    #[test]
    fn pin_check_reports_mismatch() {
        let check = |pin, peer| pin_check("client 0", pin, peer);
        assert_eq!(check(Some("aa:01"), Some("aa:01")).status, Outcome::Pass);
        let mismatch = check(Some("aa:01"), Some("bb:02"));
        assert_eq!(mismatch.status, Outcome::Fail);
        assert!(mismatch.detail.contains("bb:02") && mismatch.detail.contains("aa:01"));
        let unpinned = check(None, Some("bb:02"));
        assert_eq!(unpinned.status, Outcome::Warn);
        assert!(unpinned.detail.contains("bb:02"));
        assert!(check(Some("aa:01"), None).detail.contains("aa:01"));
    }
}

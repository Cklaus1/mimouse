use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use tokio::task::JoinHandle;
use toml;
use toml_edit::{self, DocumentMut};

use crate::cli::CliArgs;
use crate::ipc::{ConfigReport, ConfigValue, DEFAULT_PORT, Position, RestartPending, Source};
use input_capture::Chord;
use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "mimouse.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/mimouse/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\mimouse\\")
    };
    Ok(PathBuf::from(default_path))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    release_double_tap_ctrl: Option<bool>,
    cert_path: Option<PathBuf>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, String>>,
    clipboard: Option<bool>,
    max_file_size: Option<u64>,
    notify_switch: Option<bool>,
    idle_inhibit: Option<bool>,
    local_hotkey: Option<Vec<scancode::Linux>>,
    telemetry: Option<bool>,
    telemetry_endpoint: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    activate_on_startup: Option<bool>,
    enter_hook: Option<String>,
    range: Option<[u8; 2]>,
    hotkey: Option<Vec<scancode::Linux>>,
    fingerprint: Option<String>,
}

impl ConfigToml {
    /// the file at `path` and its text, if it parses and validates
    fn load(path: &Path) -> Result<(ConfigToml, String), ConfigError> {
        let text = fs::read_to_string(path)?;
        let config: ConfigToml = toml::from_str(&text)?;
        config.validate()?;
        Ok((config.normalized(), text))
    }

    /// checks what serde can't: each client's `range` and `port`, client
    /// `hotkey`s distinct from each other, from `local_hotkey` and from
    /// `release_bind`, the listen `port`, `max_file_size` and
    /// `telemetry_endpoint`
    fn validate(&self) -> Result<(), ConfigError> {
        if self.port == Some(0) {
            return Err(ConfigError::Port { client: None });
        }
        if self.max_file_size == Some(0) {
            return Err(ConfigError::MaxFileSize);
        }
        // empty: the built-in endpoint
        if let Some(url) = self.telemetry_endpoint.as_deref().filter(|u| !u.is_empty()) {
            crate::telemetry::endpoint(Some(url)).map_err(ConfigError::TelemetryEndpoint)?;
        }
        let release_bind = match &self.release_bind {
            Some(keys) => (keys.as_slice(), "`release_bind`"),
            None => (DEFAULT_RELEASE_KEYS.as_slice(), "`release_bind` (default)"),
        };
        // chords as capture matches them: order and repeats don't count
        let mut taken: Vec<(Chord, String)> = [
            Some(release_bind),
            self.local_hotkey.as_deref().map(|k| (k, "`local_hotkey`")),
        ]
        .into_iter()
        .flatten()
        .filter_map(|(keys, name)| Some((Chord::new(keys.iter().copied())?, name.to_owned())))
        .collect();
        for (i, c) in self.clients.iter().flatten().enumerate() {
            if let Some(range) = c.range {
                if input_capture::EdgeRange::new(range[0], range[1]).is_none() {
                    let client = c.name(i);
                    return Err(ConfigError::Range { client, range });
                }
            }
            if c.port == Some(0) {
                let client = Some(c.name(i));
                return Err(ConfigError::Port { client });
            }
            let Some(hotkey) = &c.hotkey else {
                continue;
            };
            let Some(chord) = Chord::new(hotkey.iter().copied()) else {
                continue;
            };
            if let Some((_, other)) = taken.iter().find(|(taken, _)| *taken == chord) {
                return Err(ConfigError::Hotkey {
                    client: c.name(i),
                    hotkey: hotkey.clone(),
                    other: other.clone(),
                });
            }
            taken.push((chord, format!("the `hotkey` of client {}", c.name(i))));
        }
        Ok(())
    }

    /// as the service holds it: clients through `ConfigClient`, empty
    /// sections dropped, so base, disk and memory compare equal when unchanged
    fn normalized(mut self) -> Self {
        self.clients = self
            .clients
            .map(|c| {
                c.into_iter()
                    .map(|c| TomlClient::from(ConfigClient::from(c)))
                    .collect::<Vec<_>>()
            })
            .filter(|c| !c.is_empty());
        self.authorized_fingerprints = self.authorized_fingerprints.filter(|k| !k.is_empty());
        self
    }

    /// whether clipboard sharing is enabled (default: disabled)
    fn clipboard(&self) -> bool {
        self.clipboard.unwrap_or(false)
    }

    /// largest total size of copied files sent to a client (default: 100 MiB)
    fn max_file_size(&self) -> u64 {
        self.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE)
    }
}

impl TomlClient {
    /// how errors name the client at index `i`: its host name, else `#i`
    fn name(&self, i: usize) -> String {
        self.hostname
            .clone()
            .or(self.host_name.clone())
            .unwrap_or_else(|| format!("#{i}"))
    }
}

/// whether the config file at `path` parses and validates
pub fn check_file(path: &Path) -> Result<(), ConfigError> {
    ConfigToml::load(path).map(|_| ())
}

/// `config.toml.last-good`: the file as last loaded or saved successfully
pub fn last_good_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".last-good");
    name.into()
}

/// shell command that restores the last good copy of the config at `path`
pub fn restore_command(path: &Path) -> String {
    let last_good = last_good_path(path);
    #[cfg(windows)]
    let copy = "copy";
    #[cfg(not(windows))]
    let copy = "cp";
    format!("{copy} \"{}\" \"{}\"", last_good.display(), path.display())
}

#[derive(Parser, Debug)]
#[command(
    author,
    version = build::CLAP_LONG_VERSION,
    about,
    long_about = None,
    after_help = "MiMouse is originally forked from Lan Mouse (https://github.com/feschber/lan-mouse) by Ferdinand Schober, GPL-3.0-or-later."
)]
struct Args {
    /// the listen port for mimouse
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// MiMouse commandline interface
    Cli(CliArgs),
    /// run in daemon mode
    Daemon,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(libei_capture)]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(layer_shell_capture)]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(x11_capture)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11_capture)]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(x11_capture)]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(wlroots_emulation)]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(libei_emulation)]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(rdp_emulation)]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(x11_emulation)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => Self::X11,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the config in effect (normalized)
    config_toml: ConfigToml,
    /// the config the service started with: restart-only keys keep its
    /// values until the next start
    started: ConfigToml,
    /// the port the service listens on and where it comes from
    port: u16,
    port_source: Source,
    /// `cert_path` when neither flag nor file sets it
    default_cert_path: PathBuf,
    /// the snapshot `config_toml` was loaded from, the base of the
    /// three-way merge on save; the last good copy (or the defaults) while
    /// the file is invalid
    base: ConfigToml,
    /// a reload of the file in flight, kept across cancelled `changed()` calls
    loading: Option<JoinHandle<Result<ConfigToml, ConfigError>>>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub enter_hook: Option<String>,
    /// the part of the edge that switches to this client, in percent
    pub range: Option<[u8; 2]>,
    /// chord that switches control to this client
    pub hotkey: Option<Vec<scancode::Linux>>,
    /// certificate fingerprint outgoing connections accept (`None`: trust
    /// the first one seen)
    pub fingerprint: Option<String>,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
        let range = toml.range;
        let hotkey = toml.hotkey;
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        Self {
            ips,
            hostname,
            port,
            pos,
            active,
            enter_hook,
            range,
            hotkey,
            fingerprint: toml.fingerprint,
        }
    }
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let enter_hook = client.enter_hook;
        let range = client.range;
        let hotkey = client.hotkey;
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            enter_hook,
            range,
            hotkey,
            fingerprint: client.fingerprint,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
    #[error("client {client}: invalid range {range:?}: need [start, end] with start < end <= 100")]
    Range { client: String, range: [u8; 2] },
    #[error("{}`port` = 0: need a port in 1..=65535", client.as_ref().map(|c| format!("client {c}: ")).unwrap_or_default())]
    Port { client: Option<String> },
    #[error("client {client}: `hotkey` {hotkey:?} is also {other}")]
    Hotkey {
        client: String,
        hotkey: Vec<scancode::Linux>,
        other: String,
    },
    #[error("`max_file_size` = 0: need at least 1 (bytes of copied files sent at once)")]
    MaxFileSize,
    #[error("`telemetry_endpoint`: {0}")]
    TelemetryEndpoint(String),
    #[error("{} is invalid, fix it first (`mimouse cli doctor` shows the last good copy): {error}", path.display())]
    Invalid { path: PathBuf, error: String },
    #[error("`{key}` changed both in {} and in the service; retry once the service has reloaded the file", path.display())]
    Conflict { path: PathBuf, key: String },
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

/// default `max_file_size`: 100 MiB, Mouse Without Borders' limit
const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// ways to return control to the capturing machine
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseBind {
    /// chord: exactly these keys held at once
    pub keys: Vec<scancode::Linux>,
    /// Ctrl tapped twice in quick succession
    pub double_tap_ctrl: bool,
}

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        Self::from_args(Args::parse())
    }

    /// the config at `path`, as `mimouse --config <path>` loads it
    #[cfg(test)]
    pub(crate) fn at(path: &Path) -> Result<Self, ConfigError> {
        Self::from_args(Args::parse_from([
            "mimouse".as_ref(),
            "--config".as_ref(),
            path.as_os_str(),
        ]))
    }

    fn from_args(args: Args) -> Result<Self, ConfigError> {
        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present, on every entry path (service, CLI, test commands),
        // so notify::Watcher (which requires the dir to exist on macOS FSEvents
        // and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::load(&config_path) {
            Ok((config, text)) => {
                write_last_good(&config_path, &text);
                config
            }
            Err(e) => fallback(&config_path, e),
        };

        // --cert-path <file> overrules default location
        let default_cert_path = default_path()?.join(CERT_FILE_NAME);
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.cert_path.clone())
            .unwrap_or_else(|| default_cert_path.clone());
        let (port, port_source) = match (args.port, config_toml.port) {
            (Some(port), _) => (port, Source::Flag),
            (None, Some(port)) => (port, Source::File),
            (None, None) => (DEFAULT_PORT, Source::Default),
        };

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )?;
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            base: config_toml.clone(),
            started: config_toml.clone(),
            port,
            port_source,
            default_cert_path,
            config_toml,
            loading: None,
            watcher,
            watch_rx,
        };
        config.watch()?;
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        self.watcher
            .watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    /// waits until the file changed into a different valid config, which is
    /// then in effect. An invalid file keeps the running config. Cancel
    /// safe: a load in flight is picked up by the next call
    pub async fn changed(&mut self) {
        loop {
            if let Some(load) = &mut self.loading {
                let result = load
                    .await
                    .map_err(|e| ConfigError::Io(io::Error::other(e)))
                    .and_then(|r| r);
                self.loading = None;
                match result {
                    Ok(config) if config != self.config_toml => {
                        self.base = config.clone();
                        self.config_toml = config;
                        return;
                    }
                    Ok(config) => self.base = config,
                    Err(e) => log::warn!(
                        "{:?}: {e}; keeping the running config and refusing to save until the file is fixed; restore the last good copy with `{}`",
                        self.config_path,
                        restore_command(&self.config_path)
                    ),
                }
                continue;
            }
            let event = match self.watch_rx.recv().await.expect("channel closed") {
                Ok(event) => event,
                // says nothing about the file: keep the running config
                Err(e) => {
                    log::warn!("config watcher: {e}");
                    continue;
                }
            };
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                log::info!("reading config from {:?}", self.config_path);
                let path = self.config_path.clone();
                self.loading = Some(tokio::task::spawn_blocking(move || {
                    let (config, text) = ConfigToml::load(&path)?;
                    write_last_good(&path, &text);
                    Ok(config)
                }));
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// directory of the config file (also holds the audit log)
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// public key fingerprints authorized for connection
    pub fn authorized_fingerprints(&self) -> HashMap<String, String> {
        self.config_toml
            .authorized_fingerprints
            .clone()
            .unwrap_or_default()
    }

    /// whether clipboard sharing is enabled (default: disabled); a change
    /// takes effect at the next start
    pub fn clipboard(&self) -> bool {
        self.started.clipboard()
    }

    /// largest total size of copied files sent to a client (default:
    /// 100 MiB); a change takes effect at the next start
    pub fn max_file_size(&self) -> u64 {
        self.started.max_file_size()
    }

    /// whether a notification announces each switch of control (default: enabled)
    pub fn notify_switch(&self) -> bool {
        self.config_toml.notify_switch.unwrap_or(true)
    }

    /// whether idle/lock is inhibited while this machine controls another one
    /// (default: enabled)
    pub fn idle_inhibit(&self) -> bool {
        self.config_toml.idle_inhibit.unwrap_or(true)
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args.capture_backend.or(self.started.capture_backend)
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.started.emulation_backend)
    }

    /// the port the service listens on: flag, else file, else default at
    /// the start, then the one `set_port` reported
    pub fn port(&self) -> u16 {
        self.port
    }

    /// the port the service now listens on, written by the next `save`;
    /// a `--port` flag still takes precedence at the next start
    pub fn set_port(&mut self, port: u16) {
        self.port = port;
        self.port_source = Source::Cli;
        self.config_toml.port = Some(port);
    }

    /// whether `set_port(port)` keeps the config valid; checked before the
    /// listener rebinds, since the save comes after
    pub fn check_port(&self, port: u16) -> Result<(), ConfigError> {
        let config = ConfigToml {
            port: Some(port),
            ..self.config_toml.clone()
        };
        config.validate()
    }

    /// each key's value in effect and where it comes from, and the
    /// restart-only keys changed in the file
    pub fn report(&self) -> ConfigReport {
        let (args, file, started) = (&self.args, &self.config_toml, &self.started);
        let release_bind = self.release_bind();
        let fingerprints: BTreeMap<_, _> = self.authorized_fingerprints().into_iter().collect();
        let keys = vec![
            entry(
                "capture_backend",
                json(self.capture_backend()),
                source(args.capture_backend, &started.capture_backend),
                true,
            ),
            entry(
                "emulation_backend",
                json(self.emulation_backend()),
                source(args.emulation_backend, &started.emulation_backend),
                true,
            ),
            entry("port", json(self.port), self.port_source, true),
            entry(
                "release_bind",
                json(release_bind.keys),
                source(None::<()>, &file.release_bind),
                false,
            ),
            entry(
                "release_double_tap_ctrl",
                json(release_bind.double_tap_ctrl),
                source(None::<()>, &file.release_double_tap_ctrl),
                false,
            ),
            entry(
                "cert_path",
                json(&self.cert_path),
                source(args.cert_path.as_ref(), &started.cert_path),
                true,
            ),
            entry(
                "clients",
                json(file.clients.as_deref().unwrap_or_default()),
                source(None::<()>, &file.clients),
                false,
            ),
            entry(
                "authorized_fingerprints",
                json(fingerprints),
                source(None::<()>, &file.authorized_fingerprints),
                false,
            ),
            entry(
                "clipboard",
                json(self.clipboard()),
                source(None::<()>, &started.clipboard),
                true,
            ),
            entry(
                "max_file_size",
                json(self.max_file_size()),
                source(None::<()>, &started.max_file_size),
                true,
            ),
            entry(
                "notify_switch",
                json(self.notify_switch()),
                source(None::<()>, &file.notify_switch),
                false,
            ),
            entry(
                "idle_inhibit",
                json(self.idle_inhibit()),
                source(None::<()>, &file.idle_inhibit),
                false,
            ),
            entry(
                "local_hotkey",
                json(self.local_hotkey()),
                source(None::<()>, &file.local_hotkey),
                false,
            ),
            entry(
                "telemetry",
                json(self.telemetry()),
                source(None::<()>, &file.telemetry),
                false,
            ),
            entry(
                "telemetry_endpoint",
                json(
                    crate::telemetry::endpoint(self.telemetry_endpoint())
                        .ok()
                        .map(|e| e.to_string()),
                ),
                source(None::<()>, &file.telemetry_endpoint),
                false,
            ),
        ];
        ConfigReport {
            path: self.config_path.clone(),
            keys,
            awaiting_restart: self.awaiting_restart(),
        }
    }

    /// restart-only keys whose value in the file (as last loaded) differs
    /// from the one in effect; a key set by a flag ignores the file
    pub fn awaiting_restart(&self) -> Vec<RestartPending> {
        let (args, disk, started) = (&self.args, &self.base, &self.started);
        let disk_cert = disk.cert_path.as_ref().unwrap_or(&self.default_cert_path);
        let keys = [
            (
                "capture_backend",
                args.capture_backend.is_some(),
                json(started.capture_backend),
                json(disk.capture_backend),
            ),
            (
                "emulation_backend",
                args.emulation_backend.is_some(),
                json(started.emulation_backend),
                json(disk.emulation_backend),
            ),
            (
                "port",
                args.port.is_some(),
                json(self.port),
                json(disk.port.unwrap_or(DEFAULT_PORT)),
            ),
            (
                "cert_path",
                args.cert_path.is_some(),
                json(&self.cert_path),
                json(disk_cert),
            ),
            (
                "clipboard",
                false,
                json(started.clipboard()),
                json(disk.clipboard()),
            ),
            (
                "max_file_size",
                false,
                json(started.max_file_size()),
                json(disk.max_file_size()),
            ),
        ];
        keys.into_iter()
            .filter(|(_, flagged, in_effect, on_disk)| !flagged && in_effect != on_disk)
            .map(|(key, _, in_effect, on_disk)| RestartPending {
                key: key.to_owned(),
                in_effect,
                on_disk,
            })
            .collect()
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .clients
            .clone()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> ReleaseBind {
        ReleaseBind {
            keys: self
                .config_toml
                .release_bind
                .clone()
                .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned())),
            double_tap_ctrl: self.config_toml.release_double_tap_ctrl.unwrap_or(true),
        }
    }

    /// chord that returns control to this machine from any client
    pub fn local_hotkey(&self) -> Option<Vec<scancode::Linux>> {
        self.config_toml.local_hotkey.clone()
    }

    /// whether usage statistics are sent (default: disabled); takes effect
    /// on reload
    pub fn telemetry(&self) -> bool {
        self.config_toml.telemetry.unwrap_or(false)
    }

    /// base URL of the telemetry collector, overriding the built-in one
    pub fn telemetry_endpoint(&self) -> Option<&str> {
        self.config_toml.telemetry_endpoint.as_deref()
    }

    /// saves `telemetry = enabled` like `save` saves the clients; a failed
    /// save leaves the config in effect unchanged
    pub async fn save_telemetry(
        &mut self,
        enabled: bool,
        clients: Vec<ConfigClient>,
        authorized: HashMap<String, String>,
    ) -> Result<bool, ConfigError> {
        let previous = self.config_toml.telemetry.replace(enabled);
        let result = self.save(clients, authorized).await;
        if result.is_err() {
            self.config_toml.telemetry = previous;
        }
        result
    }

    /// writes the config with the runtime's `clients` and `authorized` keys
    /// back to the file as a three-way merge: keys changed only in memory or
    /// only on disk (since `base`) both survive; a key changed on both sides,
    /// or a file that does not parse or validate, refuses the save and leaves
    /// the file untouched. Returns whether the file held changes the runtime
    /// lacks; they are now in effect here, so the runtime has to reload
    pub async fn save(
        &mut self,
        clients: Vec<ConfigClient>,
        authorized: HashMap<String, String>,
    ) -> Result<bool, ConfigError> {
        log::info!("writing config to {:?}", self.config_path);
        let mut memory = self.config_toml.clone();
        memory.clients = Some(clients.into_iter().map(TomlClient::from).collect());
        memory.authorized_fingerprints = Some(authorized);
        let memory = memory.normalized();
        let (path, base, mem) = (self.config_path.clone(), self.base.clone(), memory.clone());
        let merged = tokio::task::spawn_blocking(move || write_merged(&path, &base, &mem))
            .await
            .map_err(io::Error::other)??;
        let reload = merged != memory;
        // a reload in flight read the file before this merge took it in;
        // applying it later would undo the in-memory change just saved
        self.loading = None;
        self.base = merged.clone();
        self.config_toml = merged;
        Ok(reload)
    }
}

fn json(value: impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("config serialization")
}

/// where a key's value comes from: the `flag`, else the `file`, else the
/// default
fn source<F, T>(flag: Option<F>, file: &Option<T>) -> Source {
    match (flag, file) {
        (Some(_), _) => Source::Flag,
        (None, Some(_)) => Source::File,
        (None, None) => Source::Default,
    }
}

fn entry(key: &str, value: serde_json::Value, source: Source, restart: bool) -> ConfigValue {
    ConfigValue {
        key: key.to_owned(),
        value,
        source,
        restart,
    }
}

/// what to run on while the file at `path` is invalid at startup: its last
/// good copy, else the defaults. Saving stays refused until the file is
/// fixed, since `write_merged` never merges into an invalid file
fn fallback(path: &Path, error: ConfigError) -> ConfigToml {
    log::warn!("{path:?}: {error}");
    let last_good = last_good_path(path);
    match ConfigToml::load(&last_good) {
        Ok((config, _)) => {
            log::warn!(
                "running on the last good config {last_good:?}; changes are not saved until {path:?} is fixed; restore it with `{}`",
                restore_command(path)
            );
            config
        }
        Err(e) => {
            log::warn!(
                "no last good config ({last_good:?}: {e}); running on defaults, changes are not saved until {path:?} is fixed"
            );
            ConfigToml::default()
        }
    }
}

/// blocking part of `Config::save`: merges `memory`, loaded from `base`,
/// into the file at `path` and replaces the file with the result
fn write_merged(
    path: &Path,
    base: &ConfigToml,
    memory: &ConfigToml,
) -> Result<ConfigToml, ConfigError> {
    let target = resolve_symlinks(path)?;
    let invalid = |error: String| ConfigError::Invalid {
        path: path.to_owned(),
        error,
    };
    let (mut doc, merged) = match fs::read_to_string(&target) {
        // nothing on disk to keep
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            memory.validate()?;
            (DocumentMut::new(), memory.clone())
        }
        Err(e) => return Err(e.into()),
        Ok(current) => {
            let doc = current
                .parse::<DocumentMut>()
                .map_err(|e| invalid(e.to_string()))?;
            let disk = toml_edit::de::from_document::<ConfigToml>(doc.clone())
                .map_err(|e| invalid(e.to_string()))?;
            disk.validate().map_err(|e| invalid(e.to_string()))?;
            let disk = disk.normalized();
            let merged = merge3(
                "",
                Some(&to_value(base)),
                Some(&to_value(&disk)),
                Some(&to_value(memory)),
            )
            .map_err(|key| ConfigError::Conflict {
                path: path.to_owned(),
                key,
            })?
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
            let merged = merged
                .try_into::<ConfigToml>()
                .map_err(|e| invalid(e.to_string()))?
                .normalized();
            merged.validate()?;
            if merged == disk {
                write_last_good(path, &current);
                return Ok(merged);
            }
            (doc, merged)
        }
    };
    merge_config(&mut doc, &merged).map_err(|e| invalid(e.to_string()))?;
    let text = doc.to_string();
    replace_file(&target, text.as_bytes(), false)?;
    write_last_good(path, &text);
    Ok(merged)
}

fn to_value(config: &ConfigToml) -> toml::Value {
    toml::Value::try_from(config).expect("config serialization")
}

/// three-way merge of the value at `key`: a side that kept `base` takes the
/// other side's change, tables merge per key. Err: the dotted key that
/// changed differently on both sides
fn merge3(
    key: &str,
    base: Option<&toml::Value>,
    disk: Option<&toml::Value>,
    memory: Option<&toml::Value>,
) -> Result<Option<toml::Value>, String> {
    if disk == memory || memory == base {
        return Ok(disk.cloned());
    }
    if disk == base {
        return Ok(memory.cloned());
    }
    let (Some(toml::Value::Table(disk)), Some(toml::Value::Table(memory))) = (disk, memory) else {
        return Err(key.to_owned());
    };
    let empty = toml::Table::new();
    let base = match base {
        Some(toml::Value::Table(base)) => base,
        _ => &empty,
    };
    let keys: BTreeSet<&String> = base
        .keys()
        .chain(disk.keys())
        .chain(memory.keys())
        .collect();
    let mut merged = toml::Table::new();
    for k in keys {
        let path = if key.is_empty() {
            k.clone()
        } else {
            format!("{key}.{k}")
        };
        if let Some(v) = merge3(&path, base.get(k), disk.get(k), memory.get(k))? {
            merged.insert(k.clone(), v);
        }
    }
    Ok(Some(toml::Value::Table(merged)))
}

/// `path` with symlinks followed, dangling ones too: renaming onto the link
/// itself would replace it with a regular file
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut path = path.to_owned();
    // like the kernel's limit, against link cycles
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = fs::read_link(&path)?;
                path = match path.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => return Ok(path),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// replaces `target` (not a symlink) with `content` atomically: a temp file
/// in its directory, renamed over it. Keeps the mode of an existing target;
/// `private` makes the file owner-only instead. A read-only target is
/// refused, like writing it in place would be
pub(crate) fn replace_file(target: &Path, content: &[u8], private: bool) -> io::Result<()> {
    let permissions = match fs::metadata(target) {
        Ok(m) => Some(m.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if permissions.as_ref().is_some_and(|p| p.readonly()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is read-only", target.display()),
        ));
    }
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{}: not a file", target.display())))?;
    let tmp = target.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    let write = || -> io::Result<()> {
        {
            let mut f = if private {
                crate::crypto::create_private(&tmp)?
            } else {
                File::create(&tmp)?
            };
            f.write_all(content)?;
            f.sync_all()?;
        }
        if let (false, Some(permissions)) = (private, permissions) {
            fs::set_permissions(&tmp, permissions)?;
        }
        fs::rename(&tmp, target)
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `config.toml.last-good` (owner-only: it holds fingerprints) from the
/// valid `text` of the config at `path`; failures are only logged
fn write_last_good(path: &Path, text: &str) {
    let last_good = last_good_path(path);
    if fs::read_to_string(&last_good).is_ok_and(|s| s == text) {
        return;
    }
    if let Err(e) = replace_file(&last_good, text.as_bytes(), true) {
        log::warn!("{last_good:?}: {e}");
    }
}

/// update `doc` in place so it describes `config` (normalized): only keys
/// whose value differs are touched, so comments, ordering and unknown keys
/// survive. Fails if `doc` is not a valid config: every key would count as
/// changed
fn merge_config(doc: &mut DocumentMut, config: &ConfigToml) -> Result<(), toml_edit::de::Error> {
    let old = toml_edit::de::from_document::<ConfigToml>(doc.clone())?.normalized();
    let mut old = to_table(&old);
    let mut new = to_table(config);
    let old_clients = old.remove("clients");
    let new_clients = new.remove("clients");
    merge_table(doc.as_table_mut(), &old, &new);
    if semantic(old_clients.as_ref()) != semantic(new_clients.as_ref()) {
        merge_clients(
            doc.as_table_mut(),
            config.clients.as_deref().unwrap_or_default(),
        );
    }
    Ok(())
}

/// `[[clients]]`: an existing entry with the same hostname and position is
/// edited in place; failing that, one with the same hostname, ips or
/// position (a moved or renamed client keeps its comments and unknown
/// keys). Unmatched entries are removed, new ones appended
fn merge_clients(doc: &mut toml_edit::Table, clients: &[TomlClient]) {
    let mut existing = match doc.remove("clients") {
        Some(toml_edit::Item::ArrayOfTables(a)) => a.into_iter().map(Some).collect(),
        Some(toml_edit::Item::Value(toml_edit::Value::Array(a))) => a
            .into_iter()
            .map(|v| match v {
                toml_edit::Value::InlineTable(t) => Some(t.into_table()),
                _ => None,
            })
            .collect(),
        _ => Vec::<Option<toml_edit::Table>>::new(),
    };
    if clients.is_empty() {
        return;
    }
    let parsed: Vec<Option<TomlClient>> = existing
        .iter()
        .map(|t| t.as_ref().and_then(table_client))
        .collect();
    fn position(c: &TomlClient) -> Position {
        c.position.unwrap_or_default()
    }
    let same: [fn(&TomlClient, &TomlClient) -> bool; 4] = [
        |a, b| a.hostname == b.hostname && position(a) == position(b),
        |a, b| a.hostname.is_some() && a.hostname == b.hostname,
        |a, b| a.ips.as_ref().is_some_and(|ips| !ips.is_empty()) && a.ips == b.ips,
        |a, b| position(a) == position(b),
    ];
    let mut matched: Vec<Option<usize>> = vec![None; clients.len()];
    let mut taken = vec![false; existing.len()];
    for same in same {
        for (client, m) in clients.iter().zip(&mut matched) {
            if m.is_some() {
                continue;
            }
            *m = (0..existing.len())
                .find(|&i| !taken[i] && parsed[i].as_ref().is_some_and(|old| same(client, old)));
            if let Some(i) = *m {
                taken[i] = true;
            }
        }
    }
    let mut result = toml_edit::ArrayOfTables::new();
    for (client, m) in clients.iter().zip(matched) {
        let new = to_table(client);
        match m.and_then(|i| Some((existing[i].take()?, parsed[i].clone()?))) {
            Some((mut table, old)) => {
                let old = TomlClient::from(ConfigClient::from(old));
                merge_table(&mut table, &to_table(&old), &new);
                result.push(table);
            }
            None => result.push(new),
        }
    }
    doc.insert("clients", toml_edit::Item::ArrayOfTables(result));
}

fn table_client(t: &toml_edit::Table) -> Option<TomlClient> {
    toml_edit::de::from_document(DocumentMut::from(t.clone())).ok()
}

/// apply the difference between the known keys `old` and `new` to `dst`;
/// keys known to neither are left alone
fn merge_table(dst: &mut toml_edit::Table, old: &toml_edit::Table, new: &toml_edit::Table) {
    let keys = old
        .iter()
        .chain(new.iter())
        .map(|(k, _)| k.to_owned())
        .collect::<Vec<_>>();
    for k in keys {
        let (old, new) = (old.get(&k), new.get(&k));
        if semantic(old) == semantic(new) {
            continue;
        }
        let Some(new) = new else {
            dst.remove(&k);
            continue;
        };
        match (dst.get_mut(&k), new) {
            (Some(toml_edit::Item::Table(d)), n) if n.is_table_like() => {
                let n = n.clone().into_table().expect("table like");
                let o = old.cloned().and_then(|o| o.into_table().ok());
                merge_table(d, &o.unwrap_or_default(), &n);
            }
            (Some(toml_edit::Item::Value(d)), toml_edit::Item::Value(n)) => {
                let decor = d.decor().clone();
                *d = n.clone();
                *d.decor_mut() = decor;
            }
            (Some(d), n) => *d = n.clone(),
            (None, n) => {
                dst.insert(&k, n.clone());
            }
        }
    }
}

/// serialize into a standalone table
fn to_table<T: Serialize>(value: &T) -> toml_edit::Table {
    toml_edit::ser::to_document(value)
        .expect("config serialization")
        .as_table()
        .clone()
}

/// value of an item independent of formatting
fn semantic(item: Option<&toml_edit::Item>) -> Option<toml::Value> {
    let mut doc = DocumentMut::new();
    doc.insert("v", item?.clone());
    toml::from_str::<toml::Table>(&doc.to_string())
        .ok()?
        .remove("v")
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED: &str = r#"# my mimouse config
port = 4242 # not the default
release_bind = ["KeyA", "KeyB"]
unknown_key = "kept"

[authorized_fingerprints]
# the laptop
"aa:bb" = "laptop"

# the desktop on the left
[[clients]]
hostname = "desk"
position = "left"
ips = ["192.168.0.2"]
activate_on_startup = true

# right screen
[[clients]]
hostname = "lap" # trailing
position = "right"
port = 4243
"#;

    /// the document and the config as the service holds it
    fn parsed(s: &str) -> (DocumentMut, ConfigToml) {
        let doc = s.parse::<DocumentMut>().unwrap();
        let cfg = toml_edit::de::from_document::<ConfigToml>(doc.clone()).unwrap();
        (doc, cfg.normalized())
    }

    #[test]
    fn noop_save_is_identical() {
        let (mut doc, cfg) = parsed(COMMENTED);
        merge_config(&mut doc, &cfg).unwrap();
        assert_eq!(doc.to_string(), COMMENTED);
    }

    #[test]
    fn changing_one_client_keeps_rest() {
        let (mut doc, mut cfg) = parsed(COMMENTED);
        cfg.clients.as_mut().unwrap()[1].port = Some(5000);
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert_eq!(out, COMMENTED.replace("port = 4243", "port = 5000"));
        assert_eq!(parsed(&out).1, cfg);
    }

    #[test]
    fn add_and_remove_client() {
        let (mut doc, mut cfg) = parsed(COMMENTED);
        let clients = cfg.clients.as_mut().unwrap();
        clients.remove(0);
        clients.push(TomlClient {
            hostname: Some("new".into()),
            host_name: None,
            ips: Some(vec!["10.0.0.1".parse().unwrap()]),
            port: None,
            position: Some(Position::Top),
            activate_on_startup: None,
            enter_hook: None,
            range: Some([0, 100]),
            hotkey: Some(vec![KeyLeftCtrl, KeyLeftAlt, scancode::Linux::Key1]),
            fingerprint: None,
        });
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert!(!out.contains("desk"), "{out}");
        assert!(out.contains("# my mimouse config\nport = 4242 # not the default"));
        assert!(out.contains("# right screen\n[[clients]]\nhostname = \"lap\" # trailing"));
        assert!(out.contains("unknown_key = \"kept\""));
        assert!(out.contains("# the laptop\n\"aa:bb\" = \"laptop\""));
        let (_, back) = parsed(&out);
        assert_eq!(back, cfg);

        // and removing every client drops the section
        cfg.clients = None;
        merge_config(&mut doc, &cfg).unwrap();
        assert!(!doc.to_string().contains("clients"));
    }

    #[test]
    fn top_level_change_keeps_decor() {
        let (mut doc, mut cfg) = parsed(COMMENTED);
        cfg.port = Some(1234);
        cfg.authorized_fingerprints
            .as_mut()
            .unwrap()
            .insert("cc:dd".into(), "phone".into());
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert!(out.contains("port = 1234 # not the default"), "{out}");
        assert!(
            out.contains("# the laptop\n\"aa:bb\" = \"laptop\"\n"),
            "{out}"
        );
        assert_eq!(parsed(&out).1, cfg);
    }

    #[test]
    fn fresh_file() {
        let (_, cfg) = parsed(COMMENTED);
        let mut doc = DocumentMut::new();
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert!(out.contains("[[clients]]"), "{out}");
        assert_eq!(parsed(&out).1, cfg);
    }

    fn with_range(range: &str) -> Result<ConfigToml, ConfigError> {
        let s =
            format!("[[clients]]\nhostname = \"megapc\"\nposition = \"left\"\nrange = {range}\n");
        let cfg: ConfigToml = toml::from_str(&s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn range_bounds() {
        for ok in ["[0, 100]", "[0, 1]", "[99, 100]", "[20, 80]"] {
            assert!(with_range(ok).is_ok(), "{ok}");
        }
        let cfg = with_range("[20, 80]").unwrap();
        let client = ConfigClient::from(cfg.clients.unwrap()[0].clone());
        assert_eq!(client.range, Some([20, 80]));
        for bad in ["[50, 50]", "[80, 20]", "[0, 101]"] {
            let e = with_range(bad).unwrap_err().to_string();
            assert!(e.contains("client megapc: invalid range"), "{bad}: {e}");
        }
        // not a percentage at all: rejected by serde
        assert!(with_range("[-1, 50]").is_err());
        assert!(with_range("[0, 300]").is_err());
    }

    /// `check_file` (as startup, reload and `doctor` load) on `text`
    fn check(name: &str, text: &str) -> Result<(), ConfigError> {
        let dir = temp_dir(name);
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, text).unwrap();
        let result = check_file(&path);
        fs::remove_dir_all(&dir).unwrap();
        result
    }

    /// `text` is refused with an error naming each of `names`
    fn rejected(name: &str, text: &str, names: &[&str]) {
        let e = check(name, text).expect_err(name).to_string();
        for n in names {
            assert!(e.contains(n), "{name}: {e:?} lacks {n:?}");
        }
    }

    const HOTKEYS: &str = r#"
[[clients]]
hostname = "a"
position = "left"
hotkey = ["KeyLeftCtrl", "Key1"]

[[clients]]
hostname = "b"
position = "right"
hotkey = ["KeyLeftCtrl", "Key2"]
"#;

    /// two clients with one chord (in any key order): one of them could
    /// never be reached by it. Such a file is never overwritten
    #[tokio::test]
    async fn duplicate_client_hotkey_is_rejected() {
        check("hotkeys", HOTKEYS).unwrap();
        let duplicate = HOTKEYS.replace(r#"["KeyLeftCtrl", "Key2"]"#, r#"["Key1", "KeyLeftCtrl"]"#);
        rejected(
            "duplicate-hotkey",
            &duplicate,
            &["client b: `hotkey`", "the `hotkey` of client a"],
        );

        let dir = temp_dir("duplicate-hotkey-save");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, &duplicate).unwrap();
        let mut config = Config::at(&path).unwrap();
        let mut keys = config.authorized_fingerprints();
        keys.insert("cc:dd".into(), "phone".into());
        let e = config.save(config.clients(), keys).await.unwrap_err();
        assert!(matches!(e, ConfigError::Invalid { .. }), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), duplicate);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// a client hotkey that is also `local_hotkey` would both return control
    /// and move it on
    #[test]
    fn client_hotkey_equal_to_local_hotkey_is_rejected() {
        let text = format!("local_hotkey = [\"Key2\", \"KeyLeftCtrl\"]\n{HOTKEYS}");
        rejected(
            "local-hotkey",
            &text,
            &["client b: `hotkey`", "is also `local_hotkey`"],
        );
    }

    /// a client hotkey that is also the release bind, set or default
    #[test]
    fn client_hotkey_equal_to_release_bind_is_rejected() {
        let text = format!("release_bind = [\"Key1\", \"KeyLeftCtrl\"]\n{HOTKEYS}");
        rejected(
            "release-bind",
            &text,
            &["client a: `hotkey`", "is also `release_bind`"],
        );
        let default = HOTKEYS.replace(
            r#"["KeyLeftCtrl", "Key2"]"#,
            r#"["KeyLeftAlt", "KeyLeftMeta", "KeyLeftShift", "KeyLeftCtrl"]"#,
        );
        rejected(
            "default-release-bind",
            &default,
            &["client b: `hotkey`", "is also `release_bind` (default)"],
        );
    }

    /// port 0 is no port to listen on or connect to; `change-port 0` is
    /// refused before the listener rebinds
    #[test]
    fn port_zero_is_rejected() {
        rejected(
            "port-zero",
            &format!("port = 0\n{HOTKEYS}"),
            &["`port` = 0"],
        );
        let client = HOTKEYS.replace("position = \"right\"", "position = \"right\"\nport = 0");
        rejected("client-port-zero", &client, &["client b: `port` = 0"]);

        let dir = temp_dir("change-port-zero");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, COMMENTED).unwrap();
        let config = Config::at(&path).unwrap();
        assert!(config.check_port(0).is_err());
        config.check_port(4300).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `max_file_size = 0` is no "files off": it drops every copy with
    /// files, text included
    #[test]
    fn max_file_size_zero_is_rejected() {
        check("max-file-size-one", "max_file_size = 1\n").unwrap();
        rejected(
            "max-file-size-zero",
            "max_file_size = 0\n",
            &["`max_file_size` = 0"],
        );
    }

    /// a plain-text telemetry endpoint fails at load, not at the first
    /// upload; empty means the built-in one
    #[test]
    fn non_https_telemetry_endpoint_is_rejected() {
        check(
            "https-endpoint",
            "telemetry_endpoint = \"https://t.example\"\n",
        )
        .unwrap();
        check("empty-endpoint", "telemetry_endpoint = \"\"\n").unwrap();
        rejected(
            "http-endpoint",
            "telemetry_endpoint = \"http://t.example\"\n",
            &["`telemetry_endpoint`", "only https://"],
        );
    }

    /// B10: alternate clients at one position, overlapping ranges included,
    /// are a valid config
    #[test]
    fn alternate_clients_at_one_position_load() {
        let text = HOTKEYS.replace(
            "position = \"right\"",
            "position = \"left\"\nrange = [0, 50]",
        );
        let text = text.replacen(
            "position = \"left\"",
            "position = \"left\"\nactivate_on_startup = true",
            1,
        );
        let dir = temp_dir("alternates");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, &text).unwrap();
        check_file(&path).unwrap();
        let positions: Vec<_> = Config::at(&path)
            .unwrap()
            .clients()
            .into_iter()
            .map(|c| c.pos)
            .collect();
        assert_eq!(positions, [Position::Left, Position::Left]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// set-position / set-host edit the client's table in place
    #[test]
    fn moved_or_renamed_client_keeps_its_table() {
        let commented = COMMENTED.replace(
            "activate_on_startup = true\n",
            "activate_on_startup = true\nfuture_key = 1\n",
        );
        let (mut doc, mut cfg) = parsed(&commented);
        cfg.clients.as_mut().unwrap()[0].position = Some(Position::Top);
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert_eq!(
            out,
            commented.replace("position = \"left\"", "position = \"top\"")
        );

        cfg.clients.as_mut().unwrap()[0].hostname = Some("desk2".into());
        merge_config(&mut doc, &cfg).unwrap();
        let out = doc.to_string();
        assert_eq!(
            out,
            commented
                .replace("position = \"left\"", "position = \"top\"")
                .replace("\"desk\"", "\"desk2\"")
        );
        assert_eq!(parsed(&out).1, cfg);
    }

    /// a file that does not deserialize (e.g. mid-edit) is not overwritten
    #[test]
    fn invalid_file_is_not_merged() {
        let (_, cfg) = parsed(COMMENTED);
        let broken = COMMENTED.replace("port = 4242", "port = \"4242\"");
        let mut doc = broken.parse::<DocumentMut>().unwrap();
        assert!(merge_config(&mut doc, &cfg).is_err());
        assert_eq!(doc.to_string(), broken);
    }

    /// a fresh directory under the system temp dir
    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("mimouse-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// edits the file at `path` like a user would, returns the new text
    fn edit(path: &Path, f: impl FnOnce(&mut DocumentMut)) -> String {
        let mut doc = fs::read_to_string(path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        f(&mut doc);
        let text = doc.to_string();
        fs::write(path, &text).unwrap();
        text
    }

    fn invalid_range(doc: &mut DocumentMut) {
        let mut client = toml_edit::Table::new();
        client["hostname"] = toml_edit::value("bad");
        client["range"] = toml_edit::value(toml_edit::Array::from_iter([80i64, 20]));
        match doc.get_mut("clients") {
            Some(toml_edit::Item::ArrayOfTables(clients)) => clients.push(client),
            _ => {
                let mut clients = toml_edit::ArrayOfTables::new();
                clients.push(client);
                doc.insert("clients", toml_edit::Item::ArrayOfTables(clients));
            }
        }
    }

    const UNKNOWN_KEYS: &str = r#"future_top = 1

[future_table]
x = 1

[[clients]]
hostname = "a"
position = "left"
future_client_key = "kept"
"#;

    const INVALID_RANGE: &str = "[[clients]]\nhostname = \"megapc\"\nrange = [80, 20]\n";

    struct Fixture {
        name: &'static str,
        /// `None`: no file
        content: Option<&'static str>,
        /// the config path is a relative symlink to a 0640 file elsewhere
        symlink: bool,
        /// still in the file after edits
        kept: &'static [&'static str],
    }

    /// load -> save: unchanged is byte-identical; disk and memory edits of
    /// different keys merge; the same key changed on both sides and an
    /// invalid file refuse and leave the file byte-identical
    #[tokio::test]
    async fn round_trip_fixtures() {
        let fixtures = [
            Fixture {
                name: "commented",
                content: Some(COMMENTED),
                symlink: false,
                kept: &[
                    "# my mimouse config\n",
                    "# the laptop\n",
                    "unknown_key = \"kept\"",
                ],
            },
            Fixture {
                name: "unknown-keys",
                content: Some(UNKNOWN_KEYS),
                symlink: false,
                kept: &[
                    "future_top = 1",
                    "[future_table]\nx = 1",
                    "future_client_key = \"kept\"",
                ],
            },
            Fixture {
                name: "symlinked",
                content: Some(COMMENTED),
                symlink: cfg!(unix),
                kept: &["# my mimouse config\n", "# the laptop\n"],
            },
            Fixture {
                name: "missing",
                content: None,
                symlink: false,
                kept: &[],
            },
        ];
        for f in fixtures {
            let dir = temp_dir(&format!("roundtrip-{}", f.name));
            let path = dir.join("cfg").join(CONFIG_FILE_NAME);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            #[cfg(unix)]
            if f.symlink {
                use std::os::unix::fs::PermissionsExt;
                let real = dir.join("real").join(CONFIG_FILE_NAME);
                fs::create_dir_all(real.parent().unwrap()).unwrap();
                fs::write(&real, f.content.unwrap()).unwrap();
                fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
                std::os::unix::fs::symlink("../real/config.toml", &path).unwrap();
            }
            if let (false, Some(content)) = (f.symlink, f.content) {
                fs::write(&path, content).unwrap();
            }
            let mut config = Config::at(&path).unwrap();

            let original = fs::read(&path).unwrap();
            let saved = config
                .save(config.clients(), config.authorized_fingerprints())
                .await;
            assert!(!saved.unwrap(), "{}", f.name);
            assert_eq!(fs::read(&path).unwrap(), original, "{}", f.name);

            edit(&path, |doc| doc["notify_switch"] = toml_edit::value(false));
            let mut keys = config.authorized_fingerprints();
            keys.insert("cc:dd".into(), "phone".into());
            let reload = config.save(config.clients(), keys).await.unwrap();
            assert!(reload, "{}: the disk edit is new to memory", f.name);
            assert!(!config.notify_switch(), "{}", f.name);
            let (on_disk, text) = ConfigToml::load(&path).unwrap();
            assert_eq!(on_disk.notify_switch, Some(false), "{}", f.name);
            assert_eq!(
                on_disk.authorized_fingerprints.unwrap()["cc:dd"],
                "phone",
                "{}",
                f.name
            );
            for kept in f.kept {
                assert!(text.contains(kept), "{}: {kept:?} lost:\n{text}", f.name);
            }
            #[cfg(unix)]
            if f.symlink {
                use std::os::unix::fs::PermissionsExt;
                assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
                let real = dir.join("real").join(CONFIG_FILE_NAME);
                let mode = fs::metadata(&real).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o640);
            }

            let before = edit(&path, |doc| {
                doc["authorized_fingerprints"]["ee:ff"] = toml_edit::value("disk")
            });
            let mut keys = config.authorized_fingerprints();
            keys.insert("ee:ff".into(), "memory".into());
            let e = config.save(config.clients(), keys).await.unwrap_err();
            assert!(
                matches!(&e, ConfigError::Conflict { key, .. } if key == "authorized_fingerprints.ee:ff"),
                "{}: {e}",
                f.name
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), before, "{}", f.name);

            let edits: [fn(&mut DocumentMut); 3] = [
                |doc| doc["port"] = toml_edit::value("not a port"),
                invalid_range,
                // no edit: made unparseable below
                |_| {},
            ];
            for (i, broken) in edits.into_iter().enumerate() {
                let mut text = edit(&path, broken);
                if i == 2 {
                    // does not parse at all
                    text.push_str("[[[\n");
                    fs::write(&path, &text).unwrap();
                }
                let mut keys = config.authorized_fingerprints();
                keys.insert("gg:hh".into(), "memory".into());
                let e = config.save(config.clients(), keys).await.unwrap_err();
                assert!(matches!(e, ConfigError::Invalid { .. }), "{}: {e}", f.name);
                assert_eq!(fs::read_to_string(&path).unwrap(), text, "{}", f.name);
                fs::write(&path, &before).unwrap();
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// an invalid file at startup: memory is the defaults (no last good
    /// copy), and nothing is ever written over the file
    #[tokio::test]
    async fn invalid_range_file_is_never_written() {
        let dir = temp_dir("invalid-range");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, INVALID_RANGE).unwrap();
        let mut config = Config::at(&path).unwrap();
        assert!(config.clients().is_empty());
        let unchanged = config
            .save(config.clients(), config.authorized_fingerprints())
            .await;
        assert!(matches!(unchanged, Err(ConfigError::Invalid { .. })));
        let mut keys = config.authorized_fingerprints();
        keys.insert("cc:dd".into(), "phone".into());
        let changed = config.save(config.clients(), keys).await;
        assert!(matches!(changed, Err(ConfigError::Invalid { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), INVALID_RANGE);
        assert!(!last_good_path(&path).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// an invalid file at startup loads the last good copy; saving is
    /// refused until the file is fixed, then merges with the fix
    #[tokio::test]
    async fn invalid_file_at_startup_loads_last_good() {
        let dir = temp_dir("last-good");
        let path = dir.join(CONFIG_FILE_NAME);
        let last_good = last_good_path(&path);
        fs::write(&path, COMMENTED).unwrap();
        drop(Config::at(&path).unwrap());
        assert_eq!(fs::read_to_string(&last_good).unwrap(), COMMENTED);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&last_good).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let broken = COMMENTED.replace("activate_on_startup = true", "range = [80, 20]");
        fs::write(&path, &broken).unwrap();
        let mut config = Config::at(&path).unwrap();
        assert_eq!(config.clients().len(), 2);
        assert_eq!(config.port(), 4242);
        assert!(config.authorized_fingerprints().contains_key("aa:bb"));

        let mut keys = config.authorized_fingerprints();
        keys.insert("cc:dd".into(), "phone".into());
        let e = config
            .save(config.clients(), keys.clone())
            .await
            .unwrap_err();
        assert!(matches!(e, ConfigError::Invalid { .. }), "{e}");
        assert!(e.to_string().contains("mimouse cli doctor"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
        assert_eq!(fs::read_to_string(&last_good).unwrap(), COMMENTED);

        let fixed = COMMENTED.replace("port = 4242", "port = 4244");
        fs::write(&path, &fixed).unwrap();
        assert!(config.save(config.clients(), keys).await.unwrap());
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("port = 4244"), "{out}");
        assert!(out.contains("\"cc:dd\" = \"phone\""), "{out}");
        assert_eq!(fs::read_to_string(&last_good).unwrap(), out);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// clients without `fingerprint` load unpinned; a pin set at runtime is
    /// saved into its client's table and loads back
    #[tokio::test]
    async fn fingerprint_round_trips() {
        let dir = temp_dir("fingerprint");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, COMMENTED).unwrap();
        let mut config = Config::at(&path).unwrap();
        let mut clients = config.clients();
        assert!(clients.iter().all(|c| c.fingerprint.is_none()));

        clients[1].fingerprint = Some("aa:01".into());
        config
            .save(clients, config.authorized_fingerprints())
            .await
            .unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert_eq!(
            out,
            COMMENTED.replace("port = 4243\n", "port = 4243\nfingerprint = \"aa:01\"\n")
        );
        let pins: Vec<_> = Config::at(&path)
            .unwrap()
            .clients()
            .into_iter()
            .map(|c| c.fingerprint)
            .collect();
        assert_eq!(pins, [None, Some("aa:01".to_owned())]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// a changed port is saved; `--port` still wins over the saved value
    #[tokio::test]
    async fn changed_port_is_saved_and_flag_wins() {
        let dir = temp_dir("set-port");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, COMMENTED).unwrap();
        let mut config = Config::at(&path).unwrap();
        config.set_port(4300);
        config
            .save(config.clients(), config.authorized_fingerprints())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            COMMENTED.replace("port = 4242", "port = 4300")
        );
        assert_eq!(Config::at(&path).unwrap().port(), 4300);
        let flagged = Config::from_args(Args::parse_from([
            "mimouse".as_ref(),
            "--config".as_ref(),
            path.as_os_str(),
            "--port".as_ref(),
            "4400".as_ref(),
        ]))
        .unwrap();
        assert_eq!(flagged.port(), 4400);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// B8: a watcher error neither reloads nor stops the service
    #[tokio::test(start_paused = true)]
    async fn watcher_error_does_not_reload() {
        let dir = temp_dir("watch-error");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, COMMENTED).unwrap();
        let mut config = Config::at(&path).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        config.watch_rx = rx;
        tx.send(Err(notify::Error::generic("queue overflow")))
            .await
            .unwrap();
        let wait = std::time::Duration::from_secs(5);
        let reloaded = tokio::time::timeout(wait, config.changed()).await;
        assert!(reloaded.is_err(), "reloaded on a watcher error");
        assert_eq!(tx.capacity(), 1, "error not consumed");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// reloads `config` from its file, as the watcher does after an edit
    async fn reload(config: &mut Config) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        config.watch_rx = rx;
        let modify = EventKind::Modify(notify::event::ModifyKind::Any);
        let event = notify::Event::new(modify).add_path(config.config_path.clone());
        tx.send(Ok(event)).await.unwrap();
        let wait = std::time::Duration::from_secs(10);
        tokio::time::timeout(wait, config.changed())
            .await
            .expect("reloaded");
    }

    /// R-6: a reload cancelled mid-load read the file a save then merged;
    /// the next `changed()` must not apply that stale read over the save
    #[tokio::test]
    async fn save_drops_the_reload_it_merged() {
        use futures::FutureExt;
        let dir = temp_dir("save-drops-reload");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, "notify_switch = true\n").unwrap();
        let mut config = Config::at(&path).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        config.watch_rx = rx;
        fs::write(&path, "notify_switch = false\n").unwrap();
        let modify = EventKind::Modify(notify::event::ModifyKind::Any);
        let event = notify::Event::new(modify).add_path(config.config_path.clone());
        tx.send(Ok(event)).await.unwrap();
        // the load starts, then another `select!` branch cancels `changed()`
        assert!(config.changed().now_or_never().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let authorized = HashMap::from([("fp".to_owned(), "x".to_owned())]);
        assert!(
            config.save(vec![], authorized).await.unwrap(),
            "edit merged"
        );
        let wait = std::time::Duration::from_secs(1);
        let reloaded = tokio::time::timeout(wait, config.changed()).await;
        assert!(reloaded.is_err(), "stale load applied");
        assert!(config.authorized_fingerprints().contains_key("fp"));
        assert!(!config.notify_switch(), "merged edit lost");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// T4: a reload applies hot keys; restart-only keys keep the value the
    /// service started with and are listed as awaiting a restart, unless a
    /// flag sets them
    #[tokio::test]
    async fn restart_only_change_awaits_restart() {
        let dir = temp_dir("awaiting-restart");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, COMMENTED).unwrap();
        let mut config = Config::at(&path).unwrap();
        let mut flagged = Config::from_args(Args::parse_from([
            "mimouse".as_ref(),
            "--config".as_ref(),
            path.as_os_str(),
            "--port".as_ref(),
            "4400".as_ref(),
        ]))
        .unwrap();
        assert_eq!(config.awaiting_restart(), []);
        edit(&path, |doc| {
            doc["port"] = toml_edit::value(4300);
            doc["clipboard"] = toml_edit::value(true);
            doc["max_file_size"] = toml_edit::value(1024);
            doc["idle_inhibit"] = toml_edit::value(false);
        });
        reload(&mut config).await;
        assert!(!config.idle_inhibit(), "hot key applied on reload");
        assert!(!config.clipboard(), "clipboard changed without a restart");
        assert_eq!(config.max_file_size(), DEFAULT_MAX_FILE_SIZE);
        assert_eq!(config.port(), 4242);
        let pending = |c: &Config| {
            c.awaiting_restart()
                .into_iter()
                .map(|p| (p.key, p.in_effect, p.on_disk))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pending(&config),
            [
                ("port".into(), json(4242), json(4300)),
                ("clipboard".into(), json(false), json(true)),
                (
                    "max_file_size".into(),
                    json(DEFAULT_MAX_FILE_SIZE),
                    json(1024)
                ),
            ]
        );
        let report = config.report();
        assert_eq!(report.awaiting_restart.len(), 3);
        let idle = report
            .keys
            .iter()
            .find(|k| k.key == "idle_inhibit")
            .unwrap();
        assert_eq!((&idle.value, idle.source), (&json(false), Source::File));

        // `--port` wins over the file at the next start too
        reload(&mut flagged).await;
        assert_eq!(flagged.port(), 4400);
        let keys: Vec<_> = pending(&flagged).into_iter().map(|p| p.0).collect();
        assert_eq!(keys, ["clipboard", "max_file_size"]);

        // `change-port` to the file's port: the live port is what a restart uses
        config.set_port(4300);
        assert!(!pending(&config).iter().any(|p| p.0 == "port"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// T4: a flag beats the file, the file beats the default, per key
    #[tokio::test]
    async fn effective_config_sources() {
        let dir = temp_dir("sources");
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "port = 4300\ncapture_backend = \"dummy\"\ncert_path = \"/file.pem\"\nclipboard = true\n",
        )
        .unwrap();
        let config = Config::from_args(Args::parse_from([
            "mimouse".as_ref(),
            "--config".as_ref(),
            path.as_os_str(),
            "--port".as_ref(),
            "4400".as_ref(),
            "--emulation-backend".as_ref(),
            "dummy".as_ref(),
            "--cert-path".as_ref(),
            "/flag.pem".as_ref(),
        ]))
        .unwrap();
        let report = config.report();
        let key = |name: &str| {
            let k = report.keys.iter().find(|k| k.key == name).unwrap();
            (k.value.clone(), k.source, k.restart)
        };
        assert_eq!(key("port"), (json(4400), Source::Flag, true));
        assert_eq!(key("cert_path"), (json("/flag.pem"), Source::Flag, true));
        assert_eq!(
            key("emulation_backend"),
            (json("dummy"), Source::Flag, true)
        );
        assert_eq!(key("capture_backend"), (json("dummy"), Source::File, true));
        assert_eq!(key("clipboard"), (json(true), Source::File, true));
        let max = json(DEFAULT_MAX_FILE_SIZE);
        assert_eq!(key("max_file_size"), (max, Source::Default, true));
        assert_eq!(key("idle_inhibit"), (json(true), Source::Default, false));
        assert_eq!(key("local_hotkey"), (json(()), Source::Default, false));
        assert_eq!(report.keys.len(), 15, "every key of the file is reported");
        assert_eq!(report.awaiting_restart, []);

        // without flags: the file, where it sets the key
        let config = Config::at(&path).unwrap();
        let report = config.report();
        let source = |name: &str| report.keys.iter().find(|k| k.key == name).unwrap().source;
        assert_eq!(source("port"), Source::File);
        assert_eq!(source("cert_path"), Source::File);
        assert_eq!(source("emulation_backend"), Source::Default);
        fs::remove_dir_all(&dir).unwrap();
    }
}

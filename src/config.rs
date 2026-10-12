use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut};

use crate::cli::CliArgs;
use crate::ipc::{DEFAULT_PORT, Position};

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
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
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
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub enter_hook: Option<String>,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
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
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            enter_hook,
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
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

/// ways to return control to the capturing machine
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseBind {
    /// chord: all keys held at once
    pub keys: Vec<scancode::Linux>,
    /// Ctrl tapped twice in quick succession
    pub double_tap_ctrl: bool,
}

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

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

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

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
            config_toml,
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

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        self.watcher.unwatch(&self.config_dir)?;
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        loop {
            let event = self.watch_rx.recv().await.expect("channel closed");
            let event = event.expect("filesystem event");
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
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
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// whether clipboard sharing is enabled (default: disabled)
    pub fn clipboard(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard)
            .unwrap_or(false)
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> ReleaseBind {
        let toml = self.config_toml.as_ref();
        ReleaseBind {
            keys: toml
                .and_then(|c| c.release_bind.clone())
                .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned())),
            double_tap_ctrl: toml.and_then(|c| c.release_double_tap_ctrl).unwrap_or(true),
        }
    }

    /// set configured clients
    pub fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        if clients.is_empty() {
            return;
        }
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").clients =
            Some(clients.into_iter().map(|c| c.into()).collect::<Vec<_>>());
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, String>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .authorized_fingerprints = Some(fingerprints);
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", &self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        Ok(changed)
    }

    pub fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", &self.config_path);
        let new_config = self.config_toml.clone().unwrap_or_default();
        let current = match fs::read_to_string(self.config_path()) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut doc = match current.parse::<DocumentMut>() {
            Ok(doc) => doc,
            Err(e) => {
                log::warn!("{:?}: {e}, rewriting it", self.config_path());
                DocumentMut::new()
            }
        };
        merge_config(&mut doc, &new_config);
        let new_config = doc.to_string();
        if new_config == current {
            return Ok(());
        }

        let _ = self.unwatch();
        /* write new config to file */
        if let Some(p) = self.config_path().parent() {
            fs::create_dir_all(p)?;
        }
        {
            let mut f = File::create(self.config_path())?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
        }

        let _ = self.watch();

        Ok(())
    }
}

/// update `doc` in place so it describes `config`: only keys whose value
/// differs are touched, so comments, ordering and unknown keys survive
fn merge_config(doc: &mut DocumentMut, config: &ConfigToml) {
    let old = toml_edit::de::from_document::<ConfigToml>(doc.clone()).unwrap_or_default();
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
}

/// `[[clients]]`: an existing entry with the same hostname and position is
/// edited in place, unmatched entries are removed, new ones appended
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
    let key = |c: &TomlClient| (c.hostname.clone(), c.position.unwrap_or_default());
    let mut result = toml_edit::ArrayOfTables::new();
    for client in clients {
        let found = existing.iter_mut().find(|t| {
            t.as_ref()
                .and_then(|t| table_client(t))
                .is_some_and(|c| key(&c) == key(client))
        });
        let new = to_table(client);
        match found.and_then(Option::take) {
            Some(mut table) => {
                let old = table_client(&table).expect("matched above");
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

    fn parsed(s: &str) -> (DocumentMut, ConfigToml) {
        let doc = s.parse::<DocumentMut>().unwrap();
        let cfg = toml_edit::de::from_document::<ConfigToml>(doc.clone()).unwrap();
        (doc, cfg)
    }

    /// what the service does: clients go through ConfigClient
    fn normalized(mut cfg: ConfigToml) -> ConfigToml {
        cfg.clients = cfg.clients.map(|c| {
            c.into_iter()
                .map(|c| TomlClient::from(ConfigClient::from(c)))
                .collect()
        });
        cfg
    }

    #[test]
    fn noop_save_is_identical() {
        let (mut doc, cfg) = parsed(COMMENTED);
        merge_config(&mut doc, &normalized(cfg));
        assert_eq!(doc.to_string(), COMMENTED);
    }

    #[test]
    fn changing_one_client_keeps_rest() {
        let (mut doc, cfg) = parsed(COMMENTED);
        let mut cfg = normalized(cfg);
        cfg.clients.as_mut().unwrap()[1].port = Some(5000);
        merge_config(&mut doc, &cfg);
        let out = doc.to_string();
        assert_eq!(out, COMMENTED.replace("port = 4243", "port = 5000"));
        assert_eq!(normalized(parsed(&out).1), cfg);
    }

    #[test]
    fn add_and_remove_client() {
        let (mut doc, cfg) = parsed(COMMENTED);
        let mut cfg = normalized(cfg);
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
        });
        merge_config(&mut doc, &cfg);
        let out = doc.to_string();
        assert!(!out.contains("desk"), "{out}");
        assert!(out.contains("# my mimouse config\nport = 4242 # not the default"));
        assert!(out.contains("# right screen\n[[clients]]\nhostname = \"lap\" # trailing"));
        assert!(out.contains("unknown_key = \"kept\""));
        assert!(out.contains("# the laptop\n\"aa:bb\" = \"laptop\""));
        let (_, back) = parsed(&out);
        assert_eq!(normalized(back), cfg);

        // and removing every client drops the section
        cfg.clients = None;
        merge_config(&mut doc, &cfg);
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
        merge_config(&mut doc, &normalized(cfg.clone()));
        let out = doc.to_string();
        assert!(out.contains("port = 1234 # not the default"), "{out}");
        assert!(
            out.contains("# the laptop\n\"aa:bb\" = \"laptop\"\n"),
            "{out}"
        );
        assert_eq!(normalized(parsed(&out).1), normalized(cfg));
    }

    #[test]
    fn fresh_file() {
        let (_, cfg) = parsed(COMMENTED);
        let cfg = normalized(cfg);
        let mut doc = DocumentMut::new();
        merge_config(&mut doc, &cfg);
        let out = doc.to_string();
        assert!(out.contains("[[clients]]"), "{out}");
        assert_eq!(normalized(parsed(&out).1), cfg);
    }
}

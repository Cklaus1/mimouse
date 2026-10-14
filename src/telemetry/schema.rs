//! The upload format, `telemetry-collector/schema.json` (schema version 1):
//! every field is always present (`null` where nullable), and nothing else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(super) const SCHEMA_VERSION: u8 = 1;
/// completed days kept and sent
pub(super) const MAX_DAYS: usize = 7;
/// crash records per batch
pub(super) const MAX_CRASHES: usize = 8;
/// `count` maximum
pub(super) const MAX_COUNT: u64 = 1_000_000_000;
pub(super) const MAX_PEERS: u64 = 10_000;
/// `cli_commands` keys per day
pub(super) const MAX_COMMANDS: usize = 64;
/// crash message length, in characters
pub(super) const MAX_MESSAGE: usize = 256;

/// body of `POST /v1/batch`
#[derive(Serialize)]
pub(super) struct Batch<'a> {
    pub schema_version: u8,
    pub install_id: &'a str,
    #[serde(flatten)]
    pub identity: &'a Identity,
    pub days: &'a [Day],
    pub crashes: &'a [Crash],
}

/// the build and the system it runs on
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct Identity {
    pub version: String,
    pub commit: String,
    pub os_family: &'static str,
    pub os_version: Option<String>,
    pub desktop: Option<String>,
    pub session_type: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Selection {
    Configured,
    Auto,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Backend {
    pub backend: Option<String>,
    pub selection: Option<Selection>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DurationBuckets {
    pub lt_10s: u64,
    pub lt_60s: u64,
    pub ge_60s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SizeBuckets {
    pub lt_1k: u64,
    pub lt_64k: u64,
    pub lt_1m: u64,
    pub lt_64m: u64,
    pub ge_64m: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Reliability {
    pub reconnects: u64,
    pub ping_timeouts: u64,
    pub watchdog_releases: u64,
    pub capture_disables: u64,
    pub emulation_disables: u64,
    pub approval_waits: DurationBuckets,
    pub clipboard_failures: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Switches {
    pub edge: u64,
    pub hotkey: u64,
    pub cli_focus: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Usage {
    pub switches: Switches,
    pub peers: u64,
    pub clipboard_transfers: SizeBuckets,
    pub file_transfers: SizeBuckets,
    pub cli_commands: BTreeMap<String, u64>,
}

/// the counters of one UTC day
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Day {
    /// `YYYY-MM-DD`
    pub date: String,
    pub capture: Backend,
    pub emulation: Backend,
    pub reliability: Reliability,
    pub usage: Usage,
}

impl Day {
    pub(super) fn new(date: String) -> Self {
        Self {
            date,
            ..Default::default()
        }
    }

    /// counts a request named `command` (a `FrontendRequest` action);
    /// names beyond `MAX_COMMANDS` per day are dropped
    pub(super) fn count_command(&mut self, command: &str) {
        let commands = &mut self.usage.cli_commands;
        if !is_command_name(command)
            || (commands.len() >= MAX_COMMANDS && !commands.contains_key(command))
        {
            return;
        }
        let count = commands.entry(command.to_owned()).or_default();
        *count = (*count + 1).min(MAX_COUNT);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Crash {
    pub location: String,
    pub message: String,
    pub capture_backend: Option<String>,
    pub emulation_backend: Option<String>,
}

/// `^[a-z][a-z0-9_]{0,31}$`
fn is_command_name(name: &str) -> bool {
    name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `value` if it has 1 to `max` characters, all `allowed`
fn checked(value: String, max: usize, allowed: impl Fn(char) -> bool) -> Option<String> {
    (!value.is_empty() && value.chars().count() <= max && value.chars().all(allowed))
        .then_some(value)
}

impl Identity {
    /// reads the OS release and the desktop session (blocking: files, and
    /// the registry on Windows)
    pub(super) fn detect() -> Self {
        let commit = crate::config::build::SHORT_COMMIT;
        let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
        let commit = match (7..=40).contains(&commit.len()) && commit.chars().all(hex) {
            true => commit.to_owned(),
            false => String::new(),
        };
        let linux = cfg!(target_os = "linux");
        let var = |name| std::env::var(name).ok().filter(|_| linux);
        Self {
            version: crate::config::build::PKG_VERSION.to_owned(),
            commit,
            os_family: std::env::consts::OS,
            os_version: os_version().and_then(|v| {
                checked(v, 64, |c| {
                    c.is_ascii_alphanumeric() || " ._()+-".contains(c)
                })
            }),
            desktop: var("XDG_CURRENT_DESKTOP")
                .and_then(|d| checked(d, 64, |c| c.is_ascii_alphanumeric() || ":_.-".contains(c))),
            session_type: var("XDG_SESSION_TYPE")
                .and_then(|s| checked(s, 16, |c| c.is_ascii_lowercase() || c.is_ascii_digit())),
        }
    }
}

/// `ID VERSION_ID` from os-release, e.g. `fedora 41`
#[cfg(not(any(windows, target_os = "macos")))]
fn os_version() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    parse_os_release(&text)
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn parse_os_release(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|v| !v.is_empty())
    };
    let id = value("ID")?;
    Some(match value("VERSION_ID") {
        Some(version) => format!("{id} {version}"),
        None => id,
    })
}

/// `ProductVersion` of SystemVersion.plist, e.g. `15.1`
#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let (_, rest) = plist.split_once("<key>ProductVersion</key>")?;
    let (_, rest) = rest.split_once("<string>")?;
    let (version, _) = rest.split_once("</string>")?;
    Some(version.trim().to_owned())
}

/// `major.minor.build` from the registry, e.g. `10.0.26100`
#[cfg(windows)]
fn os_version() -> Option<String> {
    use windows::{
        Win32::System::Registry::{
            HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
        },
        core::{PCWSTR, w},
    };
    const KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let dword = |name: PCWSTR| {
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                KEY,
                name,
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut value).cast()),
                Some(&mut size),
            )
        }
        .is_ok();
        ok.then_some(value)
    };
    let mut build = [0u16; 32];
    let mut size = size_of_val(&build) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            KEY,
            w!("CurrentBuild"),
            RRF_RT_REG_SZ,
            None,
            Some(build.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .is_ok();
    let build = ok.then(|| {
        let len = build.iter().position(|&c| c == 0).unwrap_or(build.len());
        String::from_utf16_lossy(&build[..len])
    })?;
    let major = dword(w!("CurrentMajorVersionNumber"))?;
    let minor = dword(w!("CurrentMinorVersionNumber"))?;
    Some(format!("{major}.{minor}.{build}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_gives_id_and_version() {
        let fedora = "NAME=\"Fedora Linux\"\nVERSION_ID=41\nID=fedora\nPRETTY_NAME=\"x\"\n";
        assert_eq!(parse_os_release(fedora).as_deref(), Some("fedora 41"));
        let ubuntu = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n";
        assert_eq!(parse_os_release(ubuntu).as_deref(), Some("ubuntu 24.04"));
        // rolling releases have no version
        assert_eq!(parse_os_release("ID=arch\n").as_deref(), Some("arch"));
        assert_eq!(parse_os_release("NAME=x\n"), None);
    }

    #[test]
    fn command_names_follow_the_schema() {
        let mut day = Day::new("2026-10-13".into());
        for name in ["list", "pair_accept", "Bad", "1st", "with-dash", ""] {
            day.count_command(name);
        }
        day.count_command("list");
        let names: Vec<_> = day.usage.cli_commands.iter().collect();
        assert_eq!(
            names,
            [(&"list".to_owned(), &2), (&"pair_accept".to_owned(), &1)]
        );
        for i in 0..100 {
            day.count_command(&format!("c{i}"));
        }
        assert_eq!(day.usage.cli_commands.len(), MAX_COMMANDS);
        // known names keep counting at the cap
        day.count_command("list");
        assert_eq!(day.usage.cli_commands["list"], 3);
    }
}

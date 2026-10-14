//! What telemetry keeps on disk, all blocking `std::fs` (run through
//! `spawn_blocking` from the service): the install id in the config dir,
//! the counters and queue in `<cache dir>/mimouse/telemetry.json`, and the
//! crash records the panic hook writes next to it.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::schema::{Crash, Day, MAX_CRASHES, MAX_DAYS};

const STATE_FILE: &str = "telemetry.json";
const ID_FILE: &str = "telemetry-id";
const CRASH_PREFIX: &str = "crash-";

/// where telemetry keeps its files
#[derive(Clone, Debug)]
pub(crate) struct Dirs {
    /// `<cache dir>/mimouse`: counters, queue, crash records
    pub cache: PathBuf,
    /// the config dir: the install id
    pub config: PathBuf,
}

impl Dirs {
    /// `<cache dir>/mimouse`, else the config dir
    pub(crate) fn new(config_dir: &Path) -> Self {
        let cache = crate::clipboard::files::cache_dir()
            .map(|c| c.join("mimouse"))
            .unwrap_or_else(|| config_dir.to_owned());
        Self {
            cache,
            config: config_dir.to_owned(),
        }
    }

    fn state(&self) -> PathBuf {
        self.cache.join(STATE_FILE)
    }

    fn id(&self) -> PathBuf {
        self.config.join(ID_FILE)
    }
}

/// counters of the current day and the queue, as persisted
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    /// counting since the start of the day (UTC)
    pub today: Day,
    /// completed days not uploaded yet, oldest first
    pub days: Vec<Day>,
    /// crash records not uploaded yet, oldest first
    pub crashes: Vec<Crash>,
    /// UTC date of the last upload attempt
    pub last_attempt: Option<String>,
    /// why the last upload failed
    pub last_error: Option<String>,
}

impl State {
    /// starts `date` once the day changed: the finished day joins the
    /// queue, which keeps the most recent `MAX_DAYS`
    pub(super) fn roll(&mut self, date: &str) -> bool {
        if self.today.date == date {
            return false;
        }
        let finished = std::mem::replace(&mut self.today, Day::new(date.to_owned()));
        if !finished.date.is_empty() {
            self.days.push(finished);
        }
        self.cap();
        true
    }

    pub(super) fn queue_crashes(&mut self, crashes: impl IntoIterator<Item = Crash>) {
        self.crashes.extend(crashes);
        self.cap();
    }

    fn cap(&mut self) {
        let excess = self.days.len().saturating_sub(MAX_DAYS);
        self.days.drain(..excess);
        let excess = self.crashes.len().saturating_sub(MAX_CRASHES);
        self.crashes.drain(..excess);
    }

    /// nothing waits for an upload
    pub(super) fn is_empty(&self) -> bool {
        self.days.is_empty() && self.crashes.is_empty()
    }
}

/// a crash record as the panic hook writes it
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CrashRecord {
    /// telemetry was on when it was written: it may be uploaded
    pub upload: bool,
    #[serde(flatten)]
    pub crash: Crash,
}

pub(super) fn crash_file_name(unique: &str) -> String {
    format!("{CRASH_PREFIX}{unique}.json")
}

/// crash records in `dir`, oldest first; unreadable ones are skipped
pub(super) fn crash_records(dir: &Path) -> Vec<(PathBuf, CrashRecord)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(CRASH_PREFIX) && n.ends_with(".json"))
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let record = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((p, record))
        })
        .collect()
}

/// crash records written while telemetry was off: never uploaded, only
/// shown. Beyond the newest `MAX_CRASHES` they are deleted
pub(crate) fn local_crashes(dirs: &Dirs) -> Vec<Crash> {
    let mut local: Vec<_> = crash_records(&dirs.cache)
        .into_iter()
        .filter(|(_, r)| !r.upload)
        .collect();
    let excess = local.len().saturating_sub(MAX_CRASHES);
    for (path, _) in local.drain(..excess) {
        let _ = fs::remove_file(path);
    }
    local.into_iter().map(|(_, r)| r.crash).collect()
}

/// crash records written while telemetry was on, waiting to be queued
pub(super) fn uploadable_crashes(dirs: &Dirs) -> Vec<(PathBuf, Crash)> {
    crash_records(&dirs.cache)
        .into_iter()
        .filter(|(_, r)| r.upload)
        .map(|(p, r)| (p, r.crash))
        .collect()
}

/// the persisted state; a missing or damaged file starts over
pub(super) fn load_state(dirs: &Dirs) -> State {
    let path = dirs.state();
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!("{path:?}: {e}; starting over");
            State::default()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => State::default(),
        Err(e) => {
            log::warn!("{path:?}: {e}; starting over");
            State::default()
        }
    }
}

/// replaces the persisted state, readable by the user only
pub(super) fn save_state(dirs: &Dirs, json: &[u8]) -> io::Result<()> {
    fs::create_dir_all(&dirs.cache)?;
    crate::config::replace_file(&dirs.state(), json, true)
}

/// the install id, created (UUID v4) if there is none
pub(super) fn install_id(dirs: &Dirs) -> io::Result<String> {
    let path = dirs.id();
    match fs::read_to_string(&path) {
        Ok(id) if is_install_id(id.trim()) => return Ok(id.trim().to_owned()),
        Ok(_) => log::warn!("{path:?}: not an install id, replacing it"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let id = new_install_id()?;
    fs::create_dir_all(&dirs.config)?;
    crate::config::replace_file(&path, id.as_bytes(), true)?;
    Ok(id)
}

/// the install id, if there is one
pub(super) fn existing_install_id(dirs: &Dirs) -> Option<String> {
    let id = fs::read_to_string(dirs.id()).ok()?;
    is_install_id(id.trim()).then(|| id.trim().to_owned())
}

/// deletes the install id, the counters and queue, and the crash records
/// waiting for an upload
pub(super) fn purge(dirs: &Dirs) -> io::Result<()> {
    let mut result = Ok(());
    let paths = [dirs.id(), dirs.state()]
        .into_iter()
        .chain(uploadable_crashes(dirs).into_iter().map(|(p, _)| p));
    for path in paths {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                log::warn!("{path:?}: {e}");
                result = Err(e);
            }
            _ => {}
        }
    }
    result
}

/// a random lowercase UUID v4, from ring's `SystemRandom` (through the
/// rustls provider)
fn new_install_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("no random numbers"))?;
    Ok(format_uuid_v4(bytes))
}

fn format_uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// `^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`
fn is_install_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    let lengths: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    lengths == [8, 4, 4, 4, 12]
        && groups
            .iter()
            .all(|g| g.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
        && groups[2].starts_with('4')
        && groups[3].starts_with(['8', '9', 'a', 'b'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_ids_are_lowercase_uuid_v4() {
        for bytes in [[0u8; 16], [0xff; 16]] {
            let id = format_uuid_v4(bytes);
            assert!(is_install_id(&id), "{id}");
        }
        assert_eq!(
            format_uuid_v4([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        let a = new_install_id().unwrap();
        let b = new_install_id().unwrap();
        assert!(is_install_id(&a) && is_install_id(&b), "{a} {b}");
        assert_ne!(a, b);
        assert!(!is_install_id("3F2B8C1E-9A4D-4E7F-8B21-6C0D5E9A1F34"));
        assert!(!is_install_id("3f2b8c1e-9a4d-1e7f-8b21-6c0d5e9a1f34"));
    }

    #[test]
    fn queue_keeps_the_most_recent_days_and_crashes() {
        let mut state = State::default();
        assert!(state.roll("2026-10-01"));
        // the first day of counting has no finished day before it
        assert!(state.days.is_empty());
        assert!(!state.roll("2026-10-01"));
        for day in 2..=12 {
            state.roll(&format!("2026-10-{day:02}"));
        }
        let dates: Vec<_> = state.days.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(
            dates,
            [
                "2026-10-05",
                "2026-10-06",
                "2026-10-07",
                "2026-10-08",
                "2026-10-09",
                "2026-10-10",
                "2026-10-11"
            ]
        );
        assert_eq!(state.today.date, "2026-10-12");
        let crash = |line| Crash {
            location: format!("src/x.rs:{line}"),
            message: "m".into(),
            capture_backend: None,
            emulation_backend: None,
        };
        state.queue_crashes((0..10).map(crash));
        assert_eq!(state.crashes.len(), MAX_CRASHES);
        assert_eq!(state.crashes[0], crash(2));
    }
}

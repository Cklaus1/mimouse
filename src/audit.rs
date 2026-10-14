//! Audit log of state-changing control requests: one JSON object per line in
//! `<config dir>/audit.jsonl`, rotated at 10 MiB, 3 old files kept.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Sender},
    thread::JoinHandle,
};

use serde_json::{Value, json};

use crate::ipc::{FrontendRequest, Peer};

const FILE_NAME: &str = "audit.jsonl";
const MAX_SIZE: u64 = 10 * 1024 * 1024;
const KEEP: usize = 3;

/// records are written by a thread of their own, in order: the service's
/// event loop does no file IO. Dropping the log writes what is queued
pub(crate) struct AuditLog {
    lines: Option<Sender<String>>,
    writer: Option<JoinHandle<()>>,
}

impl AuditLog {
    pub(crate) fn new(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let (lines, rx) = mpsc::channel::<String>();
        let writer = std::thread::Builder::new()
            .name("audit".into())
            .spawn(move || {
                for line in rx {
                    if let Err(e) = append(&path, &line) {
                        log::warn!("audit log {path:?}: {e}");
                    }
                }
            })
            .map_err(|e| log::warn!("audit log: {e}"))
            .ok();
        Self {
            lines: Some(lines),
            writer,
        }
    }

    /// record `request` from `requester` and its outcome
    pub(crate) fn record(
        &self,
        requester: &Peer,
        request: &FrontendRequest,
        result: &Result<(), String>,
    ) {
        let line = line(requester, request, result);
        if let Some(lines) = &self.lines {
            // the writer only stops once `lines` is dropped
            let _ = lines.send(line);
        }
    }
}

impl Drop for AuditLog {
    fn drop(&mut self) {
        drop(self.lines.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn line(requester: &Peer, request: &FrontendRequest, result: &Result<(), String>) -> String {
    let mut details = serde_json::to_value(request).expect("serializable request");
    let action = details
        .as_object_mut()
        .and_then(|o| o.remove("action"))
        .unwrap_or(Value::Null);
    let mut record = json!({
        "ts": jiff::Timestamp::now().to_string(),
        "requester": requester,
        "action": action,
        "details": details,
    });
    record["result"] = match result {
        Ok(()) => json!("ok"),
        Err(e) => json!({ "error": e }),
    };
    let mut line = record.to_string();
    line.push('\n');
    line
}

fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Err(e) = rotate(path) {
        // keep recording into the current file rather than losing events
        log::warn!("audit log {path:?}: rotation failed, appending anyway: {e}");
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(line.as_bytes())
}

/// audit.jsonl -> audit.1.jsonl -> ... -> audit.KEEP.jsonl (dropped)
fn rotate(path: &Path) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(m) if m.len() >= MAX_SIZE => {}
        _ => return Ok(()),
    }
    for i in (1..KEEP).rev() {
        let from = rotated(path, i);
        if from.exists() {
            fs::rename(&from, rotated(path, i + 1))?;
        }
    }
    fs::rename(path, rotated(path, 1))
}

fn rotated(path: &Path, i: usize) -> PathBuf {
    path.with_file_name(format!("audit.{i}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_max_size_and_keeps_three() {
        let dir = std::env::temp_dir().join(format!("mimouse-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let full = vec![b'x'; MAX_SIZE as usize];
        let request = FrontendRequest::SaveConfig;

        // below the limit: appended in order, no rotation
        let log = AuditLog::new(&dir);
        log.record(&Peer::default(), &request, &Ok(()));
        log.record(&Peer::default(), &request, &Err("failed".into()));
        drop(log);
        assert!(!rotated(&path, 1).exists());
        let lines: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"], "save_config");
        assert_eq!(lines[1]["result"]["error"], "failed");

        // each full file shifts the chain; the oldest falls off after KEEP
        for _ in 0..=KEEP {
            fs::write(&path, &full).unwrap();
            AuditLog::new(&dir).record(&Peer::default(), &request, &Ok(()));
            assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
        }
        for i in 1..=KEEP {
            assert_eq!(fs::metadata(rotated(&path, i)).unwrap().len(), MAX_SIZE);
        }
        assert!(!rotated(&path, KEEP + 1).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn appends_when_rotation_fails() {
        let dir = std::env::temp_dir().join(format!("mimouse-audit-rot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // audit.2 can't move onto the non-empty directory audit.3: rename fails
        fs::create_dir_all(rotated(&dir.join(FILE_NAME), 2)).unwrap();
        fs::create_dir_all(rotated(&dir.join(FILE_NAME), 3).join("x")).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, vec![b'x'; MAX_SIZE as usize]).unwrap();

        AuditLog::new(&dir).record(&Peer::default(), &FrontendRequest::SaveConfig, &Ok(()));
        let content = fs::read_to_string(&path).unwrap();
        let last: Value = serde_json::from_str(content[MAX_SIZE as usize..].trim()).unwrap();
        assert_eq!(last["action"], "save_config", "event not appended");
        fs::remove_dir_all(&dir).unwrap();
    }
}

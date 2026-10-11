//! Audit log of state-changing control requests: one JSON object per line in
//! `<config dir>/audit.jsonl`, rotated at 10 MiB, 3 old files kept.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::ipc::{FrontendRequest, Peer};

const FILE_NAME: &str = "audit.jsonl";
const MAX_SIZE: u64 = 10 * 1024 * 1024;
const KEEP: usize = 3;

pub(crate) struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// record `request` from `requester` and its outcome
    pub(crate) fn record(
        &self,
        requester: &Peer,
        request: &FrontendRequest,
        result: &Result<(), String>,
    ) {
        if let Err(e) = self.write(requester, request, result) {
            log::warn!("audit log {:?}: {e}", self.path);
        }
    }

    fn write(
        &self,
        requester: &Peer,
        request: &FrontendRequest,
        result: &Result<(), String>,
    ) -> io::Result<()> {
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

        self.rotate()?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&self.path)?.write_all(line.as_bytes())
    }

    /// audit.jsonl -> audit.1.jsonl -> ... -> audit.KEEP.jsonl (dropped)
    fn rotate(&self) -> io::Result<()> {
        match fs::metadata(&self.path) {
            Ok(m) if m.len() >= MAX_SIZE => {}
            _ => return Ok(()),
        }
        for i in (1..KEEP).rev() {
            let from = self.rotated(i);
            if from.exists() {
                fs::rename(&from, self.rotated(i + 1))?;
            }
        }
        fs::rename(&self.path, self.rotated(1))
    }

    fn rotated(&self, i: usize) -> PathBuf {
        self.path.with_file_name(format!("audit.{i}.jsonl"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_max_size_and_keeps_three() {
        let dir = std::env::temp_dir().join(format!("mimouse-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = AuditLog::new(&dir);
        let full = vec![b'x'; MAX_SIZE as usize];
        let request = FrontendRequest::SaveConfig;

        // below the limit: appended, no rotation
        log.record(&Peer::default(), &request, &Ok(()));
        log.record(&Peer::default(), &request, &Err("failed".into()));
        assert!(!log.rotated(1).exists());
        let lines: Vec<Value> = fs::read_to_string(&log.path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"], "save_config");
        assert_eq!(lines[1]["result"]["error"], "failed");

        // each full file shifts the chain; the oldest falls off after KEEP
        for _ in 0..=KEEP {
            fs::write(&log.path, &full).unwrap();
            log.record(&Peer::default(), &request, &Ok(()));
            assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 1);
        }
        for i in 1..=KEEP {
            assert_eq!(fs::metadata(log.rotated(i)).unwrap().len(), MAX_SIZE);
        }
        assert!(!log.rotated(KEEP + 1).exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}

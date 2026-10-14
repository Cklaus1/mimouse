//! The panic hook: release builds abort on panic, so the record is written
//! synchronously before the default hook runs. Only the location (relative
//! to the crate), a `&'static str` message and the backends in use are kept;
//! formatted messages may carry input events, host names or paths.
//! SIGSEGV, OOM and stack overflows are not captured.

use std::{
    fs,
    io::Write,
    panic::PanicHookInfo,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering::Relaxed},
};

use super::{
    counters::Counters,
    schema::{Crash, MAX_MESSAGE},
    store::{CrashRecord, crash_file_name},
};

/// the message of a panic whose payload is not a `&'static str`
pub(super) const FORMATTED: &str = "<formatted>";

/// records every panic in `dir`, then runs the previous hook
pub(super) fn install(dir: PathBuf, counters: &'static Counters) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_record(&dir, info, counters);
        previous(info);
    }));
}

fn write_record(dir: &Path, info: &PanicHookInfo<'_>, counters: &Counters) {
    let (capture_backend, emulation_backend) = counters.backends();
    let record = CrashRecord {
        upload: counters.enabled.load(Relaxed),
        crash: Crash {
            location: info
                .location()
                .map(|l| format!("{}:{}", relative_file(l.file()), l.line().min(999_999)))
                .unwrap_or_else(|| "unknown.rs:0".to_owned()),
            message: message(info.payload()),
            capture_backend: capture_backend.map(str::to_owned),
            emulation_backend: emulation_backend.map(str::to_owned),
        },
    };
    let Ok(json) = serde_json::to_vec(&record) else {
        return;
    };
    // the time orders the records; the counter tells panics apart
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let unique = format!(
        "{millis:016}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    );
    let _ = fs::create_dir_all(dir);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Ok(mut file) = options.open(dir.join(crash_file_name(&unique))) {
        let _ = file.write_all(&json);
    }
}

/// the payload of `panic!("literal")`; anything formatted is withheld
fn message(payload: &(dyn std::any::Any + Send)) -> String {
    match payload.downcast_ref::<&'static str>() {
        Some(message) => message.chars().take(MAX_MESSAGE).collect(),
        None => FORMATTED.to_owned(),
    }
}

/// `file` from `src/` or its crate's directory on (`src/capture.rs`,
/// `input-capture/src/libei.rs`, `tokio-1.51.1/src/runtime/park.rs`), in
/// the characters the schema allows
pub(super) fn relative_file(file: &str) -> String {
    let file = file.replace('\\', "/");
    let relative = if file.starts_with("src/") {
        file.as_str()
    } else if let Some(src) = file.rfind("/src/") {
        let crate_dir = file[..src].rfind('/').map_or(0, |i| i + 1);
        &file[crate_dir..]
    } else {
        file.rsplit('/').next().unwrap_or_default()
    };
    let mut clean: String = relative
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '.' | '/' | '-' => c,
            _ => '_',
        })
        .collect();
    while clean.contains("..") {
        clean = clean.replace("..", "_");
    }
    let Some(stem) = clean.strip_suffix(".rs") else {
        return "unknown.rs".to_owned();
    };
    // at most 160 characters before `.rs`, from the end
    let stem = &stem[stem.len().saturating_sub(160)..];
    let stem = stem.trim_start_matches(['.', '/', '-']);
    match stem.is_empty() {
        true => "unknown.rs".to_owned(),
        false => format!("{stem}.rs"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_start_at_the_crate() {
        let cases = [
            ("src/capture.rs", "src/capture.rs"),
            ("input-capture/src/libei.rs", "input-capture/src/libei.rs"),
            (
                "/home/alice/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.51.1/src/runtime/park.rs",
                "tokio-1.51.1/src/runtime/park.rs",
            ),
            (
                "/rustc/b940084d7/library/core/src/option.rs",
                "core/src/option.rs",
            ),
            (
                "C:\\Users\\alice\\mimouse\\src\\main.rs",
                "mimouse/src/main.rs",
            ),
            ("/home/alice/build.rs", "build.rs"),
            ("/home/alice/notes.txt", "unknown.rs"),
            ("../x/src/../../a b.rs", "x/src/_/_/a_b.rs"),
        ];
        for (file, expected) in cases {
            assert_eq!(relative_file(file), expected, "{file}");
        }
        let long = format!("src/{}.rs", "a".repeat(300));
        let stripped = relative_file(&long);
        assert_eq!(stripped.len(), 163);
        assert!(!stripped.contains("alice"));
    }

    #[test]
    fn only_static_messages_are_kept() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("channel closed");
        assert_eq!(message(payload.as_ref()), "channel closed");
        let payload: Box<dyn std::any::Any + Send> =
            Box::new(format!("event from {}", "192.168.1.20"));
        assert_eq!(message(payload.as_ref()), FORMATTED);
        static LONG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "x".repeat(1000));
        let payload: Box<dyn std::any::Any + Send> = Box::new(LONG.as_str());
        assert_eq!(message(payload.as_ref()).chars().count(), MAX_MESSAGE);
    }
}

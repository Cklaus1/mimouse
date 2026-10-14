//! Copied files: local file references and the store for received files.
//!
//! Received files are written to `<cache dir>/mimouse/files/<transfer id>/`
//! while they arrive; only the newest `KEEP_TRANSFERS` complete transfer
//! directories are kept.

use std::{
    collections::HashSet,
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::task::spawn_blocking;

/// longest accepted file name in bytes (NAME_MAX on Linux)
pub(super) const MAX_NAME_LEN: usize = 255;
/// number of transfer directories kept in the store
const KEEP_TRANSFERS: usize = 3;

/// a regular file on the local file system
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileRef {
    pub(super) path: PathBuf,
    pub(super) size: u64,
    /// part of the clipboard digest, so copying a modified file again counts
    /// as a change
    modified: Option<SystemTime>,
}

impl FileRef {
    /// `None` if `path` is not a regular file (e.g. a directory)
    fn stat(path: &Path) -> io::Result<Option<FileRef>> {
        let meta = fs::metadata(path)?;
        Ok(meta.is_file().then(|| FileRef {
            path: path.to_owned(),
            size: meta.len(),
            modified: meta.modified().ok(),
        }))
    }
}

/// the regular files among the copied `paths`; directories and missing
/// files are skipped (blocking: stats every path)
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn local_files(paths: Vec<PathBuf>) -> Vec<FileRef> {
    paths
        .into_iter()
        .filter_map(|path| match FileRef::stat(&path) {
            Ok(Some(file)) => Some(file),
            Ok(None) => {
                log::info!(
                    "clipboard: not sending {}: not a regular file",
                    path.display()
                );
                None
            }
            Err(e) => {
                log::warn!("clipboard: not sending {}: {e}", path.display());
                None
            }
        })
        .collect()
}

/// whether `name` is a plain file name that cannot leave the directory it is
/// stored in
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
}

/// wire names of the `files` to send: base names, made unique by appending
/// ` (2)`, ` (3)`, ... before the extension
pub(super) fn transfer_names(files: &[FileRef]) -> Result<Vec<String>, PathBuf> {
    let mut used = HashSet::new();
    let mut names = Vec::with_capacity(files.len());
    for file in files {
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| valid_name(n))
            .ok_or_else(|| file.path.clone())?;
        let mut unique = name.clone();
        let mut n = 2;
        while !used.insert(unique.clone()) {
            unique = numbered(&name, n);
            n += 1;
        }
        if !valid_name(&unique) {
            return Err(file.path.clone());
        }
        names.push(unique);
    }
    Ok(names)
}

/// `name (n).ext`
fn numbered(name: &str, n: usize) -> String {
    match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

/// names to store the received files of one transfer under: made safe for
/// Windows if `sanitize`, and unique even where names differing only in
/// case collide (`fold_case`: Windows, and macOS by default)
struct LocalNames {
    used: HashSet<String>,
    sanitize: bool,
    fold_case: bool,
}

impl LocalNames {
    fn new(sanitize: bool, fold_case: bool) -> Self {
        Self {
            used: HashSet::new(),
            sanitize,
            fold_case,
        }
    }

    /// the local name of the next file, named `name` on the wire
    fn next(&mut self, name: &str) -> String {
        let name = if self.sanitize {
            windows_file_name(name)
        } else {
            name.to_owned()
        };
        let key = |n: &str| {
            if self.fold_case {
                n.to_lowercase()
            } else {
                n.to_owned()
            }
        };
        let mut unique = name.clone();
        let mut n = 2;
        while !self.used.insert(key(&unique)) {
            unique = numbered(&name, n);
            n += 1;
        }
        unique
    }
}

/// a valid wire name made safe for the Windows file system: reserved
/// characters replaced, trailing dots and spaces removed, device names
/// prefixed with `_`
fn windows_file_name(name: &str) -> String {
    const DEVICES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut safe: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    safe.truncate(safe.trim_end_matches(['.', ' ']).len());
    let stem = safe.split('.').next().unwrap_or_default().trim_end();
    if safe.is_empty() || DEVICES.iter().any(|d| d.eq_ignore_ascii_case(stem)) {
        safe.insert(0, '_');
    }
    safe
}

/// the per-user cache directory
/// (`$XDG_CACHE_HOME` / `~/.cache`, `%LOCALAPPDATA%`, `~/Library/Caches`)
pub(crate) fn cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    return env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"));
    #[cfg(not(any(windows, target_os = "macos")))]
    return env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
}

/// directory holding the received transfers
#[derive(Clone, Debug)]
pub(super) struct FileStore {
    root: PathBuf,
    /// transfers being received: not pruned
    receiving: Arc<Mutex<HashSet<u64>>>,
}

impl FileStore {
    pub(super) fn new() -> Option<Self> {
        Some(Self::at(cache_dir()?.join("mimouse").join("files")))
    }

    fn at(root: PathBuf) -> Self {
        Self {
            root,
            receiving: Default::default(),
        }
    }

    /// receive the files of one update; its transfer directory is created
    /// with the first file
    pub(super) fn receive(&self) -> Receiving {
        Receiving {
            store: self.clone(),
            dir: None,
            names: LocalNames::new(cfg!(windows), cfg!(any(windows, target_os = "macos"))),
            file: None,
            paths: Vec::new(),
        }
    }

    /// existing transfer ids, ascending
    fn transfers(&self) -> io::Result<Vec<u64>> {
        let mut ids: Vec<u64> = fs::read_dir(&self.root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// create the directory of a new transfer, kept until `received`
    /// (blocking)
    fn begin(&self) -> io::Result<(u64, PathBuf)> {
        create_private_dir_all(&self.root)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut receiving = self.receiving.lock().expect("lock");
        let mut id = next_transfer_id(&self.transfers()?, now);
        loop {
            let dir = self.root.join(id.to_string());
            match fs::create_dir(&dir) {
                Ok(()) => {
                    receiving.insert(id);
                    return Ok((id, dir));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => id += 1,
                Err(e) => return Err(e),
            }
        }
    }

    /// transfer `id` is complete (or removed)
    fn received(&self, id: u64) {
        self.receiving.lock().expect("lock").remove(&id);
    }

    /// remove all but the newest `KEEP_TRANSFERS` complete transfers
    /// (blocking)
    fn prune(&self) {
        let receiving = self.receiving.lock().expect("lock");
        let ids = match self.transfers() {
            Ok(ids) => ids,
            Err(e) => return log::warn!("clipboard: {}: {e}", self.root.display()),
        };
        let complete: Vec<_> = ids
            .into_iter()
            .filter(|id| !receiving.contains(id))
            .collect();
        for id in &complete[..complete.len().saturating_sub(KEEP_TRANSFERS)] {
            let dir = self.root.join(id.to_string());
            if let Err(e) = fs::remove_dir_all(&dir) {
                log::warn!("clipboard: could not remove {}: {e}", dir.display());
            }
        }
    }
}

/// a transfer id newer than every existing one, even if the clock went back
fn next_transfer_id(existing: &[u64], now: u64) -> u64 {
    existing.last().map_or(now, |&last| now.max(last + 1))
}

/// runs blocking file system work off the async thread
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    spawn_blocking(f).await.map_err(io::Error::other)?
}

/// the files of one update being received into a new transfer directory;
/// dropped before `finish`, the directory is removed
pub(super) struct Receiving {
    store: FileStore,
    /// id and directory of the transfer, once created
    dir: Option<(u64, PathBuf)>,
    names: LocalNames,
    /// the file being written
    file: Option<fs::File>,
    paths: Vec<PathBuf>,
}

impl Receiving {
    /// start the next file, named `name` (a valid wire name)
    pub(super) async fn create(&mut self, name: &str) -> io::Result<()> {
        self.file = None;
        let dir = match &self.dir {
            Some((_, dir)) => dir.clone(),
            None => {
                let store = self.store.clone();
                let (id, dir) = blocking(move || store.begin()).await?;
                self.dir = Some((id, dir.clone()));
                dir
            }
        };
        let path = dir.join(self.names.next(name));
        let (file, path) = blocking(move || {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            Ok((file, path))
        })
        .await?;
        self.file = Some(file);
        self.paths.push(path);
        Ok(())
    }

    /// append `chunk` to the file started last; gives the buffer back
    pub(super) async fn append(&mut self, chunk: Vec<u8>) -> io::Result<Vec<u8>> {
        let mut file = self.file.take().expect("a file was created");
        let (file, chunk) = blocking(move || {
            file.write_all(&chunk)?;
            Ok((file, chunk))
        })
        .await?;
        self.file = Some(file);
        Ok(chunk)
    }

    /// the received files; older transfers are pruned
    pub(super) async fn finish(mut self) -> io::Result<Vec<FileRef>> {
        self.file = None;
        let Some((id, _)) = self.dir else {
            return Ok(Vec::new());
        };
        let paths = std::mem::take(&mut self.paths);
        let stored = blocking(move || {
            let mut stored = Vec::with_capacity(paths.len());
            for path in &paths {
                stored.extend(FileRef::stat(path)?);
            }
            Ok(stored)
        })
        .await?;
        self.dir = None;
        let store = self.store.clone();
        blocking(move || {
            store.received(id);
            store.prune();
            Ok(())
        })
        .await?;
        Ok(stored)
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        let Some((id, dir)) = self.dir.take() else {
            return;
        };
        self.file = None;
        let store = self.store.clone();
        let remove = move || {
            let _ = fs::remove_dir_all(&dir);
            store.received(id);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// received files may be private: the store is readable by the user only
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
pub(super) mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// a temporary directory removed on drop
    pub(in super::super) struct TempDir(PathBuf);

    impl TempDir {
        pub(in super::super) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = env::temp_dir().join(format!(
                "mimouse-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(in super::super) fn path(&self) -> &Path {
            &self.0
        }

        pub(in super::super) fn file(&self, name: &str, contents: &[u8]) -> FileRef {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            FileRef::stat(&path).unwrap().unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn name_validation() {
        for ok in [
            "a.txt",
            "report 2026.pdf",
            "a..b",
            ".hidden",
            "grüße",
            "x:y",
        ] {
            assert!(valid_name(ok), "{ok:?}");
        }
        let long = "a".repeat(MAX_NAME_LEN + 1);
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "/etc",
            "../x",
            "..\\x",
            "a\\b",
            "a\0b",
            long.as_str(),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn local_files_skips_directories_and_missing() {
        let tmp = TempDir::new();
        let file = tmp.file("a.txt", b"abc");
        fs::create_dir(tmp.path().join("dir")).unwrap();
        let files = local_files(vec![
            tmp.path().join("dir"),
            file.path.clone(),
            tmp.path().join("missing"),
        ]);
        assert_eq!(files, vec![file]);
        assert_eq!(files[0].size, 3);
    }

    #[test]
    fn duplicate_names_are_numbered() {
        let a = TempDir::new();
        let b = TempDir::new();
        let c = TempDir::new();
        let files = [
            a.file("notes.txt", b"1"),
            b.file("notes.txt", b"2"),
            c.file("notes.txt", b"3"),
            a.file("Makefile", b"4"),
            b.file("Makefile", b"5"),
        ];
        assert_eq!(
            transfer_names(&files).unwrap(),
            [
                "notes.txt",
                "notes (2).txt",
                "notes (3).txt",
                "Makefile",
                "Makefile (2)"
            ]
        );
    }

    #[test]
    fn untransferable_name_is_reported() {
        let tmp = TempDir::new();
        let file = tmp.file("a\\b", b"x");
        assert_eq!(transfer_names(std::slice::from_ref(&file)), Err(file.path));
    }

    #[test]
    fn windows_names() {
        assert_eq!(windows_file_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(windows_file_name("name. ."), "name");
        assert_eq!(windows_file_name("..."), "_");
        assert_eq!(windows_file_name("con.txt"), "_con.txt");
        assert_eq!(windows_file_name("LPT1"), "_LPT1");
        assert_eq!(windows_file_name("console.txt"), "console.txt");
    }

    #[test]
    fn local_names_collide_after_sanitizing_or_case_folding() {
        let local_names = |names: &[&str], sanitize, fold_case| {
            let mut local = LocalNames::new(sanitize, fold_case);
            names.iter().map(|n| local.next(n)).collect::<Vec<_>>()
        };
        let names = [
            "Readme", "README", "a:b", "a_b", "x.", "x", "c.TXT", "C.txt",
        ];
        assert_eq!(
            local_names(&names, true, true),
            [
                "Readme",
                "README (2)",
                "a_b",
                "a_b (2)",
                "x",
                "x (2)",
                "c.TXT",
                "C (2).txt"
            ]
        );
        // case-sensitive file systems keep the wire names
        assert_eq!(local_names(&names[..2], false, false), ["Readme", "README"]);
        assert_eq!(
            local_names(&names[..2], false, true),
            ["Readme", "README (2)"]
        );
    }

    #[test]
    fn transfer_ids_increase() {
        assert_eq!(next_transfer_id(&[], 1000), 1000);
        assert_eq!(next_transfer_id(&[5, 900], 1000), 1000);
        // clock went back
        assert_eq!(next_transfer_id(&[5, 2000], 1000), 2001);
    }

    /// receive `files` into `store`, each in chunks of at most two bytes
    async fn store_files(store: &FileStore, files: &[(&str, &[u8])]) -> io::Result<Vec<FileRef>> {
        let mut receiving = store.receive();
        for (name, contents) in files {
            receiving.create(name).await?;
            for chunk in contents.chunks(2) {
                receiving.append(chunk.to_vec()).await?;
            }
        }
        receiving.finish().await
    }

    #[tokio::test]
    async fn store_writes_files_and_keeps_last_three_transfers() {
        let tmp = TempDir::new();
        let store = FileStore::at(tmp.path().join("files"));
        let mut dirs = Vec::new();
        for i in 0..5u8 {
            let stored = store_files(&store, &[("a.txt", &[i]), ("b.bin", &[i, i, i])])
                .await
                .unwrap();
            assert_eq!(stored.len(), 2);
            assert_eq!(fs::read(&stored[0].path).unwrap(), [i]);
            assert_eq!(fs::read(&stored[1].path).unwrap(), [i, i, i]);
            assert_eq!(stored[1].size, 3);
            dirs.push(stored[0].path.parent().unwrap().to_owned());
        }
        assert_eq!(store.transfers().unwrap().len(), 3);
        for (i, dir) in dirs.iter().enumerate() {
            assert_eq!(dir.exists(), i >= 2, "transfer {i}");
        }
    }

    /// a transfer still being received is neither pruned nor counted as one
    /// of the kept ones; one that is dropped unfinished is removed
    #[tokio::test]
    async fn transfers_in_progress_are_not_pruned() {
        let tmp = TempDir::new();
        let store = FileStore::at(tmp.path().join("files"));
        let mut slow = store.receive();
        slow.create("slow").await.unwrap();
        let slow_dir = store.transfers().unwrap();
        for i in 0..KEEP_TRANSFERS as u8 + 1 {
            store_files(&store, &[("a", &[i])]).await.unwrap();
        }
        assert_eq!(store.transfers().unwrap().len(), KEEP_TRANSFERS + 1);
        assert!(store.transfers().unwrap().starts_with(&slow_dir));

        drop(slow);
        // removal runs on the blocking pool
        for _ in 0..100 {
            if store.transfers().unwrap().len() == KEEP_TRANSFERS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!store.transfers().unwrap().starts_with(&slow_dir));
    }

    #[tokio::test]
    async fn failed_store_leaves_no_transfer() {
        let tmp = TempDir::new();
        let store = FileStore::at(tmp.path().to_owned());
        // the second file cannot be created (no such directory)
        assert!(
            store_files(&store, &[("a", b"1"), ("missing/b", b"2")])
                .await
                .is_err()
        );
        for _ in 0..100 {
            if store.transfers().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(store.transfers().unwrap().is_empty());
    }
}

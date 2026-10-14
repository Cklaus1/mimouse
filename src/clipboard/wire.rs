//! Wire format of a clipboard update (MiMouse <-> MiMouse only):
//!
//! v1: `"LMCB" | 1 | count: u8 | count * (mime_len: u8 | mime | len: u32 BE | data)`
//!
//! v2: the v1 layout with version 2, followed by
//! `file_count: u16 BE | file_count * (name_len: u16 BE | name | len: u64 BE | data)`.
//! Names are UTF-8 base names (see [`valid_name`]).
//!
//! Updates without files are sent as v1, so older MiMouse peers keep
//! receiving text and images. A clear is a v1 update without items, which
//! older MiMouse peers also decode as an empty clipboard.
//!
//! Formats are held in memory (at most `MAX_CLIPBOARD_SIZE`); file contents
//! are streamed from and to disk, at most [`CHUNK`] bytes at a time.

use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Take},
    task::spawn_blocking,
};

use super::{
    ClipboardData, MAX_CLIPBOARD_SIZE,
    files::{MAX_NAME_LEN, transfer_names, valid_name},
    transport::TransportError,
};

const MAGIC: &[u8; 4] = b"LMCB";
const V1: u8 = 1;
const V2: u8 = 2;
const MAX_ITEMS: usize = u8::MAX as usize;
const MAX_MIME_LEN: usize = u8::MAX as usize;
const MAX_FILES: usize = u16::MAX as usize;
/// largest piece of a copied file held in memory while it is sent or received
pub(super) const CHUNK: usize = 256 * 1024;

/// an update ready to send: the formats encoded, the files with their wire
/// names; file contents are read when sending
pub(super) struct Encoded {
    /// magic, version and formats
    head: Vec<u8>,
    files: Vec<(PathBuf, String)>,
}

/// the files of an update opened for one transfer, with their sizes then
#[derive(Default)]
pub(super) struct Opened(Vec<(File, u64)>);

/// longest frame a peer may send when copied files may total `max_file_size`
pub(super) fn max_frame_len(max_file_size: u64) -> u64 {
    let header = (MAGIC.len() + 2) as u64;
    let items = (MAX_ITEMS * (1 + MAX_MIME_LEN + 4) + MAX_CLIPBOARD_SIZE) as u64;
    let files = (2 + MAX_FILES * (2 + MAX_NAME_LEN + 8)) as u64;
    header + items + files + max_file_size
}

/// encode `data` for sending; fails if its files total more than
/// `max_file_size` bytes or cannot be named on the wire
pub(super) fn encode(data: &ClipboardData, max_file_size: u64) -> Result<Encoded, TransportError> {
    let items: Vec<_> = data
        .items
        .iter()
        .filter(|(mime, _)| mime.len() <= MAX_MIME_LEN)
        .take(MAX_ITEMS)
        .collect();
    let files_size = data.files_size();
    if files_size > max_file_size {
        return Err(TransportError::TooLarge(files_size));
    }
    if data.files.len() > MAX_FILES {
        return Err(TransportError::TooManyFiles(data.files.len()));
    }
    let names = transfer_names(&data.files).map_err(TransportError::FileName)?;
    let mut head = Vec::with_capacity(6 + data.size() + 5 * items.len());
    head.extend_from_slice(MAGIC);
    head.push(if data.files.is_empty() { V1 } else { V2 });
    head.push(items.len() as u8);
    for (mime, bytes) in items {
        head.push(mime.len() as u8);
        head.extend_from_slice(mime.as_bytes());
        head.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        head.extend_from_slice(bytes);
    }
    let paths = data.files.iter().map(|f| f.path.clone());
    Ok(Encoded {
        head,
        files: paths.zip(names).collect(),
    })
}

/// open the files at `paths` for sending (blocking); fails if they now total
/// more than `max_file_size` bytes (they may have grown since they were
/// copied)
pub(super) fn open(paths: &[PathBuf], max_file_size: u64) -> Result<Opened, TransportError> {
    let mut total: u64 = 0;
    let mut opened = Vec::with_capacity(paths.len());
    for path in paths {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        total = total.saturating_add(len);
        if total > max_file_size {
            return Err(TransportError::TooLarge(total));
        }
        opened.push((file, len));
    }
    Ok(Opened(opened))
}

impl Encoded {
    pub(super) fn paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(|(path, _)| path.clone()).collect()
    }

    /// length of the payload sent with `opened`
    pub(super) fn len(&self, opened: &Opened) -> u64 {
        if self.files.is_empty() {
            return self.head.len() as u64;
        }
        let files: u64 = (self.files.iter().zip(&opened.0))
            .map(|((_, name), (_, len))| 2 + name.len() as u64 + 8 + len)
            .sum();
        self.head.len() as u64 + 2 + files
    }

    /// write the payload with the files `opened` for it (see [`open`]); fails
    /// if a file changed its size since it was opened
    pub(super) async fn write(
        &self,
        w: &mut (impl AsyncWrite + Unpin),
        opened: Opened,
    ) -> Result<(), TransportError> {
        w.write_all(&self.head).await?;
        if self.files.is_empty() {
            return Ok(());
        }
        w.write_all(&(self.files.len() as u16).to_be_bytes())
            .await?;
        let mut buf = Vec::with_capacity(CHUNK + 1);
        for ((_, name), (file, len)) in self.files.iter().zip(opened.0) {
            w.write_all(&(name.len() as u16).to_be_bytes()).await?;
            w.write_all(name.as_bytes()).await?;
            w.write_all(&len.to_be_bytes()).await?;
            buf = send_file(w, file, len, buf).await?;
        }
        Ok(())
    }
}

/// write the `len` bytes of `file` to `w` in chunks, using `buf`; a file
/// that is shorter or longer by now is not sent
async fn send_file(
    w: &mut (impl AsyncWrite + Unpin),
    mut file: File,
    len: u64,
    mut buf: Vec<u8>,
) -> Result<Vec<u8>, TransportError> {
    let mut remaining = len;
    loop {
        let want = remaining.min(CHUNK as u64);
        // with the last chunk, read a byte more to notice a file that grew
        let limit = want + u64::from(want == remaining);
        let read;
        (file, buf, read) = spawn_blocking(move || {
            buf.clear();
            let read = (&mut file).take(limit).read_to_end(&mut buf);
            (file, buf, read)
        })
        .await
        .map_err(io::Error::other)?;
        if read? as u64 != want {
            return Err(TransportError::FileChanged);
        }
        w.write_all(&buf).await?;
        remaining -= want;
        if remaining == 0 {
            return Ok(buf);
        }
    }
}

/// where the files of a received update go
pub(super) trait FileSink {
    /// start the next file; `name` is a valid wire name
    async fn create(&mut self, name: &str) -> Result<(), TransportError>;
    /// append `chunk` to the file started last; gives the buffer back
    async fn append(&mut self, chunk: Vec<u8>) -> Result<Vec<u8>, TransportError>;
}

/// read an update of `len` bytes from `r`: returns its formats and passes
/// its files to `files`, [`CHUNK`] bytes at a time. `Invalid` if it is
/// malformed, has invalid file names, or exceeds the size limits.
pub(super) async fn read(
    r: &mut (impl AsyncRead + Unpin),
    len: u64,
    max_file_size: u64,
    files: &mut impl FileSink,
) -> Result<ClipboardData, TransportError> {
    let mut r = r.take(len);
    let data = read_update(&mut r, max_file_size, files).await?;
    if r.limit() > 0 {
        // trailing bytes
        return Err(TransportError::Invalid);
    }
    Ok(data)
}

async fn read_update(
    r: &mut Take<impl AsyncRead + Unpin>,
    max_file_size: u64,
    files: &mut impl FileSink,
) -> Result<ClipboardData, TransportError> {
    let mut head = [0; 6];
    r.read_exact(&mut head).await?;
    let [m0, m1, m2, m3, version, count] = head;
    if [m0, m1, m2, m3] != *MAGIC || (version != V1 && version != V2) {
        return Err(TransportError::Invalid);
    }
    let mut data = ClipboardData::default();
    for _ in 0..count {
        let mut mime = vec![0; r.read_u8().await? as usize];
        r.read_exact(&mut mime).await?;
        let len = r.read_u32().await? as usize;
        // counted as `ClipboardData::size` does, which skips empty formats
        let size = data.size() + if len > 0 { mime.len() + len } else { 0 };
        if size > MAX_CLIPBOARD_SIZE || len as u64 > r.limit() {
            return Err(TransportError::Invalid);
        }
        let mut bytes = vec![0; len];
        r.read_exact(&mut bytes).await?;
        let mime = String::from_utf8(mime).map_err(|_| TransportError::Invalid)?;
        data.push(&mime, bytes);
    }
    if version == V1 {
        return Ok(data);
    }
    let mut total: u64 = 0;
    let mut buf = Vec::with_capacity(CHUNK);
    for _ in 0..r.read_u16().await? {
        let mut name = vec![0; r.read_u16().await? as usize];
        r.read_exact(&mut name).await?;
        let name = String::from_utf8(name)
            .ok()
            .filter(|n| valid_name(n))
            .ok_or(TransportError::Invalid)?;
        let len = r.read_u64().await?;
        total = total
            .checked_add(len)
            .filter(|&t| t <= max_file_size && len <= r.limit())
            .ok_or(TransportError::Invalid)?;
        files.create(&name).await?;
        let mut remaining = len;
        while remaining > 0 {
            buf.resize(remaining.min(CHUNK as u64) as usize, 0);
            r.read_exact(&mut buf).await?;
            remaining -= buf.len() as u64;
            buf = files.append(buf).await?;
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use super::{super::files::tests::TempDir, *};
    use crate::clipboard::{MIME_HTML, MIME_PNG, MIME_TEXT};

    const MAX: u64 = 1024;

    fn sample() -> ClipboardData {
        let mut data = ClipboardData::default();
        data.push(MIME_TEXT, "grüße".into());
        data.push(MIME_HTML, "<b>x</b>".into());
        data.push(MIME_PNG, vec![0x89, b'P', b'N', b'G', 0, 0xff]);
        data
    }

    /// received files kept in memory; records the largest chunk it got
    #[derive(Default)]
    struct Memory {
        files: Vec<(String, Vec<u8>)>,
        largest_chunk: usize,
    }

    impl FileSink for Memory {
        async fn create(&mut self, name: &str) -> Result<(), TransportError> {
            self.files.push((name.to_owned(), Vec::new()));
            Ok(())
        }

        async fn append(&mut self, chunk: Vec<u8>) -> Result<Vec<u8>, TransportError> {
            self.largest_chunk = self.largest_chunk.max(chunk.len());
            let (_, file) = self.files.last_mut().expect("created");
            file.extend_from_slice(&chunk);
            Ok(chunk)
        }
    }

    /// a writer that records the largest write it got
    #[derive(Default)]
    struct Recorder {
        written: Vec<u8>,
        largest_write: usize,
    }

    impl AsyncWrite for Recorder {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.largest_write = self.largest_write.max(buf.len());
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    type Decoded = (ClipboardData, Vec<(String, Vec<u8>)>);

    async fn decode(buf: &[u8], max_file_size: u64) -> Option<Decoded> {
        let mut files = Memory::default();
        let data = read(&mut &buf[..], buf.len() as u64, max_file_size, &mut files).await;
        Some((data.ok()?, files.files))
    }

    async fn decoded(buf: &[u8]) -> Option<Decoded> {
        decode(buf, MAX).await
    }

    /// the payload sent for `data`
    async fn encoded(data: &ClipboardData, max_file_size: u64) -> Result<Vec<u8>, TransportError> {
        let encoded = encode(data, max_file_size)?;
        let opened = open(&encoded.paths(), max_file_size)?;
        let len = encoded.len(&opened);
        let mut buf = Vec::new();
        encoded.write(&mut buf, opened).await?;
        assert_eq!(buf.len() as u64, len);
        Ok(buf)
    }

    /// a v2 frame without items, written by hand
    fn v2_frame(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = b"LMCB\x02\x00".to_vec();
        buf.extend_from_slice(&(files.len() as u16).to_be_bytes());
        for (name, bytes) in files {
            buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            buf.extend_from_slice(bytes);
        }
        buf
    }

    #[tokio::test]
    async fn roundtrip() {
        let data = sample();
        let encoded = encoded(&data, MAX).await.unwrap();
        // no files: readable by MiMouse peers that only know v1
        assert_eq!(encoded[4], V1);
        assert_eq!(decoded(&encoded).await, Some((data, vec![])));
    }

    #[tokio::test]
    async fn clear_is_an_empty_v1_update() {
        let encoded = encoded(&ClipboardData::default(), MAX).await.unwrap();
        assert_eq!(encoded, b"LMCB\x01\x00");
        assert_eq!(
            decoded(&encoded).await,
            Some((ClipboardData::default(), vec![]))
        );
    }

    #[tokio::test]
    async fn roundtrip_with_files() {
        let tmp = TempDir::new();
        let mut data = sample();
        data.set_files(vec![
            tmp.file("a.txt", b"hello"),
            tmp.file("empty", b""),
            tmp.file("bin", &[0, 1, 2, 255]),
        ]);
        let encoded = encoded(&data, MAX).await.unwrap();
        assert_eq!(encoded[4], V2);
        let (items, files) = decoded(&encoded).await.unwrap();
        assert_eq!(items, sample());
        assert_eq!(
            files,
            [
                ("a.txt".to_string(), b"hello".to_vec()),
                ("empty".to_string(), vec![]),
                ("bin".to_string(), vec![0, 1, 2, 255]),
            ]
        );
    }

    #[tokio::test]
    async fn rejects_truncated_and_trailing_bytes() {
        let tmp = TempDir::new();
        let mut with_files = sample();
        with_files.set_files(vec![tmp.file("f", b"xyz")]);
        for data in [sample(), with_files] {
            let encoded = encoded(&data, MAX).await.unwrap();
            for len in 0..encoded.len() {
                assert!(
                    decoded(&encoded[..len]).await.is_none(),
                    "prefix of {len} bytes"
                );
            }
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert!(decoded(&trailing).await.is_none());
        }
    }

    #[tokio::test]
    async fn rejects_unknown_version() {
        let mut encoded = encoded(&sample(), MAX).await.unwrap();
        encoded[4] = 3;
        assert!(decoded(&encoded).await.is_none());
    }

    #[tokio::test]
    async fn file_size_cap_boundary() {
        let tmp = TempDir::new();
        let half = vec![7; MAX as usize / 2];
        let mut at_cap = ClipboardData::default();
        at_cap.set_files(vec![tmp.file("a", &half), tmp.file("b", &half)]);
        let encoded = encoded(&at_cap, MAX).await.unwrap();
        assert!(decoded(&encoded).await.is_some());
        assert!(decode(&encoded, MAX - 1).await.is_none());

        let mut over = ClipboardData::default();
        over.set_files(vec![
            tmp.file("c", &half),
            tmp.file("d", &[7; MAX as usize / 2 + 1]),
        ]);
        assert!(matches!(encode(&over, MAX), Err(TransportError::TooLarge(n)) if n == MAX + 1));
        let frame = v2_frame(&[("c", &half), ("d", &[7; MAX as usize / 2 + 1])]);
        assert!(decoded(&frame).await.is_none());
    }

    #[tokio::test]
    async fn changed_file_is_not_sent() {
        let tmp = TempDir::new();
        let mut data = ClipboardData::default();
        data.set_files(vec![tmp.file("a", b"small")]);
        let path = tmp.path().join("a");
        // grown past the limit after it was copied
        std::fs::write(&path, vec![0; MAX as usize + 1]).unwrap();
        assert!(matches!(
            encoded(&data, MAX).await,
            Err(TransportError::TooLarge(_))
        ));
        // grown or shrunk while it is sent
        for contents in [&b"larger"[..], b"tiny"] {
            std::fs::write(&path, b"small").unwrap();
            let encoded = encode(&data, MAX).unwrap();
            let opened = open(&encoded.paths(), MAX).unwrap();
            std::fs::write(&path, contents).unwrap();
            let sent = encoded.write(&mut Vec::new(), opened).await;
            assert!(
                matches!(sent, Err(TransportError::FileChanged)),
                "{contents:?}"
            );
        }
    }

    /// C7: a large file is sent and received `CHUNK` bytes at a time, never
    /// held whole
    #[tokio::test]
    async fn files_are_streamed_in_chunks() {
        let tmp = TempDir::new();
        let contents = vec![3; 4 * CHUNK + 7];
        let max = contents.len() as u64;
        let mut data = ClipboardData::default();
        data.set_files(vec![tmp.file("big", &contents)]);

        let encoded = encode(&data, max).unwrap();
        let opened = open(&encoded.paths(), max).unwrap();
        let mut sent = Recorder::default();
        encoded.write(&mut sent, opened).await.unwrap();
        assert!(sent.largest_write <= CHUNK, "{} bytes", sent.largest_write);

        let mut files = Memory::default();
        let len = sent.written.len() as u64;
        read(&mut &sent.written[..], len, max, &mut files)
            .await
            .unwrap();
        assert!(
            files.largest_chunk <= CHUNK,
            "{} bytes",
            files.largest_chunk
        );
        assert_eq!(files.files, [("big".to_string(), contents)]);
    }

    #[tokio::test]
    async fn rejects_unsafe_file_names() {
        assert!(decoded(&v2_frame(&[("ok.txt", b"x")])).await.is_some());
        for name in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "/etc/passwd",
            "..\\x",
            "C:\\x",
            "a\0b",
        ] {
            assert!(
                decoded(&v2_frame(&[("ok.txt", b"x"), (name, b"x")]))
                    .await
                    .is_none(),
                "{name:?}"
            );
        }
    }
}

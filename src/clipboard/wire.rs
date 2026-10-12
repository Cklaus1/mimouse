//! Wire format of a clipboard update (MiMouse <-> MiMouse only):
//!
//! v1: `"LMCB" | 1 | count: u8 | count * (mime_len: u8 | mime | len: u32 BE | data)`
//!
//! v2: the v1 layout with version 2, followed by
//! `file_count: u16 BE | file_count * (name_len: u16 BE | name | len: u64 BE | data)`.
//! Names are UTF-8 base names (see [`valid_name`]).
//!
//! Updates without files are sent as v1, so older MiMouse peers keep
//! receiving text and images.

use std::{fs::File, io::Read};

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

/// a decoded update; file contents borrow from the received frame
pub(super) struct Frame<'a> {
    pub(super) data: ClipboardData,
    pub(super) files: Vec<(&'a str, &'a [u8])>,
}

/// longest frame a peer may send when copied files may total `max_file_size`
pub(super) fn max_frame_len(max_file_size: u64) -> u64 {
    let header = (MAGIC.len() + 2) as u64;
    let items = (MAX_ITEMS * (1 + MAX_MIME_LEN + 4) + MAX_CLIPBOARD_SIZE) as u64;
    let files = (2 + MAX_FILES * (2 + MAX_NAME_LEN + 8)) as u64;
    header + items + files + max_file_size
}

/// encode `data`, reading the contents of its files (blocking);
/// fails if the files total more than `max_file_size` bytes
pub(super) fn encode(data: &ClipboardData, max_file_size: u64) -> Result<Vec<u8>, TransportError> {
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
    let capacity =
        6 + data.size() + 5 * items.len() + 2 + names.iter().map(|n| n.len() + 10).sum::<usize>();
    let mut buf = Vec::with_capacity(capacity + usize::try_from(files_size).unwrap_or(0));
    buf.extend_from_slice(MAGIC);
    buf.push(if data.files.is_empty() { V1 } else { V2 });
    buf.push(items.len() as u8);
    for (mime, bytes) in items {
        buf.push(mime.len() as u8);
        buf.extend_from_slice(mime.as_bytes());
        buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
    if data.files.is_empty() {
        return Ok(buf);
    }
    buf.extend_from_slice(&(data.files.len() as u16).to_be_bytes());
    // files may have grown since they were copied: read at most one byte
    // more than the remaining budget to notice
    let mut budget = max_file_size;
    for (file, name) in data.files.iter().zip(names) {
        buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
        buf.extend_from_slice(name.as_bytes());
        let len_at = buf.len();
        buf.extend_from_slice(&[0; 8]);
        let len = File::open(&file.path)?
            .take(budget + 1)
            .read_to_end(&mut buf)? as u64;
        if len > budget {
            return Err(TransportError::TooLarge(max_file_size - budget + len));
        }
        budget -= len;
        buf[len_at..len_at + 8].copy_from_slice(&len.to_be_bytes());
    }
    Ok(buf)
}

/// decode an update; `None` if it is malformed, has invalid file names, or
/// exceeds the size limits
pub(super) fn decode(buf: &[u8], max_file_size: u64) -> Option<Frame<'_>> {
    let rest = buf.strip_prefix(MAGIC)?;
    let (&version, rest) = rest.split_first()?;
    if version != V1 && version != V2 {
        return None;
    }
    let (&count, mut rest) = rest.split_first()?;
    let mut data = ClipboardData::default();
    for _ in 0..count {
        let (&mime_len, r) = rest.split_first()?;
        let (mime, r) = r.split_at_checked(mime_len as usize)?;
        let (len, r) = r.split_first_chunk::<4>()?;
        let (bytes, r) = r.split_at_checked(u32::from_be_bytes(*len) as usize)?;
        rest = r;
        data.push(std::str::from_utf8(mime).ok()?, bytes.to_vec());
    }
    if data.size() > MAX_CLIPBOARD_SIZE {
        return None;
    }
    let mut files = Vec::new();
    if version == V2 {
        let (count, r) = rest.split_first_chunk::<2>()?;
        rest = r;
        let mut total: u64 = 0;
        for _ in 0..u16::from_be_bytes(*count) {
            let (name_len, r) = rest.split_first_chunk::<2>()?;
            let (name, r) = r.split_at_checked(u16::from_be_bytes(*name_len) as usize)?;
            let name = std::str::from_utf8(name).ok().filter(|n| valid_name(n))?;
            let (len, r) = r.split_first_chunk::<8>()?;
            let len = u64::from_be_bytes(*len);
            total = total.checked_add(len).filter(|&t| t <= max_file_size)?;
            let (bytes, r) = r.split_at_checked(usize::try_from(len).ok()?)?;
            rest = r;
            files.push((name, bytes));
        }
    }
    rest.is_empty().then_some(Frame { data, files })
}

#[cfg(test)]
mod tests {
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

    type Decoded = (ClipboardData, Vec<(String, Vec<u8>)>);

    fn decoded(buf: &[u8]) -> Option<Decoded> {
        decode(buf, MAX).map(|f| {
            let files = f
                .files
                .iter()
                .map(|(n, d)| (n.to_string(), d.to_vec()))
                .collect();
            (f.data, files)
        })
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

    #[test]
    fn roundtrip() {
        let data = sample();
        let encoded = encode(&data, MAX).unwrap();
        // no files: readable by MiMouse peers that only know v1
        assert_eq!(encoded[4], V1);
        assert_eq!(decoded(&encoded), Some((data, vec![])));
    }

    #[test]
    fn roundtrip_with_files() {
        let tmp = TempDir::new();
        let mut data = sample();
        data.set_files(vec![
            tmp.file("a.txt", b"hello"),
            tmp.file("empty", b""),
            tmp.file("bin", &[0, 1, 2, 255]),
        ]);
        let encoded = encode(&data, MAX).unwrap();
        assert_eq!(encoded[4], V2);
        let (items, files) = decoded(&encoded).unwrap();
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

    #[test]
    fn rejects_truncated_and_trailing_bytes() {
        let tmp = TempDir::new();
        let mut with_files = sample();
        with_files.set_files(vec![tmp.file("f", b"xyz")]);
        for data in [sample(), with_files] {
            let encoded = encode(&data, MAX).unwrap();
            for len in 0..encoded.len() {
                assert!(decoded(&encoded[..len]).is_none(), "prefix of {len} bytes");
            }
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert!(decoded(&trailing).is_none());
        }
    }

    #[test]
    fn rejects_unknown_version() {
        let mut encoded = encode(&sample(), MAX).unwrap();
        encoded[4] = 3;
        assert!(decoded(&encoded).is_none());
    }

    #[test]
    fn file_size_cap_boundary() {
        let tmp = TempDir::new();
        let half = vec![7; MAX as usize / 2];
        let mut at_cap = ClipboardData::default();
        at_cap.set_files(vec![tmp.file("a", &half), tmp.file("b", &half)]);
        let encoded = encode(&at_cap, MAX).unwrap();
        assert!(decoded(&encoded).is_some());
        assert!(decode(&encoded, MAX - 1).is_none());

        let mut over = ClipboardData::default();
        over.set_files(vec![
            tmp.file("c", &half),
            tmp.file("d", &[7; MAX as usize / 2 + 1]),
        ]);
        assert!(matches!(encode(&over, MAX), Err(TransportError::TooLarge(n)) if n == MAX + 1));
        assert!(
            decoded(&v2_frame(&[
                ("c", &half),
                ("d", &[7; MAX as usize / 2 + 1])
            ]))
            .is_none()
        );
    }

    #[test]
    fn file_grown_after_copy_is_not_sent() {
        let tmp = TempDir::new();
        let mut data = ClipboardData::default();
        data.set_files(vec![tmp.file("a", b"small")]);
        std::fs::write(tmp.path().join("a"), vec![0; MAX as usize + 1]).unwrap();
        assert!(matches!(
            encode(&data, MAX),
            Err(TransportError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_unsafe_file_names() {
        assert!(decoded(&v2_frame(&[("ok.txt", b"x")])).is_some());
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
                decoded(&v2_frame(&[("ok.txt", b"x"), (name, b"x")])).is_none(),
                "{name:?}"
            );
        }
    }
}

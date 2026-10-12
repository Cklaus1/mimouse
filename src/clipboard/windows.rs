//! Windows clipboard backend.
//!
//! A monitor thread receives `WM_CLIPBOARDUPDATE` and reads text
//! (`CF_UNICODETEXT`), HTML (`HTML Format`), images (`PNG`, falling back to
//! `CF_BITMAP` converted to PNG) and copied files (`CF_HDROP`). A writer
//! thread sets remote contents.
//! Our own writes are recognized by their clipboard sequence number.

use std::{
    io::Cursor,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use clipboard_win::{
    Clipboard, Getter, Monitor, SysResult,
    formats::{CF_DIB, CF_HDROP, CF_UNICODETEXT, FileList, Html, RawData, Unicode},
    options::NoClear,
    raw,
};
use image::ImageFormat;
use tokio::sync::mpsc;

use super::{
    BackendTx, ClipboardData, MAX_CLIPBOARD_SIZE, MIME_HTML, MIME_PNG, MIME_TEXT,
    files::local_files,
};

const OPEN_ATTEMPTS: usize = 20;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(25);
// WM_CLIPBOARDUPDATE can arrive while the owner is still rendering delayed
// formats (e.g. OleSetClipboard followed by OleFlushClipboard); reading then
// fails with OS error 1418 and no further update is posted for that change
const READ_ATTEMPTS: usize = 10;
const READ_RETRY_DELAY: Duration = Duration::from_millis(50);

pub(super) fn spawn(changes: BackendTx) -> BackendTx {
    let (tx, rx) = mpsc::unbounded_channel();
    // sequence number of the clipboard after our latest write; the lock
    // serializes clipboard access between the two threads, so the monitor
    // sees the final value of a write before deciding whether it was ours
    let own_seq = Arc::new(Mutex::new(0u32));
    let seq = own_seq.clone();
    thread::Builder::new()
        .name("clipboard-monitor".into())
        .spawn(move || monitor(changes, seq))
        .expect("spawn thread");
    thread::Builder::new()
        .name("clipboard-writer".into())
        .spawn(move || writer(rx, own_seq))
        .expect("spawn thread");
    tx
}

fn seq_num() -> u32 {
    raw::seq_num().map_or(0, |n| n.get())
}

fn monitor(changes: BackendTx, own_seq: Arc<Mutex<u32>>) {
    let mut monitor = match Monitor::new() {
        Ok(m) => m,
        Err(e) => return log::warn!("clipboard: failed to monitor clipboard: {e}"),
    };
    // returns false once the receiving end is gone
    let report = || {
        let own = own_seq.lock().expect("lock");
        if *own == seq_num() {
            return true;
        }
        let data = read_settled();
        drop(own);
        match data {
            Ok(data) if data.is_empty() => true,
            Ok(data) => changes.send(Arc::new(data)).is_ok(),
            Err(e) => {
                log::warn!("clipboard: failed to read clipboard: {e}");
                true
            }
        }
    };
    if !report() {
        return;
    }
    loop {
        match monitor.recv() {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => return log::warn!("clipboard: monitor failed: {e}"),
        }
        if !report() {
            return;
        }
    }
}

fn read_settled() -> SysResult<ClipboardData> {
    let mut attempts = READ_ATTEMPTS;
    loop {
        match read() {
            Err(e) if attempts > 1 => {
                log::debug!("clipboard: read failed ({e}), retrying");
                attempts -= 1;
                thread::sleep(READ_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

fn read() -> SysResult<ClipboardData> {
    let mut data = ClipboardData::default();
    let mut bitmap = None;
    let mut paths: Vec<String> = Vec::new();
    {
        let _clipboard = open()?;
        if raw::is_format_avail(CF_UNICODETEXT) {
            let mut text = String::new();
            Unicode.read_clipboard(&mut text)?;
            data.push(MIME_TEXT, text.trim_end_matches('\0').as_bytes().to_vec());
        }
        if let Some(html) = Html::new().filter(|h| raw::is_format_avail(h.code())) {
            let mut fragment = Vec::new();
            match html.read_clipboard(&mut fragment) {
                Ok(_) => data.push(MIME_HTML, fragment),
                Err(e) => log::debug!("clipboard: invalid HTML Format: {e}"),
            }
        }
        if let Some(png) = png_format().filter(|f| raw::is_format_avail(*f)) {
            let mut bytes = Vec::new();
            RawData(png).read_clipboard(&mut bytes)?;
            data.push(MIME_PNG, bytes);
        } else if raw::is_format_avail(CF_DIB) {
            // CF_DIB is synthesized by Windows from CF_BITMAP / CF_DIBV5;
            // clipboard-win's CF_BITMAP getter fails here with OS error 1418
            let mut dib = Vec::new();
            RawData(CF_DIB).read_clipboard(&mut dib)?;
            bitmap = dib_to_bmp(dib);
        }
        if raw::is_format_avail(CF_HDROP) {
            FileList.read_clipboard(&mut paths)?;
        }
    }
    if let Some(bmp) = bitmap {
        match convert(&bmp, ImageFormat::Bmp, ImageFormat::Png) {
            Ok(png) => data.push(MIME_PNG, png),
            Err(e) => log::warn!("clipboard: failed to convert bitmap: {e}"),
        }
    }
    if data.size() > MAX_CLIPBOARD_SIZE {
        log::warn!("clipboard: contents too large ({} bytes)", data.size());
        return Ok(ClipboardData::default());
    }
    // stat outside the clipboard lock: paths may be on slow network shares
    data.set_files(local_files(paths.into_iter().map(Into::into).collect()));
    Ok(data)
}

fn writer(mut rx: mpsc::UnboundedReceiver<Arc<ClipboardData>>, own_seq: Arc<Mutex<u32>>) {
    while let Some(data) = rx.blocking_recv() {
        let text = data.get(MIME_TEXT).map(String::from_utf8_lossy);
        let html = data.get(MIME_HTML).map(String::from_utf8_lossy);
        let png = data.get(MIME_PNG);
        let bmp = png.and_then(
            |png| match convert(png, ImageFormat::Png, ImageFormat::Bmp) {
                Ok(bmp) => Some(bmp),
                Err(e) => {
                    log::warn!("clipboard: failed to convert image: {e}");
                    None
                }
            },
        );
        let files: Vec<&str> = data
            .files()
            .iter()
            .filter_map(|f| {
                let path = f.path.to_str();
                if path.is_none() {
                    log::warn!("clipboard: cannot offer {}", f.path.display());
                }
                path
            })
            .collect();

        let mut seq = own_seq.lock().expect("lock");
        let result = (|| -> SysResult<()> {
            let _clipboard = open()?;
            raw::empty()?;
            if let Some(text) = &text {
                raw::set_string_with(text, NoClear)?;
            }
            if let Some((html, format)) = html.as_ref().zip(Html::new()) {
                raw::set_html_with(format.code(), html, NoClear)?;
            }
            if let Some((png, format)) = png.zip(png_format()) {
                raw::set_without_clear(format, png)?;
            }
            if let Some(bmp) = &bmp {
                raw::set_bitmap_with(bmp, NoClear)?;
            }
            if !files.is_empty() {
                raw::set_file_list_with(&files, NoClear)?;
            }
            Ok(())
        })();
        // the clipboard is closed now: this is the sequence number
        // the monitor will see for our write
        *seq = seq_num();
        drop(seq);
        if let Err(e) = result {
            log::warn!("clipboard: failed to set clipboard: {e}");
        }
    }
}

/// open the clipboard, waiting for other applications to close it
fn open() -> SysResult<Clipboard> {
    let mut attempts = OPEN_ATTEMPTS;
    loop {
        match Clipboard::new() {
            Ok(clipboard) => return Ok(clipboard),
            Err(e) if attempts == 0 => return Err(e),
            Err(_) => attempts -= 1,
        }
        thread::sleep(OPEN_RETRY_DELAY);
    }
}

fn png_format() -> Option<u32> {
    raw::register_format("PNG").map(|f| f.get())
}

/// prepend a BITMAPFILEHEADER to a packed DIB (BITMAPINFO + pixels)
fn dib_to_bmp(dib: Vec<u8>) -> Option<Vec<u8>> {
    const FILE_HEADER_SIZE: u32 = 14;
    const BI_BITFIELDS: u32 = 3;
    let u32_at = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            dib.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let header_size = u32_at(0)?;
    let bit_count = u32::from(u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?));
    let compression = u32_at(16)?;
    let colors_used = u32_at(32)?;
    let palette = match colors_used {
        0 if bit_count <= 8 => 1 << bit_count,
        n => n,
    };
    // BITMAPINFOHEADER with BI_BITFIELDS is followed by three color masks
    let masks = if header_size == 40 && compression == BI_BITFIELDS {
        12
    } else {
        0
    };
    let pixel_offset = FILE_HEADER_SIZE + header_size + masks + 4 * palette;
    let file_size = FILE_HEADER_SIZE + u32::try_from(dib.len()).ok()?;
    let mut bmp = Vec::with_capacity(file_size as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&pixel_offset.to_le_bytes());
    bmp.extend_from_slice(&dib);
    Some(bmp)
}

fn convert(bytes: &[u8], from: ImageFormat, to: ImageFormat) -> image::ImageResult<Vec<u8>> {
    let image = image::load_from_memory_with_format(bytes, from)?;
    // CF_BITMAP has no (reliable) alpha channel
    let image = match to {
        ImageFormat::Bmp => image::DynamicImage::ImageRgb8(image.to_rgb8()),
        _ => image,
    };
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, to)?;
    Ok(out.into_inner())
}

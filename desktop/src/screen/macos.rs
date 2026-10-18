//! `/usr/sbin/screencapture`, one PNG per display, composited at the largest
//! backing scale.

use std::{path::PathBuf, process::Stdio, time::Duration};

use core_graphics::display::CGDisplay;
use image::{RgbaImage, imageops};

use super::{Display, composite};
use crate::{Geometry, Image, ScreenError};

const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
/// `doctor::run_command`'s rule (timeout + `kill_on_drop`), which lives in the
/// root crate and is unreachable from here
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Backend;

impl Backend {
    pub(crate) const METHOD: Option<&'static str> = Some("screencapture");

    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) async fn capture(&self) -> Result<Image, ScreenError> {
        let displays = blocking(displays).await??;
        let dir = blocking(|| {
            tempfile::Builder::new()
                .prefix("mimouse-screen")
                .tempdir()
                .map_err(|e| ScreenError::Failed(format!("creating a temp dir: {e}")))
        })
        .await??;
        let files: Vec<PathBuf> = (1..=displays.len())
            .map(|n| dir.path().join(format!("{n}.png")))
            .collect();
        // -D n: the n-th display of the active display list (main first)
        for (n, file) in files.iter().enumerate() {
            screencapture(n + 1, file.clone()).await?;
        }
        blocking(move || {
            let image = composite_files(&displays, &files);
            // removes the PNGs
            drop(dir);
            image
        })
        .await?
    }

    pub(crate) async fn geometry(&self) -> Result<Geometry, ScreenError> {
        let displays = blocking(displays).await??;
        Ok(composite(&displays))
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ScreenError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ScreenError::Failed(format!("capture thread: {e}")))
}

/// active displays, the main display first
fn displays() -> Result<Vec<Display>, ScreenError> {
    let ids = CGDisplay::active_displays()
        .map_err(|e| ScreenError::Failed(format!("CGGetActiveDisplayList: error {e}")))?;
    if ids.is_empty() {
        return Err(ScreenError::Unavailable("no active display".into()));
    }
    Ok(ids
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            let scale = display
                .display_mode()
                .filter(|mode| mode.width() > 0)
                .map_or(1.0, |mode| mode.pixel_width() as f64 / mode.width() as f64);
            Display {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                scale,
            }
        })
        .collect())
}

async fn screencapture(display: usize, file: PathBuf) -> Result<(), ScreenError> {
    let mut command = tokio::process::Command::new(SCREENCAPTURE);
    command
        .args(["-x", "-t", "png", "-D", &display.to_string()])
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Err(_) => {
            return Err(ScreenError::Failed(format!(
                "{SCREENCAPTURE} timed out after {COMMAND_TIMEOUT:?}"
            )));
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ScreenError::Unsupported(format!("{SCREENCAPTURE}: {e}")));
        }
        Ok(Err(e)) => return Err(ScreenError::Failed(format!("{SCREENCAPTURE}: {e}"))),
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        // display asleep, locked session or no Screen Recording permission
        return Err(ScreenError::Unavailable(format!(
            "{SCREENCAPTURE} -D {display}: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn composite_files(displays: &[Display], files: &[PathBuf]) -> Result<Image, ScreenError> {
    let geometry = composite(displays);
    let mut out =
        RgbaImage::from_pixel(geometry.width, geometry.height, image::Rgba([0, 0, 0, 255]));
    for (rect, file) in geometry.monitors.iter().zip(files) {
        let shot = image::open(file)
            .map_err(|e| ScreenError::Failed(format!("reading {}: {e}", file.display())))?
            .into_rgba8();
        let shot = if shot.dimensions() == (rect.width, rect.height) {
            shot
        } else {
            // a display below the largest backing scale
            imageops::resize(
                &shot,
                rect.width,
                rect.height,
                imageops::FilterType::Triangle,
            )
        };
        imageops::replace(&mut out, &shot, i64::from(rect.x), i64::from(rect.y));
    }
    Ok(Image {
        width: out.width(),
        height: out.height(),
        rgba: out.into_raw(),
    })
}

//! This desktop as an agent sees it: screenshots in desktop pixels, the lock
//! and idle state.
//!
//! Desktop pixels: the space of [`Screen::capture`] at full resolution, origin
//! at the top-left corner of the bounding box of all monitors (physical
//! pixels on Windows, the screenshot's pixels elsewhere).

use std::time::Duration;

use thiserror::Error;

/// an RGBA8 image, row-major, `width * height * 4` bytes
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

/// a rectangle in desktop pixels
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// size of the desktop and its monitors, in desktop pixels
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub monitors: Vec<Rect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Png,
    Jpeg,
}

/// an encoded (cropped, scaled) screenshot
#[derive(Clone, PartialEq)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// desktop pixels per image pixel
    pub scale: f64,
    /// desktop pixel of the image's top-left corner
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Error)]
pub enum ScreenError {
    /// this desktop can never be captured this way
    #[error("screen capture unsupported: {0}")]
    Unsupported(String),
    /// not now: locked, secure desktop, display off
    #[error("screen unavailable: {0}")]
    Unavailable(String),
    #[error("screen capture failed: {0}")]
    Failed(String),
}

/// captures this desktop
pub struct Screen {}

impl Screen {
    pub fn new() -> Self {
        Self {}
    }

    /// name of the capture method (`gdi`, `portal`, `screencapture`)
    pub fn method(&self) -> Option<&'static str> {
        None
    }

    /// the whole desktop at full resolution
    pub async fn capture(&self) -> Result<Image, ScreenError> {
        Err(ScreenError::Unsupported("not built".into()))
    }

    /// size and monitors of the desktop
    pub async fn geometry(&self) -> Result<Geometry, ScreenError> {
        Err(ScreenError::Unsupported("not built".into()))
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

/// Crops `image` to `region` (desktop pixels, clamped to the image),
/// scales it down so its longest edge is at most `max_edge`, and encodes it.
pub fn encode(
    image: &Image,
    region: Option<Rect>,
    max_edge: Option<u32>,
    format: Format,
) -> Result<Encoded, ScreenError> {
    let _ = (image, region, max_edge, format);
    Err(ScreenError::Unsupported("not built".into()))
}

/// Captures until the screen has not changed for `settle` or `timeout`
/// passed, and returns the last image.
pub async fn settle(
    screen: &Screen,
    settle: Duration,
    timeout: Duration,
) -> Result<Image, ScreenError> {
    let _ = (settle, timeout);
    screen.capture().await
}

/// screen locked or secure desktop shown; `None`: unknown here
pub async fn locked() -> Option<bool> {
    None
}

/// time since the last input of any kind; `None`: unknown here
pub async fn idle() -> Option<Duration> {
    None
}

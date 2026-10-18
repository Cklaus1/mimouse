use std::{
    future::Future,
    hash::{DefaultHasher, Hasher},
    time::Duration,
};

use tokio::time::Instant;

use crate::{Image, ScreenError};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(unix, not(target_os = "macos")))]
mod portal;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub(crate) use macos::Backend;
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use portal::Backend;
#[cfg(windows)]
pub(crate) use windows::Backend;

#[cfg(not(any(unix, windows)))]
pub(crate) use unsupported::Backend;

#[cfg(not(any(unix, windows)))]
mod unsupported {
    use crate::{Geometry, Image, ScreenError};

    pub(crate) struct Backend;

    impl Backend {
        pub(crate) const METHOD: Option<&'static str> = None;

        pub(crate) fn new() -> Self {
            Self
        }

        pub(crate) async fn capture(&self) -> Result<Image, ScreenError> {
            Err(ScreenError::Unsupported(
                "no capture method on this OS".into(),
            ))
        }

        pub(crate) async fn geometry(&self) -> Result<Geometry, ScreenError> {
            Err(ScreenError::Unsupported(
                "no capture method on this OS".into(),
            ))
        }
    }
}

/// [`crate::settle`] over any capture function
pub(crate) async fn settle<F, Fut>(
    mut capture: F,
    settle: Duration,
    timeout: Duration,
) -> Result<Image, ScreenError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Image, ScreenError>>,
{
    let deadline = Instant::now() + timeout;
    let last = capture().await?;
    if settle.is_zero() {
        return Ok(last);
    }
    let mut last_hash = hash(last).await?;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(last_hash.0);
        }
        let waited = settle.min(deadline - now);
        tokio::time::sleep(waited).await;
        let next = hash(capture().await?).await?;
        if next.1 == last_hash.1 && waited == settle {
            return Ok(next.0);
        }
        last_hash = next;
    }
}

/// the image and a hash of its pixels, computed off the async thread
async fn hash(image: Image) -> Result<(Image, u64), ScreenError> {
    tokio::task::spawn_blocking(move || {
        let mut hasher = DefaultHasher::new();
        hasher.write_u32(image.width);
        hasher.write_u32(image.height);
        hasher.write(&image.rgba);
        let hash = hasher.finish();
        (image, hash)
    })
    .await
    .map_err(|e| ScreenError::Failed(format!("hashing the screenshot: {e}")))
}

/// one display of a desktop composited from per-display captures
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Display {
    /// bounds in global points
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// pixels per point
    pub scale: f64,
}

/// geometry of the composite of `displays`: their bounding box at the largest
/// backing scale, origin at its top-left corner
#[cfg(any(target_os = "macos", test))]
pub(crate) fn composite(displays: &[Display]) -> crate::Geometry {
    if displays.is_empty() {
        return crate::Geometry {
            width: 0,
            height: 0,
            monitors: Vec::new(),
        };
    }
    let scale = displays.iter().map(|d| d.scale).fold(1.0, f64::max);
    let left = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
    let top = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
    let right = displays
        .iter()
        .map(|d| d.x + d.width)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = displays
        .iter()
        .map(|d| d.y + d.height)
        .fold(f64::NEG_INFINITY, f64::max);
    let px = |points: f64| (points * scale).round();
    let monitors = displays
        .iter()
        .map(|d| {
            let (x0, y0) = (px(d.x - left), px(d.y - top));
            let (x1, y1) = (px(d.x + d.width - left), px(d.y + d.height - top));
            crate::Rect {
                x: x0 as i32,
                y: y0 as i32,
                width: (x1 - x0) as u32,
                height: (y1 - y0) as u32,
            }
        })
        .collect();
    crate::Geometry {
        width: px(right - left) as u32,
        height: px(bottom - top) as u32,
        monitors,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{Geometry, Rect};

    fn frame(value: u8) -> Image {
        Image {
            width: 2,
            height: 1,
            rgba: vec![value; 8],
        }
    }

    /// a capture function returning `frames` in order (the last repeats),
    /// recording when each capture started
    #[allow(clippy::type_complexity)] // test-only plumbing
    fn script(
        frames: Vec<Result<u8, ScreenError>>,
    ) -> (
        impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = Result<Image, ScreenError>>>>,
        Arc<Mutex<Vec<Duration>>>,
    ) {
        let start = Instant::now();
        let frames = Arc::new(Mutex::new(VecDeque::from(frames)));
        let times = Arc::new(Mutex::new(Vec::new()));
        let recorded = times.clone();
        let capture = move || {
            let frames = frames.clone();
            let times = times.clone();
            Box::pin(async move {
                times.lock().unwrap().push(start.elapsed());
                // a capture takes time, like the portal's
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut frames = frames.lock().unwrap();
                let next = if frames.len() > 1 {
                    frames.pop_front().unwrap()
                } else {
                    match frames.front().unwrap() {
                        Ok(v) => Ok(*v),
                        Err(e) => Err(ScreenError::Failed(e.to_string())),
                    }
                };
                next.map(frame)
            }) as std::pin::Pin<Box<dyn Future<Output = _>>>
        };
        (capture, recorded)
    }

    #[allow(non_snake_case)] // reads like a unit
    fn MS(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[tokio::test(start_paused = true)]
    async fn returns_once_two_captures_settle_apart_are_equal() {
        let (capture, times) = script(vec![Ok(1), Ok(2), Ok(3), Ok(3)]);
        let image = settle(capture, MS(500), MS(10_000)).await.unwrap();
        assert_eq!(image, frame(3));
        assert_eq!(
            *times.lock().unwrap(),
            vec![MS(0), MS(600), MS(1200), MS(1800)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn returns_the_last_capture_at_the_timeout() {
        let (capture, times) = script(vec![Ok(1), Ok(2), Ok(3), Ok(4), Ok(5)]);
        let image = settle(capture, MS(500), MS(1500)).await.unwrap();
        // captures end at 100, 700, 1300; the next wait is cut to 200 ms and
        // that capture is returned without another wait
        assert_eq!(
            *times.lock().unwrap(),
            vec![MS(0), MS(600), MS(1200), MS(1500)]
        );
        assert_eq!(image, frame(4));
    }

    #[tokio::test(start_paused = true)]
    async fn a_shortened_wait_does_not_count_as_settled() {
        let (capture, times) = script(vec![Ok(1), Ok(1)]);
        let image = settle(capture, MS(500), MS(200)).await.unwrap();
        assert_eq!(image, frame(1));
        assert_eq!(*times.lock().unwrap(), vec![MS(0), MS(200)]);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_settle_returns_the_first_capture() {
        let (capture, times) = script(vec![Ok(1), Ok(2)]);
        assert_eq!(settle(capture, MS(0), MS(5000)).await.unwrap(), frame(1));
        assert_eq!(times.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_error_ends_the_wait() {
        let (capture, _) = script(vec![
            Ok(1),
            Err(ScreenError::Unavailable("locked".into())),
            Ok(1),
        ]);
        let err = settle(capture, MS(500), MS(5000)).await.unwrap_err();
        assert!(matches!(err, ScreenError::Unavailable(_)), "{err}");
    }

    #[test]
    fn composite_uses_the_largest_scale_and_the_bounding_box() {
        let displays = [
            Display {
                x: 0.0,
                y: 0.0,
                width: 1512.0,
                height: 982.0,
                scale: 2.0,
            },
            Display {
                x: -1920.0,
                y: -98.0,
                width: 1920.0,
                height: 1080.0,
                scale: 1.0,
            },
        ];
        let geometry = composite(&displays);
        assert_eq!(
            geometry,
            Geometry {
                width: 6864,
                height: 2160,
                monitors: vec![
                    Rect {
                        x: 3840,
                        y: 196,
                        width: 3024,
                        height: 1964,
                    },
                    Rect {
                        x: 0,
                        y: 0,
                        width: 3840,
                        height: 2160,
                    },
                ],
            }
        );
    }
}

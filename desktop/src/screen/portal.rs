//! xdg-desktop-portal Screenshot: one PNG file per capture, read and deleted.
//! GNOME's DisplayConfig gives the monitor layout.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use ashpd::{
    PortalError,
    desktop::{ResponseError, screenshot::Screenshot},
    zbus::{self, Connection, zvariant::OwnedValue},
};

use crate::{Geometry, Image, Rect, ScreenError};

pub(crate) struct Backend {
    /// size of the last capture
    size: Mutex<Option<(u32, u32)>>,
}

impl Backend {
    pub(crate) const METHOD: Option<&'static str> = Some("portal");

    pub(crate) fn new() -> Self {
        Self {
            size: Mutex::new(None),
        }
    }

    pub(crate) async fn capture(&self) -> Result<Image, ScreenError> {
        let shot = Screenshot::request()
            .interactive(false)
            .modal(false)
            .send()
            .await
            .and_then(|request| request.response())
            .map_err(portal_error)?;
        let path = file_path(shot.uri().as_str())?;
        let image = tokio::task::spawn_blocking(move || read_and_delete(&path))
            .await
            .map_err(|e| ScreenError::Failed(format!("reading the screenshot: {e}")))??;
        *self.size.lock().unwrap() = Some((image.width, image.height));
        Ok(image)
    }

    pub(crate) async fn geometry(&self) -> Result<Geometry, ScreenError> {
        let cached = *self.size.lock().unwrap();
        let (width, height) = match cached {
            Some(size) => size,
            None => {
                let image = self.capture().await?;
                (image.width, image.height)
            }
        };
        let layout = match gnome_layout().await {
            Ok(layout) => layout,
            Err(e) => {
                log::debug!("org.gnome.Mutter.DisplayConfig: {e}; one monitor assumed");
                Vec::new()
            }
        };
        Ok(scale_layout(&layout, width, height))
    }
}

fn portal_error(e: ashpd::Error) -> ScreenError {
    match &e {
        ashpd::Error::PortalNotFound(_) => ScreenError::Unsupported(e.to_string()),
        ashpd::Error::Zbus(zbus::Error::MethodError(name, ..))
            if [
                "org.freedesktop.DBus.Error.ServiceUnknown",
                "org.freedesktop.DBus.Error.UnknownInterface",
                "org.freedesktop.DBus.Error.UnknownMethod",
                "org.freedesktop.DBus.Error.UnknownObject",
            ]
            .contains(&name.as_str()) =>
        {
            ScreenError::Unsupported(format!("no screenshot portal: {e}"))
        }
        ashpd::Error::Response(ResponseError::Cancelled)
        | ashpd::Error::Portal(PortalError::Cancelled(_)) => {
            ScreenError::Unavailable(format!("screenshot cancelled: {e}"))
        }
        // the portal answers "other" when the permission store denies it
        ashpd::Error::Response(ResponseError::Other)
        | ashpd::Error::Portal(PortalError::NotAllowed(_)) => {
            ScreenError::Unavailable(format!("screenshot denied: {e}"))
        }
        _ => ScreenError::Failed(e.to_string()),
    }
}

/// the local path of a `file://` URI
fn file_path(uri: &str) -> Result<PathBuf, ScreenError> {
    let bad = || ScreenError::Failed(format!("the portal returned {uri:?}, not a local file"));
    let rest = uri.strip_prefix("file://").ok_or_else(bad)?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return Err(bad());
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next(), iter.next()];
            let [Some(hi), Some(lo)] = hex else {
                return Err(bad());
            };
            let digit = |c: u8| char::from(c).to_digit(16);
            let (Some(hi), Some(lo)) = (digit(hi), digit(lo)) else {
                return Err(bad());
            };
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(b);
        }
    }
    if bytes.contains(&0) {
        return Err(bad());
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Reads the PNG the portal wrote at `path`, then deletes it. Anything but a
/// regular file (a symlink, a directory) is neither read nor deleted.
fn read_and_delete(path: &Path) -> Result<Image, ScreenError> {
    let not_a_file = || {
        ScreenError::Failed(format!(
            "the portal's screenshot {} is not a regular file",
            path.display()
        ))
    };
    let io = |e: std::io::Error| {
        ScreenError::Failed(format!("reading the screenshot {}: {e}", path.display()))
    };
    if !path.symlink_metadata().map_err(io)?.file_type().is_file() {
        return Err(not_a_file());
    }
    let bytes = std::fs::read(path).map_err(io);
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => {
            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("deleting the screenshot {}: {e}", path.display());
            }
        }
        Ok(_) => return Err(not_a_file()),
        Err(e) => log::warn!("deleting the screenshot {}: {e}", path.display()),
    }
    let image = image::load_from_memory_with_format(&bytes?, image::ImageFormat::Png)
        .map_err(|e| ScreenError::Failed(format!("decoding the screenshot: {e}")))?
        .into_rgba8();
    Ok(Image {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

type MonitorSpec = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type Logical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type State = (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>);

/// GNOME's logical monitors in layout pixels
async fn gnome_layout() -> zbus::Result<Vec<Rect>> {
    let conn = Connection::session().await?;
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await?;
    let state: State = reply.body().deserialize()?;
    Ok(layout(&state))
}

/// `layout-mode` 1: layout pixels are logical (mode size / scale);
/// 2: physical (mode size)
const LAYOUT_LOGICAL: u32 = 1;

fn layout((_, monitors, logical, properties): &State) -> Vec<Rect> {
    let logical_layout = properties
        .get("layout-mode")
        .and_then(|v| u32::try_from(v).ok())
        .is_none_or(|mode| mode == LAYOUT_LOGICAL);
    logical
        .iter()
        .filter_map(|(x, y, scale, transform, _, specs, _)| {
            let (_, modes, _) = monitors
                .iter()
                .find(|(spec, ..)| Some(spec) == specs.first())?;
            let (_, w, h, ..) = modes.iter().find(|(.., props)| {
                props
                    .get("is-current")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false)
            })?;
            // transforms 1, 3, 5, 7 rotate by 90 or 270 degrees
            let (w, h) = if transform % 2 == 1 {
                (*h, *w)
            } else {
                (*w, *h)
            };
            let (w, h) = if logical_layout && *scale > 0.0 {
                (
                    (f64::from(w) / scale).round() as u32,
                    (f64::from(h) / scale).round() as u32,
                )
            } else {
                (w.max(0) as u32, h.max(0) as u32)
            };
            Some(Rect {
                x: *x,
                y: *y,
                width: w,
                height: h,
            })
        })
        .collect()
}

/// `layout` (any units) mapped uniformly onto a `width`×`height` screenshot of
/// its bounding box; one monitor covering the screenshot if `layout` is empty
fn scale_layout(layout: &[Rect], width: u32, height: u32) -> Geometry {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let left = layout.iter().map(|r| i64::from(r.x)).min();
    let top = layout.iter().map(|r| i64::from(r.y)).min();
    let right = layout
        .iter()
        .map(|r| i64::from(r.x) + i64::from(r.width))
        .max();
    let bottom = layout
        .iter()
        .map(|r| i64::from(r.y) + i64::from(r.height))
        .max();
    let (Some(left), Some(top), Some(right), Some(bottom)) = (left, top, right, bottom) else {
        return Geometry {
            width,
            height,
            monitors: vec![whole],
        };
    };
    if right <= left || bottom <= top {
        return Geometry {
            width,
            height,
            monitors: vec![whole],
        };
    }
    let map = |v: i64, origin: i64, span: i64, size: u32| {
        ((v - origin) * i64::from(size) + span / 2) / span
    };
    let monitors = layout
        .iter()
        .map(|r| {
            let x0 = map(i64::from(r.x), left, right - left, width);
            let y0 = map(i64::from(r.y), top, bottom - top, height);
            let x1 = map(
                i64::from(r.x) + i64::from(r.width),
                left,
                right - left,
                width,
            );
            let y1 = map(
                i64::from(r.y) + i64::from(r.height),
                top,
                bottom - top,
                height,
            );
            // all within 0..=width/height
            Rect {
                x: x0 as i32,
                y: y0 as i32,
                width: (x1 - x0) as u32,
                height: (y1 - y0) as u32,
            }
        })
        .collect();
    Geometry {
        width,
        height,
        monitors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn spec(connector: &str) -> MonitorSpec {
        (connector.into(), "v".into(), "p".into(), "s".into())
    }

    fn mode(w: i32, h: i32, current: bool) -> Mode {
        let mut props = HashMap::new();
        if current {
            props.insert("is-current".into(), OwnedValue::from(true));
        }
        (format!("{w}x{h}"), w, h, 60.0, 1.0, vec![1.0], props)
    }

    fn state(layout_mode: Option<u32>, logical: Vec<Logical>) -> State {
        let monitors = vec![
            (
                spec("DP-1"),
                vec![mode(1920, 1080, false), mode(3840, 2160, true)],
                HashMap::new(),
            ),
            (spec("DP-2"), vec![mode(2560, 1440, true)], HashMap::new()),
        ];
        let mut props = HashMap::new();
        if let Some(m) = layout_mode {
            props.insert("layout-mode".into(), OwnedValue::from(m));
        }
        (1, monitors, logical, props)
    }

    fn logical(x: i32, y: i32, scale: f64, transform: u32, connector: &str) -> Logical {
        (
            x,
            y,
            scale,
            transform,
            false,
            vec![spec(connector)],
            HashMap::new(),
        )
    }

    #[test]
    fn logical_layout_divides_the_current_mode_by_the_scale() {
        let s = state(
            Some(1),
            vec![
                logical(0, 0, 2.0, 0, "DP-1"),
                logical(1920, 0, 1.25, 1, "DP-2"),
            ],
        );
        assert_eq!(
            layout(&s),
            vec![rect(0, 0, 1920, 1080), rect(1920, 0, 1152, 2048)]
        );
        // missing layout-mode: logical
        let s = state(None, vec![logical(0, 0, 2.0, 0, "DP-1")]);
        assert_eq!(layout(&s), vec![rect(0, 0, 1920, 1080)]);
    }

    #[test]
    fn physical_layout_keeps_the_mode_size() {
        let s = state(Some(2), vec![logical(0, 0, 2.0, 0, "DP-1")]);
        assert_eq!(layout(&s), vec![rect(0, 0, 3840, 2160)]);
    }

    #[test]
    fn unknown_monitors_are_skipped() {
        let s = state(Some(1), vec![logical(0, 0, 1.0, 0, "HDMI-9")]);
        assert!(layout(&s).is_empty());
    }

    #[test]
    fn layout_is_scaled_onto_the_screenshot() {
        // GNOME 50 here: two 2560x1440 monitors side by side at scale 1
        let g = scale_layout(
            &[rect(0, 0, 2560, 1440), rect(2560, 0, 2560, 1440)],
            5120,
            1440,
        );
        assert_eq!(
            g.monitors,
            vec![rect(0, 0, 2560, 1440), rect(2560, 0, 2560, 1440)]
        );
        // logical layout with a negative origin, screenshot at scale 2
        let g = scale_layout(
            &[rect(-1920, 100, 1920, 1080), rect(0, 0, 1280, 800)],
            6400,
            2360,
        );
        assert_eq!((g.width, g.height), (6400, 2360));
        assert_eq!(
            g.monitors,
            vec![rect(0, 200, 3840, 2160), rect(3840, 0, 2560, 1600)]
        );
    }

    #[test]
    fn no_layout_is_one_monitor_of_the_screenshot_size() {
        let g = scale_layout(&[], 1024, 768);
        assert_eq!(
            g,
            Geometry {
                width: 1024,
                height: 768,
                monitors: vec![rect(0, 0, 1024, 768)],
            }
        );
    }

    #[test]
    fn file_uris_are_decoded() {
        assert_eq!(
            file_path("file:///home/u/Pictures/Screenshot%20from%202026.png").unwrap(),
            PathBuf::from("/home/u/Pictures/Screenshot from 2026.png")
        );
        assert_eq!(
            file_path("file://localhost/tmp/%C3%A4.png").unwrap(),
            PathBuf::from("/tmp/ä.png")
        );
        for bad in [
            "https://x/y.png",
            "file://host/y.png",
            "file:///a%2",
            "file:///a%zz",
            "file:///a%00b",
        ] {
            assert!(file_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_screenshot_file_is_read_and_deleted() {
        let dir = std::env::temp_dir().join(format!("desktop-portal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]))
            .save(&file)
            .unwrap();
        let image = read_and_delete(&file).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(&image.rgba[..4], &[1, 2, 3, 255]);
        assert!(!file.exists());

        // a symlink is neither followed nor deleted, nor is its target
        let target = dir.join("target.png");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_and_delete(&link).is_err());
        assert!(link.symlink_metadata().is_ok());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        // a directory is not deleted
        assert!(read_and_delete(&dir).is_err());
        assert!(dir.is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Live check on a desktop session; prints what it saw. Run as the
    /// session's user with its D-Bus environment:
    /// `cargo test -p desktop -- --ignored --nocapture live`
    #[tokio::test]
    #[ignore = "needs a desktop session with the Screenshot portal"]
    async fn live() {
        let screen = crate::Screen::new();
        let started = std::time::Instant::now();
        let image = screen.capture().await.unwrap();
        println!("capture: {image:?} in {:?}", started.elapsed());
        let geometry = screen.geometry().await.unwrap();
        println!("geometry: {geometry:?}");
        assert_eq!(
            (geometry.width, geometry.height),
            (image.width, image.height)
        );
        println!("locked: {:?}", crate::locked().await);
        println!("idle: {:?}", crate::idle().await);
        let encoded = crate::encode(&image, None, Some(1568), crate::Format::Jpeg).unwrap();
        println!(
            "jpeg: {}x{} scale {} {} bytes",
            encoded.width,
            encoded.height,
            encoded.scale,
            encoded.bytes.len()
        );
    }
}

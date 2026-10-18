//! GDI capture of the virtual screen in physical pixels: every call runs on a
//! blocking thread made per-monitor-DPI-aware (v2) for its duration.

use windows::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
        GetDIBits, HDC, HMONITOR, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    },
};

use crate::{Geometry, Image, Rect, ScreenError};

pub(crate) struct Backend;

impl Backend {
    pub(crate) const METHOD: Option<&'static str> = Some("gdi");

    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) async fn capture(&self) -> Result<Image, ScreenError> {
        blocking(|| {
            if crate::state::input_desktop_locked() == Some(true) {
                return Err(ScreenError::Unavailable(
                    "the screen is locked or a secure desktop is shown".into(),
                ));
            }
            capture(virtual_screen()?)
        })
        .await
    }

    pub(crate) async fn geometry(&self) -> Result<Geometry, ScreenError> {
        blocking(|| {
            let screen = virtual_screen()?;
            let mut monitors: Vec<Rect> = Vec::new();
            // SAFETY: the callback only runs during this call, while `monitors` lives
            let ok = unsafe {
                EnumDisplayMonitors(
                    None,
                    None,
                    Some(monitor),
                    LPARAM(&mut monitors as *mut Vec<Rect> as isize),
                )
            };
            if !ok.as_bool() {
                return Err(ScreenError::Failed(format!(
                    "EnumDisplayMonitors: {}",
                    std::io::Error::last_os_error()
                )));
            }
            for m in &mut monitors {
                m.x -= screen.x;
                m.y -= screen.y;
            }
            Ok(Geometry {
                width: screen.width,
                height: screen.height,
                monitors,
            })
        })
        .await
    }
}

unsafe extern "system" fn monitor(
    _: HMONITOR,
    _: HDC,
    rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    // SAFETY: data is the Vec passed by EnumDisplayMonitors' caller, rect is
    // valid for this call
    let (monitors, rect) = unsafe { (&mut *(data.0 as *mut Vec<Rect>), &*rect) };
    monitors.push(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right.saturating_sub(rect.left).max(0) as u32,
        height: rect.bottom.saturating_sub(rect.top).max(0) as u32,
    });
    true.into()
}

/// runs `f` on a blocking thread that is per-monitor-DPI-aware (v2) meanwhile
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ScreenError> + Send + 'static,
) -> Result<T, ScreenError> {
    tokio::task::spawn_blocking(move || {
        let _aware = DpiAware::enter();
        f()
    })
    .await
    .map_err(|e| ScreenError::Failed(format!("capture thread: {e}")))?
}

/// per-monitor-DPI-aware v2 until dropped; the pool thread's previous
/// awareness is restored for whatever runs on it next
struct DpiAware(DPI_AWARENESS_CONTEXT);

impl DpiAware {
    fn enter() -> Self {
        // SAFETY: plain call on this thread
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiAware {
    fn drop(&mut self) {
        // a null context: setting it failed, nothing to restore
        if !self.0.0.is_null() {
            // SAFETY: the context SetThreadDpiAwarenessContext returned
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// the virtual screen in physical pixels (the calling thread must be
/// per-monitor-DPI-aware)
fn virtual_screen() -> Result<Rect, ScreenError> {
    // SAFETY: plain queries
    let [x, y, width, height] = [
        SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
        SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN,
    ]
    .map(|m| unsafe { GetSystemMetrics(m) });
    if width <= 0 || height <= 0 {
        return Err(ScreenError::Unavailable("no display attached".into()));
    }
    Ok(Rect {
        x,
        y,
        width: width as u32,
        height: height as u32,
    })
}

fn capture(screen: Rect) -> Result<Image, ScreenError> {
    let (width, height) = (screen.width as i32, screen.height as i32);
    let gdi = |what: &str| ScreenError::Failed(format!("{what} failed"));
    // SAFETY: each handle is checked, used while alive and released by its guard
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(ScreenError::Unavailable("GetDC(NULL) failed".into()));
        }
        let _screen_dc = Guard(|| {
            ReleaseDC(None, screen_dc);
        });
        let mem = CreateCompatibleDC(Some(screen_dc));
        if mem.is_invalid() {
            return Err(gdi("CreateCompatibleDC"));
        }
        let _mem = Guard(|| {
            let _ = DeleteDC(mem);
        });
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_invalid() {
            return Err(gdi("CreateCompatibleBitmap"));
        }
        let _bitmap = Guard(|| {
            let _ = DeleteObject(bitmap.into());
        });
        let previous = SelectObject(mem, bitmap.into());
        let blit = BitBlt(
            mem,
            0,
            0,
            width,
            height,
            Some(screen_dc),
            screen.x,
            screen.y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        // GetDIBits needs the bitmap deselected
        SelectObject(mem, previous);
        blit.map_err(|e| ScreenError::Unavailable(format!("BitBlt: {e}")))?;

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // negative: top-down rows
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; screen.width as usize * screen.height as usize * 4];
        let lines = GetDIBits(
            mem,
            bitmap,
            0,
            screen.height,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        if lines != height {
            return Err(gdi("GetDIBits"));
        }
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 255;
        }
        Ok(Image {
            width: screen.width,
            height: screen.height,
            rgba: pixels,
        })
    }
}

/// runs its closure on drop
struct Guard<F: FnMut()>(F);

impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

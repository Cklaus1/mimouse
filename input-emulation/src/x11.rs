use async_trait::async_trait;
use std::ptr;
use x11::{
    xlib::{self, XCloseDisplay},
    xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::geometry::{Rect, entry_point};
use super::normalize::Steps;
use super::text::{self, Keystroke};
use super::{Edge, Emulation, EmulationHandle, TextSupport, error::X11EmulationCreationError};

/// one scroll button click per wheel notch
const V120_PER_CLICK: u32 = 120;
/// clicks per scroll event at most, so one hostile value cannot keep the
/// runtime busy
const MAX_CLICKS: u32 = 16;

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
    /// the core keyboard state when the current text began
    typing_state: u32,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new() -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;
        Ok(Self {
            display,
            typing_state: 0,
        })
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0, 0);
        }
    }

    /// The screen as one display. Xlib alone does not list monitors
    /// (that needs Xinerama / RandR); the server keeps the pointer on a
    /// monitor.
    fn screen(&self) -> Rect {
        unsafe {
            let screen = xlib::XDefaultScreen(self.display);
            Rect {
                x: 0,
                y: 0,
                width: xlib::XDisplayWidth(self.display, screen),
                height: xlib::XDisplayHeight(self.display, screen),
            }
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        unsafe {
            let x11_button = match button {
                BTN_RIGHT => 3,
                BTN_MIDDLE => 2,
                BTN_BACK => 8,
                BTN_FORWARD => 9,
                BTN_LEFT => 1,
                _ => 1,
            };
            xtest::XTestFakeButtonEvent(self.display, x11_button, state as i32, 0);
        };
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    /// `clicks` presses of the scroll button for the direction of `value`
    fn emulate_scroll(&self, axis: u8, value: f64, clicks: u32) {
        let direction = match axis {
            1 => {
                if value < 0.0 {
                    Self::SCROLL_LEFT
                } else {
                    Self::SCROLL_RIGHT
                }
            }
            _ => {
                if value < 0.0 {
                    Self::SCROLL_UP
                } else {
                    Self::SCROLL_DOWN
                }
            }
        };

        for _ in 0..clicks {
            unsafe {
                xtest::XTestFakeButtonEvent(self.display, direction, 1, 0);
                xtest::XTestFakeButtonEvent(self.display, direction, 0, 0);
            }
        }
    }

    fn emulate_key(&self, key: u32, state: u8) {
        let key = key + 8; // xorg keycodes are shifted by 8
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, key, state as i32, 0);
        }
    }

    /// the evdev code of the first key with `keysym`, `None` without one
    fn key_for(&self, keysym: xkeysym::Keysym) -> Option<u32> {
        let code = unsafe { xlib::XKeysymToKeycode(self.display, keysym.raw().into()) };
        u32::from(code).checked_sub(8).filter(|&code| code > 0)
    }

    /// the core modifier and group state, as in key events
    fn keyboard_state(&self) -> u32 {
        let (mut root, mut child) = (0, 0);
        let (mut root_x, mut root_y, mut x, mut y) = (0, 0, 0, 0);
        let mut mask = 0;
        // the mask is set even when the pointer is on another screen
        unsafe {
            xlib::XQueryPointer(
                self.display,
                xlib::XDefaultRootWindow(self.display),
                &mut root,
                &mut child,
                &mut root_x,
                &mut root_y,
                &mut x,
                &mut y,
                &mut mask,
            );
        }
        mask
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.relative_motion(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    self.emulate_scroll(axis, value, 1);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let clicks = (value.unsigned_abs() / V120_PER_CLICK).min(MAX_CLICKS);
                    self.emulate_scroll(axis, value as f64, clicks);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            _ => {}
        }
        unsafe {
            xlib::XFlush(self.display);
        }
        // FIXME
        Ok(())
    }

    /// XTest moves in whole pixels and scrolls in whole wheel clicks
    fn steps(&self) -> Steps {
        Steps {
            whole_pixels: true,
            v120: V120_PER_CLICK as i32,
        }
    }

    fn enter_at(
        &mut self,
        _: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        let Some((x, y)) = entry_point(&[self.screen()], edge, fraction) else {
            return Ok(None);
        };
        unsafe {
            // screen -1: the screen the pointer is on
            xtest::XTestFakeMotionEvent(self.display, -1, x, y, 0);
            xlib::XFlush(self.display);
        }
        Ok(Some((x, y)))
    }

    fn text_support(&self) -> TextSupport {
        TextSupport::Layout
    }

    fn desktop(&self) -> Option<Vec<Rect>> {
        Some(vec![self.screen()])
    }

    fn pointer_to(&mut self, _: EmulationHandle, x: i32, y: i32) -> Result<bool, EmulationError> {
        unsafe {
            // screen -1: the screen the pointer is on
            xtest::XTestFakeMotionEvent(self.display, -1, x, y, 0);
            xlib::XFlush(self.display);
        }
        Ok(true)
    }

    fn begin_typing(&mut self) {
        self.typing_state = self.keyboard_state();
    }

    /// The key with the keysym on its first or Shift level in the first
    /// group; AltGr levels are not searched. `None` while another group is
    /// active, or Caps Lock is on for a letter (see `text::state_changes`).
    fn keystroke(&self, c: char) -> Option<Keystroke> {
        /// `ShiftMask`
        const SHIFT: u32 = 1 << 0;
        let state = self.typing_state;
        // core state: `LockMask` and the group in bits 13 and 14
        if text::state_changes(c, (state >> 13) & 0b11, state & xlib::LockMask != 0) {
            return None;
        }
        let keysym = text::keysym(c)?;
        let key = self.key_for(keysym)?;
        let level = (0..2).find(|&level| {
            let at = unsafe { xlib::XkbKeycodeToKeysym(self.display, (key + 8) as u8, 0, level) };
            at == keysym.raw().into()
        })?;
        let mut stroke = Keystroke {
            key,
            ..Default::default()
        };
        if level == 1 {
            stroke.modifiers[0] = self.key_for(xkeysym::Keysym::Shift_L)?;
            stroke.depressed = SHIFT;
        }
        Some(stroke)
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }
}

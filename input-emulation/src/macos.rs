use super::geometry::{Rect, entry_point};
use super::macos_logic::{KeyAction, KeyPlanner};
use super::normalize::Steps;
use super::{Edge, Emulation, EmulationHandle, error::EmulationError};
use async_trait::async_trait;
use core_graphics::base::CGFloat;
use core_graphics::display::{
    CGDirectDisplayID, CGDisplay, CGDisplayBounds, CGGetDisplaysWithRect, CGPoint, CGRect, CGSize,
};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use input_event::macos_scroll::{MacosScroll, V120_PER_LINE};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};
use keycode::{KeyMap, KeyMapping};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use super::error::MacOSEmulationCreationError;

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) struct MacOSEmulation {
    /// global event source for all events
    event_source: CGEventSource,
    /// task handle for key repeats
    repeat_task: Option<JoinHandle<()>>,
    /// current state of the mouse buttons (tracked by evdev button code)
    pressed_buttons: HashSet<u32>,
    /// button previously pressed (evdev button code)
    previous_button: Option<u32>,
    /// timestamp of previous click (button down)
    previous_button_click: Option<Instant>,
    /// click state, i.e. number of clicks in quick succession
    button_click_state: i64,
    /// modifier and repeating-key state
    keys: KeyPlanner,
    /// current modifier flags, shared with the repeat task
    flags: Rc<Cell<CGEventFlags>>,
}

/// Maps an evdev button code to the CGEventType used for drag events.
fn drag_event_type(button: u32) -> CGEventType {
    match button {
        BTN_LEFT => CGEventType::LeftMouseDragged,
        BTN_RIGHT => CGEventType::RightMouseDragged,
        // middle, back, forward, and any other button all use OtherMouseDragged
        _ => CGEventType::OtherMouseDragged,
    }
}

unsafe impl Send for MacOSEmulation {}

impl MacOSEmulation {
    pub(crate) fn new() -> Result<Self, MacOSEmulationCreationError> {
        request_macos_emulation_permissions()?;

        let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| MacOSEmulationCreationError::EventSourceCreation)?;
        Ok(Self {
            event_source,
            pressed_buttons: HashSet::new(),
            previous_button: None,
            previous_button_click: None,
            button_click_state: 0,
            repeat_task: None,
            keys: KeyPlanner::default(),
            flags: Rc::new(Cell::new(CGEventFlags::empty())),
        })
    }

    /// moves the pointer to `location` with a mouse-moved event, or a drag
    /// event if a button is held
    fn post_motion(&self, location: CGPoint, dx: f64, dy: f64) -> bool {
        let event_type = self
            .pressed_buttons
            .iter()
            .next()
            .map(|&btn| drag_event_type(btn))
            .unwrap_or(CGEventType::MouseMoved);
        let Ok(event) = CGEvent::new_mouse_event(
            self.event_source.clone(),
            event_type,
            location,
            CGMouseButton::Left,
        ) else {
            log::warn!("mouse event creation failed!");
            return false;
        };
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
        event.post(CGEventTapLocation::HID);
        true
    }

    fn get_mouse_location(&self) -> Option<CGPoint> {
        let event: CGEvent = CGEvent::new(self.event_source.clone()).ok()?;
        Some(event.location())
    }

    /// posts the key-down for `key` and repeats it until stopped; the
    /// key-up is posted by the caller when the key is released
    fn start_repeat(&mut self, key: u16) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.stop_repeat();
        key_event(self.event_source.clone(), key, true, self.flags.get());
        let event_source = self.event_source.clone();
        let flags = self.flags.clone();
        self.repeat_task = Some(tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                key_event(event_source.clone(), key, true, flags.get());
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        }));
    }

    fn stop_repeat(&mut self) {
        // the task runs on this thread and is parked at an await point,
        // so aborting it guarantees no further key-down is posted
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

fn request_macos_emulation_permissions() -> Result<(), MacOSEmulationCreationError> {
    // Request both permissions up front so the user sees both TCC prompts
    // on the first launch. See the matching comment in input-capture/src/
    // macos.rs::request_macos_capture_permissions for the rationale.
    let accessibility = request_accessibility_permission();
    let input_control = request_input_control_permission();

    if !accessibility {
        return Err(MacOSEmulationCreationError::AccessibilityPermission);
    }
    if !input_control {
        return Err(MacOSEmulationCreationError::InputControlPermission);
    }
    Ok(())
}

fn request_accessibility_permission() -> bool {
    // Silent check. The service fires the one-time user-visible prompt at
    // startup (see mimouse's macos_permissions module).
    unsafe { AXIsProcessTrusted() }
}

fn request_input_control_permission() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Mac virtual key codes for the four arrow keys.
const MAC_KEY_LEFT: u16 = 0x7B;
const MAC_KEY_RIGHT: u16 = 0x7C;
const MAC_KEY_DOWN: u16 = 0x7D;
const MAC_KEY_UP: u16 = 0x7E;

fn is_arrow_key(key: u16) -> bool {
    matches!(
        key,
        MAC_KEY_LEFT | MAC_KEY_RIGHT | MAC_KEY_DOWN | MAC_KEY_UP
    )
}

fn key_event(event_source: CGEventSource, key: u16, down: bool, mut flags: CGEventFlags) {
    let event = match CGEvent::new_keyboard_event(event_source, key, down) {
        Ok(e) => e,
        Err(_) => {
            log::warn!("unable to create key event");
            return;
        }
    };
    // Hardware-generated arrow keys on macOS carry NumericPad + SecondaryFn.
    // CGEventTap-based hotkey matchers (e.g. tiling window managers) check
    // these flags to recognize navigation keys; without them synthesized
    // arrow chords fall through to the focused app.
    if is_arrow_key(key) {
        flags |= CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn;
    }
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    log::trace!("key event: {key} {down}");
}

/// flags-changed event for a modifier key: carries the modifier's own key
/// code (a bare `CGEvent` has code 0, which is 'A')
fn modifier_key_event(event_source: CGEventSource, key: u16, down: bool, flags: CGEventFlags) {
    let Ok(event) = CGEvent::new_keyboard_event(event_source, key, down) else {
        log::warn!("could not create CGEvent");
        return;
    };
    event.set_type(CGEventType::FlagsChanged);
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    log::trace!("modifiers updated: {key} {down} {flags:?}");
}

fn get_display_at_point(x: CGFloat, y: CGFloat) -> Option<CGDirectDisplayID> {
    let mut displays: [CGDirectDisplayID; 16] = [0; 16];
    let mut display_count: u32 = 0;
    let rect = CGRect::new(&CGPoint::new(x, y), &CGSize::new(0.0, 0.0));

    let error = unsafe {
        CGGetDisplaysWithRect(
            rect,
            1,
            displays.as_mut_ptr(),
            &mut display_count as *mut u32,
        )
    };

    if error != 0 {
        log::warn!("error getting displays at point ({x}, {y}): {error}");
        return Option::None;
    }

    if display_count == 0 {
        log::debug!("no displays found at point ({x}, {y})");
        return Option::None;
    }

    displays.first().copied()
}

fn get_display_bounds(display: CGDirectDisplayID) -> (CGFloat, CGFloat, CGFloat, CGFloat) {
    unsafe {
        let bounds = CGDisplayBounds(display);
        let min_x = bounds.origin.x;
        let max_x = bounds.origin.x + bounds.size.width;
        let min_y = bounds.origin.y;
        let max_y = bounds.origin.y + bounds.size.height;
        (min_x as f64, min_y as f64, max_x as f64, max_y as f64)
    }
}

fn clamp_to_screen_space(
    current_x: CGFloat,
    current_y: CGFloat,
    dx: CGFloat,
    dy: CGFloat,
) -> (CGFloat, CGFloat) {
    // Check which display the mouse is currently on
    // Determine what the location of the mouse would be after applying the move
    // Get the display at the new location
    // If the point is not on a display
    //   Clamp the mouse to the current display
    // Else If the point is on a display
    //   Clamp the mouse to the new display
    let current_display = match get_display_at_point(current_x, current_y) {
        Some(display) => display,
        None => {
            log::warn!("could not get current display!");
            return (current_x, current_y);
        }
    };

    let new_x = current_x + dx;
    let new_y = current_y + dy;

    let final_display = get_display_at_point(new_x, new_y).unwrap_or(current_display);
    let (min_x, min_y, max_x, max_y) = get_display_bounds(final_display);

    (
        new_x.clamp(min_x, max_x - 1.),
        new_y.clamp(min_y, max_y - 1.),
    )
}

#[async_trait]
impl Emulation for MacOSEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        log::trace!("{event:?}");
        match event {
            Event::Pointer(pointer_event) => {
                match pointer_event {
                    PointerEvent::Motion { time: _, dx, dy } => {
                        let mut mouse_location = match self.get_mouse_location() {
                            Some(l) => l,
                            None => {
                                log::warn!("could not get mouse location!");
                                return Ok(());
                            }
                        };

                        let (new_mouse_x, new_mouse_y) =
                            clamp_to_screen_space(mouse_location.x, mouse_location.y, dx, dy);

                        mouse_location.x = new_mouse_x;
                        mouse_location.y = new_mouse_y;
                        self.post_motion(mouse_location, dx, dy);
                    }
                    PointerEvent::Button {
                        time: _,
                        button,
                        state,
                    } => {
                        // button number for OtherMouse events (3 = back, 4 = forward, etc.)
                        let cg_button_number: Option<i64> = match button {
                            BTN_BACK => Some(3),
                            BTN_FORWARD => Some(4),
                            _ => None,
                        };
                        let (event_type, mouse_button) = match (button, state) {
                            (BTN_LEFT, 1) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                            (BTN_LEFT, 0) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                            (BTN_RIGHT, 1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                            (BTN_RIGHT, 0) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                            (BTN_MIDDLE, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                            (BTN_MIDDLE, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                            (BTN_BACK, 1) | (BTN_FORWARD, 1) => {
                                (CGEventType::OtherMouseDown, CGMouseButton::Center)
                            }
                            (BTN_BACK, 0) | (BTN_FORWARD, 0) => {
                                (CGEventType::OtherMouseUp, CGMouseButton::Center)
                            }
                            _ => {
                                log::warn!("invalid button event: {button},{state}");
                                return Ok(());
                            }
                        };
                        // store button state using the evdev button code so
                        // back, forward, and middle are tracked independently
                        if state == 1 {
                            self.pressed_buttons.insert(button);
                        } else {
                            self.pressed_buttons.remove(&button);
                        }

                        // update double-click tracking using the evdev button
                        // code so that back/forward don't alias with middle
                        if state == 1 {
                            if self.previous_button == Some(button)
                                && self
                                    .previous_button_click
                                    .is_some_and(|i| i.elapsed() < DOUBLE_CLICK_INTERVAL)
                            {
                                self.button_click_state += 1;
                            } else {
                                self.button_click_state = 1;
                            }
                            self.previous_button = Some(button);
                            self.previous_button_click = Some(Instant::now());
                        }

                        log::debug!("click_state: {}", self.button_click_state);
                        let location = self.get_mouse_location().unwrap();
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            location,
                            mouse_button,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(
                            EventField::MOUSE_EVENT_CLICK_STATE,
                            self.button_click_state,
                        );
                        // Set the button number for extra buttons (back=3, forward=4)
                        if let Some(btn_num) = cg_button_number {
                            event.set_integer_value_field(
                                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                                btn_num,
                            );
                        }
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::Axis { .. } | PointerEvent::AxisDiscrete120 { .. } => {
                        let Some(scroll) = MacosScroll::from_wire(pointer_event) else {
                            log::warn!("invalid scroll event: {pointer_event:?}");
                            return Ok(());
                        };
                        let unit = if scroll.lines {
                            ScrollEventUnit::LINE
                        } else {
                            ScrollEventUnit::PIXEL
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            unit,
                            scroll.wheel_count(),
                            scroll.vertical,
                            scroll.horizontal,
                            0,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        event.post(CGEventTapLocation::HID);
                    }
                }

                // reset button click state in case it's not a button event
                if !matches!(pointer_event, PointerEvent::Button { .. }) {
                    self.button_click_state = 0;
                }
            }
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let code = match KeyMap::from_key_mapping(KeyMapping::Evdev(key as u16)) {
                        Ok(k) => k.mac as CGKeyCode,
                        Err(_) => {
                            log::warn!("unable to map key event");
                            return Ok(());
                        }
                    };
                    let action = self.keys.key(key, code, state != 0);
                    match action {
                        KeyAction::FlagsChanged {
                            code,
                            pressed,
                            flags,
                        } => {
                            let flags = CGEventFlags::from_bits_retain(flags);
                            self.flags.set(flags);
                            modifier_key_event(self.event_source.clone(), code, pressed, flags);
                        }
                        KeyAction::Press { code, flags } => {
                            self.flags.set(CGEventFlags::from_bits_retain(flags));
                            self.start_repeat(code);
                        }
                        KeyAction::Release {
                            code,
                            flags,
                            stop_repeat,
                        } => {
                            if stop_repeat {
                                self.stop_repeat();
                            }
                            let flags = CGEventFlags::from_bits_retain(flags);
                            key_event(self.event_source.clone(), code, false, flags);
                        }
                    }
                }
                KeyboardEvent::Modifiers { depressed, .. } => {
                    // only the depressed modifiers matter; they apply to the
                    // next key event, so nothing is posted here
                    self.keys.set_modifiers(depressed);
                    self.flags
                        .set(CGEventFlags::from_bits_retain(self.keys.flags()));
                }
            },
        }
        Ok(())
    }

    /// scroll wheel deltas are whole pixels or whole lines
    fn steps(&self) -> Steps {
        Steps {
            whole_pixels: true,
            v120: V120_PER_LINE,
        }
    }

    fn enter_at(
        &mut self,
        _handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        let displays = match CGDisplay::active_displays() {
            Ok(displays) => displays,
            Err(e) => {
                log::warn!("could not list displays: {e}");
                return Ok(None);
            }
        };
        let displays = displays
            .into_iter()
            .map(|id| {
                // global display coordinates (points), origin top left of
                // the main display
                let b = CGDisplay::new(id).bounds();
                Rect {
                    x: b.origin.x.round() as i32,
                    y: b.origin.y.round() as i32,
                    width: b.size.width.round() as i32,
                    height: b.size.height.round() as i32,
                }
            })
            .collect::<Vec<_>>();
        let Some((x, y)) = entry_point(&displays, edge, fraction) else {
            return Ok(None);
        };
        let placed = self.post_motion(CGPoint::new(x as CGFloat, y as CGFloat), 0., 0.);
        Ok(placed.then_some((x, y)))
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {
        self.stop_repeat();
    }
}

// the flag bits used by the platform-independent key logic
const _: () = {
    use super::macos_logic::*;
    assert!(CGEventFlags::CGEventFlagShift.bits() == CG_SHIFT);
    assert!(CGEventFlags::CGEventFlagAlphaShift.bits() == CG_ALPHA_SHIFT);
    assert!(CGEventFlags::CGEventFlagControl.bits() == CG_CONTROL);
    assert!(CGEventFlags::CGEventFlagAlternate.bits() == CG_ALTERNATE);
    assert!(CGEventFlags::CGEventFlagCommand.bits() == CG_COMMAND);
};

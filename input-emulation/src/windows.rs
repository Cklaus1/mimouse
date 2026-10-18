use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};

use async_trait::async_trait;
use std::ops::BitOrAssign;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;
use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, HKL, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyExW, ToUnicodeEx, VIRTUAL_KEY, VK_CAPITAL,
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_SHIFT, VkKeyScanExW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    SetCursorPos, WM_NULL, XBUTTON1, XBUTTON2,
};
use windows::core::BOOL;

use super::geometry::{Rect, entry_point};
use super::keys::{Repeat, RepeatPlanner};
use super::normalize::Steps;
use super::text::{WinStep, windows_steps};
use super::windows_logic::{Wheel, wheel};
use super::{Edge, Emulation, EmulationHandle, TextSupport, Typed};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
/// pause after each typed character, before waiting for the window;
/// Windows 11 Notepad typed wrong characters at 5 ms, none at 20 ms
const CHAR_GAP: Duration = Duration::from_millis(20);
/// how long to wait for the foreground window to read messages after a
/// typed character
const SETTLE_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) struct WindowsEmulation {
    repeat: RepeatPlanner,
    repeat_task: Option<AbortHandle>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        Ok(Self {
            repeat: RepeatPlanner::default(),
            repeat_task: None,
        })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state),
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => scroll(axis, value as i32),
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value),
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    match self.repeat.key(key, state == 1) {
                        Repeat::Start(key) => self.spawn_repeat_task(key),
                        Repeat::Stop => self.kill_repeat_task(),
                        Repeat::Keep => {}
                    }
                    key_event(key, state)
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        Ok(())
    }

    /// `SendInput` moves in whole pixels; its wheels take any 120th
    fn steps(&self) -> Steps {
        Steps {
            whole_pixels: true,
            v120: 1,
        }
    }

    fn enter_at(
        &mut self,
        _: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        let Some((x, y)) = entry_point(&monitors(), edge, fraction) else {
            log::warn!("no monitors to place the pointer on");
            return Ok(None);
        };
        // virtual-desktop coordinates, the same space EnumDisplayMonitors uses
        if let Err(e) = unsafe { SetCursorPos(x, y) } {
            log::warn!("SetCursorPos({x}, {y}): {e}");
            return Ok(None);
        }
        Ok(Some((x, y)))
    }

    /// any text, as Unicode key events
    fn text_support(&self) -> TextSupport {
        TextSupport::Exact
    }

    fn desktop(&self) -> Option<Vec<Rect>> {
        let _physical = PhysicalPixels::new();
        let monitors = monitors();
        (!monitors.is_empty()).then_some(monitors)
    }

    fn pointer_to(&mut self, _: EmulationHandle, x: i32, y: i32) -> Result<bool, EmulationError> {
        let _physical = PhysicalPixels::new();
        if let Err(e) = unsafe { SetCursorPos(x, y) } {
            log::warn!("SetCursorPos({x}, {y}): {e}");
            return Ok(false);
        }
        Ok(true)
    }

    async fn type_text(
        &mut self,
        _: EmulationHandle,
        text: &str,
    ) -> Result<Option<Typed>, EmulationError> {
        let (characters, unmapped) = typing_batches(text);
        let paced = tokio::task::spawn_blocking(move || type_paced(&characters));
        if let Err(e) = paced.await {
            log::warn!("typing: {e}");
        }
        Ok(Some(Typed { unmapped }))
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {
        self.kill_repeat_task();
    }
}

/// Monitor rectangles on the virtual desktop.
fn monitors() -> Vec<Rect> {
    unsafe extern "system" fn add_monitor(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = unsafe { &mut *(data.0 as *mut Vec<Rect>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let r = info.rcMonitor;
            monitors.push(Rect {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            });
        }
        true.into()
    }
    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(add_monitor),
            LPARAM(&mut monitors as *mut Vec<Rect> as isize),
        );
    }
    monitors
}

/// Physical pixels for this thread while alive, the pixels screenshots
/// have: monitor rectangles and cursor positions are not scaled by DPI.
struct PhysicalPixels(DPI_AWARENESS_CONTEXT);

impl PhysicalPixels {
    fn new() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for PhysicalPixels {
    fn drop(&mut self) {
        // invalid: setting it failed, nothing to restore
        if !self.0.is_invalid() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

impl WindowsEmulation {
    fn spawn_repeat_task(&mut self, key: u32) {
        self.kill_repeat_task();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                key_event(key, 1);
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

/// at most one warning per interval about refused input
const REFUSED_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// when the last warning was logged and how many events were refused since
static REFUSED: Mutex<(Option<Instant>, u64)> = Mutex::new((None, 0));

/// Sends `inputs` once. Windows refuses injection while the secure desktop
/// (lock screen, UAC) is shown or UIPI blocks it; the events are dropped
/// then, since retrying would spin the runtime thread until the desktop
/// returns.
fn send_input_safe(inputs: &[INPUT]) {
    // number of events submitted
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } as usize;
    if sent >= inputs.len() {
        return;
    }
    let Ok(mut refused) = REFUSED.lock() else {
        return;
    };
    let (last_warning, count) = &mut *refused;
    *count += (inputs.len() - sent) as u64;
    if last_warning.is_none_or(|t| t.elapsed() >= REFUSED_WARN_INTERVAL) {
        log::warn!(
            "Windows refused {count} injected input event(s), dropped (secure desktop or a more privileged window has focus)"
        );
        *last_warning = Some(Instant::now());
        *count = 0;
    }
}

/// The `SendInput` batch of each character of `text`, and the characters
/// not typed.
fn typing_batches(text: &str) -> (Vec<Vec<INPUT>>, String) {
    let layout = foreground_layout();
    // Caps Lock as this thread sees it, to check what a letter key types
    let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0;
    let (steps, unmapped) = windows_steps(text, |c| layout_key(c, layout, caps));
    let characters = steps
        .iter()
        .map(|step| {
            let mut batch = Vec::new();
            match step {
                WinStep::Key(vk, modifiers) => keystroke(&mut batch, *vk, *modifiers, layout),
                WinStep::Packet(units) => {
                    for &unit in units {
                        for up in [KEYBD_EVENT_FLAGS(0), KEYEVENTF_KEYUP] {
                            batch.push(key_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE | up));
                        }
                    }
                }
            }
            batch
        })
        .collect();
    (characters, unmapped)
}

/// Sends one character at a time and lets the foreground window take it
/// before the next: WinUI applications (Windows 11 Notepad) read the
/// modifier state and a `VK_PACKET`'s character when they handle a key,
/// not when it was sent, so a backlog types wrong characters. Pacing
/// stops after [`crate::MAX_TYPE_PACING`]; the rest is sent at once.
fn type_paced(characters: &[Vec<INPUT>]) {
    let deadline = Instant::now() + crate::MAX_TYPE_PACING;
    let gap = match characters.len() {
        0 => Duration::ZERO,
        n => CHAR_GAP.min(crate::MAX_TYPE_PACING / n as u32),
    };
    for (i, character) in characters.iter().enumerate() {
        if Instant::now() >= deadline {
            send_input_safe(&characters[i..].concat());
            return;
        }
        send_input_safe(character);
        std::thread::sleep(gap);
        // returns once the window's thread reads messages again
        let window = unsafe { GetForegroundWindow() };
        let timeout = SETTLE_TIMEOUT.as_millis() as u32;
        unsafe {
            SendMessageTimeoutW(
                window,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                timeout,
                None,
            )
        };
    }
}

/// The keyboard layout of the foreground window's thread, which receives
/// the keys.
fn foreground_layout() -> HKL {
    let thread = unsafe { GetWindowThreadProcessId(GetForegroundWindow(), None) };
    unsafe { GetKeyboardLayout(thread) }
}

/// The key and `VkKeyScan` modifiers that type exactly `c` in `layout`:
/// none for characters outside it, dead keys and keys Caps Lock changes
/// into another character.
fn layout_key(c: char, layout: HKL, caps: bool) -> Option<(u16, u8)> {
    let mut unit = [0; 2];
    let [unit] = c.encode_utf16(&mut unit) else {
        return None;
    };
    let scan = unsafe { VkKeyScanExW(*unit, layout) };
    if scan == -1 {
        return None;
    }
    let [vk, modifiers] = scan.to_le_bytes();
    // Hankaku and the other modifier bits are not keys to hold
    if modifiers & !0b111 != 0 {
        return None;
    }
    let mut state = [0u8; 256];
    for (bit, key) in [(1, VK_SHIFT), (2, VK_CONTROL), (4, VK_MENU)] {
        if modifiers & bit != 0 {
            state[usize::from(key.0)] = 0x80;
        }
    }
    if caps {
        state[usize::from(VK_CAPITAL.0)] = 0x01;
    }
    let code = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, Some(layout)) };
    let mut typed = [0u16; 4];
    // flag 4: leave the keyboard state (a pending dead key) unchanged
    let n = unsafe { ToUnicodeEx(u32::from(vk), code, &state, &mut typed, 4, Some(layout)) };
    (n == 1 && typed[0] == *unit).then_some((u16::from(vk), modifiers))
}

/// Presses and releases `vk` with the `VkKeyScan` `modifiers` held.
fn keystroke(inputs: &mut Vec<INPUT>, vk: u16, modifiers: u8, layout: HKL) {
    let held: Vec<VIRTUAL_KEY> = [(1, VK_LSHIFT), (2, VK_LCONTROL), (4, VK_LMENU)]
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, key)| key)
        .collect();
    let scan = |key: VIRTUAL_KEY| {
        (unsafe { MapVirtualKeyExW(u32::from(key.0), MAPVK_VK_TO_VSC, Some(layout)) }) as u16
    };
    let up = KEYEVENTF_KEYUP;
    for &key in &held {
        inputs.push(key_input(key, scan(key), KEYBD_EVENT_FLAGS(0)));
    }
    let key = VIRTUAL_KEY(vk);
    inputs.push(key_input(key, scan(key), KEYBD_EVENT_FLAGS(0)));
    inputs.push(key_input(key, scan(key), up));
    for &key in held.iter().rev() {
        inputs.push(key_input(key, scan(key), up));
    }
}

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_mouse_input(mi: MOUSEINPUT) {
    send_input_safe(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }]);
}

fn send_keyboard_input(ki: KEYBDINPUT) {
    send_input_safe(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }]);
}
fn rel_mouse(dx: i32, dy: i32) {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn mouse_button(button: u32, state: u32) {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return,
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return,
        },
        _ => return,
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn scroll(axis: u8, value: i32) {
    let Some((wheel, data)) = wheel(axis, value) else {
        return;
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data as u32,
        dwFlags: match wheel {
            Wheel::Vertical => MOUSEEVENTF_WHEEL,
            Wheel::Horizontal => MOUSEEVENTF_HWHEEL,
        },
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn key_event(key: u32, state: u8) {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return,
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki);
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}

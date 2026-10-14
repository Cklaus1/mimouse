use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WNDPROC,
};
use windows::core::{PCWSTR, w};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, EdgeRange, Position, display_util};
use crate::ChordMatcher;
use crate::chord::{Chord, Fed};
use crate::event_queue::EventSender;
use crate::geometry::{self, Rect};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<Request>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: EventSender) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn release_capture(&self) {
        self.request(Request::Release);
    }

    pub(crate) fn create(&self, pos: Position, range: Option<EdgeRange>) {
        self.request(Request::Create(pos, range));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.request(Request::Destroy(pos));
    }

    pub(crate) fn focus(&self, pos: Position, range: Option<EdgeRange>) {
        self.request(Request::Focus(pos, range));
    }

    pub(crate) fn set_hotkeys(&self, hotkeys: Vec<(Chord, Position)>) {
        self.request(Request::SetHotkeys(hotkeys));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    /// one queue for every request, so a release and the focus that
    /// follows it are handled in order
    fn request(&self, request: Request) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::Request);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    Request = 0,
    Exit = 2,
}

enum Request {
    Create(Position, Option<EdgeRange>),
    Destroy(Position),
    Release,
    /// start capture at a position without a crossing
    Focus(Position, Option<EdgeRange>),
    SetHotkeys(Vec<(Chord, Position)>),
}

/// Queues an event for the capture task without blocking the hook; false
/// if it was dropped.
fn send_event(pos: Position, event: CaptureEvent) -> bool {
    EVENT_TX.with_borrow(|tx| tx.as_ref().is_some_and(|tx| tx.send(pos, event)))
}

thread_local! {
    /// all configured clients and the part of their edge that activates them
    static CLIENTS: RefCell<HashMap<Position, Option<EdgeRange>>> = RefCell::new(HashMap::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<EventSender>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// client hotkeys, watched while input is local
    static HOTKEYS: RefCell<ChordMatcher<Position>> = RefCell::new(ChordMatcher::default());
    /// virtual-key code of each key the hotkeys saw go down
    static VIRTUAL_KEYS: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
    /// keys held down locally when a hotkey started capture: their key-ups
    /// go to the system, or it would keep them held
    static LOCAL_KEYS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: EventSender,
    request_buffer: Arc<Mutex<Vec<Request>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: EventSender,
    request_buffer: Arc<Mutex<Vec<Request>>>,
) {
    EVENT_TX.replace(Some(event_tx));
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("mimouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("mimouse-message-window-class"),
            w!("mimouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Request as usize => {
                    let requests = std::mem::take(&mut *request_buffer.lock().unwrap());
                    for request in requests {
                        handle_request(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    let Some(range) = CLIENTS.with_borrow(|clients| clients.get(&pos).copied()) else {
        return ret;
    };

    let (entry_point, fraction, in_range) = DISPLAYS.with_borrow(|(displays, _)| {
        let entry_point = display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos);
        let rects: Vec<Rect> = displays.iter().map(to_rect).collect();
        let point = (f64::from(entry_point.0), f64::from(entry_point.1));
        let in_range = match geometry::edge_extent(&rects, pos) {
            Some(extent) => geometry::in_range(extent, geometry::along(point, pos), range),
            None => range.is_none(),
        };
        let fraction = geometry::edge_fraction(&rects, pos, point);
        (entry_point, fraction, in_range)
    });

    /* a crossing outside the client's range does not activate it */
    if !in_range {
        log::trace!("crossing at {entry_point:?} outside the range of {pos}");
        return ret;
    }

    /* notify main thread; a dropped `Begin` leaves the input local */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    if !send_event(pos, CaptureEvent::Begin(fraction)) {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    ENTRY_POINT.replace(entry_point);

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* mimouse's idle keepalive must reach the system to reset its idle timer */
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    if mouse_low_level.dwExtraInfo == crate::IDLE_KEEPALIVE_TAG {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* convert to mimouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (motion and presses are dropped if sending too fast) */
    send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event)));

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let active = ACTIVE_CLIENT.get();
    let watching = HOTKEYS.with_borrow(|hotkeys| !hotkeys.is_empty());

    /* convert to key event */
    let key_event = if active.is_some() || watching {
        to_key_event(wparam, lparam)
    } else {
        None
    };

    /* hotkeys see every key, so they know which keys are held */
    let hotkey = match key_event {
        Some(KeyboardEvent::Key { key, state, .. }) => {
            let vk = unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode };
            feed_hotkeys(key, state, vk, active.is_none())
        }
        _ => Fed::Pass,
    };

    /* get active client if any */
    let Some(client) = active else {
        /* a client hotkey while local: capture for it; only its last key
         * is swallowed, the held modifiers reach applications as usual */
        if let (Fed::Fired(pos), Some(KeyboardEvent::Key { key, .. })) = (hotkey, key_event) {
            let range = CLIENTS.with_borrow(|clients| clients.get(&pos).copied().flatten());
            /* everything held but the swallowed final key went down locally */
            let local_keys = HOTKEYS.with_borrow(|hotkeys| match hotkeys.held() {
                [held @ .., _last] => held.to_vec(),
                [] => Vec::new(),
            });
            if activate(pos, range) {
                LOCAL_KEYS.replace(local_keys);
                /* its auto-repeat and key-up must not reach the client */
                HOTKEYS.with_borrow_mut(|hotkeys| hotkeys.swallow(key));
                return LRESULT(1);
            }
        }
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    let Some(key_event) = key_event else {
        return LRESULT(1);
    };

    /* auto-repeat or key-up of the final key of the hotkey that started capture */
    if hotkey == Fed::Swallowed {
        return LRESULT(1);
    }

    /* the key-up of a key that went down locally */
    if let KeyboardEvent::Key { key, state: 0, .. } = key_event {
        let local = LOCAL_KEYS.with_borrow_mut(|keys| {
            let held = keys.contains(&key);
            keys.retain(|&k| k != key);
            held
        });
        if local {
            return CallNextHookEx(None, ncode, wparam, lparam);
        }
    }

    send_event(client, CaptureEvent::Input(Event::Keyboard(key_event)));

    /* don't pass event to applications */
    LRESULT(1)
}

/// Feeds a key to the hotkeys: whether it completes the hotkey of a
/// position or is swallowed. `local`: input goes to this machine.
fn feed_hotkeys(key: u32, state: u8, vk: u32, local: bool) -> Fed<Position> {
    VIRTUAL_KEYS.with_borrow_mut(|vks| {
        HOTKEYS.with_borrow_mut(|hotkeys| {
            if state != 0 {
                vks.insert(key, vk);
                /* key-ups on the secure desktop (Ctrl+Alt+Del, Win+L) never
                 * reach this hook. Keys swallowed while capturing are
                 * missing from the async key state, so only check while local. */
                if local {
                    hotkeys.forget_released(|k| {
                        k == key || vks.get(&k).is_none_or(|&vk| is_key_down(vk))
                    });
                }
            }
            hotkeys.feed(key, state)
        })
    })
}

fn is_key_down(vk: u32) -> bool {
    // the most significant bit is set while the key is down
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// Starts capture at `pos` without a crossing (a hotkey or a focus
/// request): moves the cursor to the middle of `range` on that edge, as if
/// it had crossed there, and reports `Begin`. False if no client is there.
fn activate(pos: Position, range: Option<EdgeRange>) -> bool {
    if !CLIENTS.with_borrow(|clients| clients.contains_key(&pos)) {
        log::warn!("cannot start capture: no client at {pos}");
        return false;
    }
    let target = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        let rects: Vec<Rect> = displays.iter().map(to_rect).collect();
        let point = geometry::focus_point(&rects, pos, range)?;
        Some((point, geometry::edge_fraction(&rects, pos, point)))
    });
    let Some(((x, y), fraction)) = target else {
        log::warn!("cannot start capture: no display");
        return false;
    };
    let point = (x as i32, y as i32);
    /* a dropped `Begin` leaves the input local */
    if !send_event(pos, CaptureEvent::Begin(fraction)) {
        return false;
    }
    if let Err(e) = unsafe { SetCursorPos(point.0, point.1) } {
        log::warn!("failed to move the cursor to {point:?}: {e}");
    }
    /* motion is relative to the entry point, where the cursor now is */
    ENTRY_POINT.replace(point);
    PREV_POS.replace(Some(point));
    ACTIVE_CLIENT.replace(Some(pos));
    log::debug!("capture for {pos} started at {point:?}");
    true
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn handle_request(request: Request) {
    match request {
        Request::Create(pos, range) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos, range));
        }
        Request::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
        Request::Release => {
            ACTIVE_CLIENT.take();
            LOCAL_KEYS.take();
        }
        Request::Focus(pos, range) => {
            activate(pos, range);
        }
        Request::SetHotkeys(hotkeys) => HOTKEYS.with_borrow_mut(|h| h.set_chords(hotkeys)),
    }
}

fn to_rect(display: &RECT) -> Rect {
    Rect {
        x: f64::from(display.left),
        y: f64::from(display.top),
        width: f64::from(display.right - display.left),
        height: f64::from(display.bottom - display.top),
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

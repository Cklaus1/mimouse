//! Low-level keyboard and mouse hooks on a thread of their own: events
//! without the injected flag come from a device.
//!
//! The capture backend installs LL hooks of its own; hooks chain, and both
//! always pass events on with `CallNextHookEx`. While the capture backend
//! captures for a peer it swallows input before hooks installed earlier
//! see it, so a watcher started before capture began misses that input.

use std::{cell::RefCell, sync::mpsc, thread, time::Instant};

use tokio::sync::watch;
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_QUIT, WM_USER,
    },
};

use super::PhysicalError;

pub(super) const METHOD: &str = "hook";

thread_local! {
    /// LL hook procedures run on the thread that installed them
    static EVENTS: RefCell<Option<watch::Sender<()>>> = const { RefCell::new(None) };
}

pub(super) struct Watcher {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn start(events: watch::Sender<()>) -> Result<Self, PhysicalError> {
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("physical-input".into())
            .spawn(move || run(events, ready_tx))
            .map_err(|e| PhysicalError::Failed(format!("thread: {e}")))?;
        match ready.recv() {
            Ok(Ok(thread_id)) => Ok(Watcher {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(PhysicalError::Failed("hook thread ended".into())),
        }
    }

    pub(super) fn injected(&self, _at: Instant) {}
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // the thread created its message queue before reporting its id
        if let Err(e) = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            log::warn!("cannot stop the physical input hooks: {e}");
            return;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(events: watch::Sender<()>, ready: mpsc::SyncSender<Result<u32, PhysicalError>>) {
    EVENTS.replace(Some(events));
    // creates the message queue, so WM_QUIT posted from now on arrives
    let mut msg = MSG::default();
    let _ = unsafe { PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE) };

    let hooks = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0).and_then(|keyboard| {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)
                .inspect_err(|_| unhook(keyboard))
                .map(|mouse| [keyboard, mouse])
        })
    };
    let hooks = match hooks {
        Ok(hooks) => hooks,
        Err(e) => {
            let _ = ready.send(Err(PhysicalError::Failed(format!(
                "SetWindowsHookExW: {e}"
            ))));
            return;
        }
    };
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    // 0: WM_QUIT, -1: error
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    hooks.into_iter().for_each(unhook);
    EVENTS.take();
}

fn unhook(hook: HHOOK) {
    if let Err(e) = unsafe { UnhookWindowsHookEx(hook) } {
        log::warn!("UnhookWindowsHookEx: {e}");
    }
}

fn physical() {
    EVENTS.with_borrow(|events| {
        if let Some(events) = events {
            events.send_replace(());
        }
    });
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !event.flags.contains(LLKHF_INJECTED) {
            physical();
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if event.flags & LLMHF_INJECTED == 0 {
            physical();
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

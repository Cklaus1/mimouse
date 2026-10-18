//! A listen-only session event tap on a CFRunLoop thread of its own:
//! events whose source state is the HID system state come from a device
//! (mimouse's emulation posts from a `CombinedSessionState` source).

use std::{
    ffi::c_void,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use core_foundation::{
    base::TCFType,
    runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode},
};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use tokio::sync::watch;

use super::PhysicalError;

pub(super) const METHOD: &str = "hook";

/// `kCGEventSourceStateHIDSystemState`
const HID_SYSTEM_STATE: i64 = 1;
/// how long the run loop runs before it checks `stop` again: bounds the
/// wait in `drop` when `CFRunLoopStop` came before the loop ran
const RUN_SLICE: Duration = Duration::from_millis(500);

pub(super) struct Watcher {
    run_loop: CFRunLoop,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn start(events: watch::Sender<()>) -> Result<Self, PhysicalError> {
        // a tap on key events without Input Monitoring asks the user;
        // only `macos_permissions` may do that
        if !unsafe { CGPreflightListenEventAccess() } {
            return Err(PhysicalError::Unsupported(
                "no Input Monitoring permission".into(),
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("physical-input".into())
            .spawn({
                let stop = stop.clone();
                move || run(events, stop, ready_tx)
            })
            .map_err(|e| PhysicalError::Failed(format!("thread: {e}")))?;
        match ready.recv() {
            Ok(Ok(run_loop)) => Ok(Watcher {
                run_loop,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(PhysicalError::Failed("event tap thread ended".into())),
        }
    }

    pub(super) fn injected(&self, _at: Instant) {}
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.run_loop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    events: watch::Sender<()>,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CFRunLoop, PhysicalError>>,
) {
    // the tap's mach port, to re-enable it after a timeout
    let port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let callback = {
        let port = port.clone();
        let stop = stop.clone();
        move |_proxy, event_type, event: &core_graphics::event::CGEvent| {
            match event_type {
                CGEventType::TapDisabledByTimeout => {
                    if let Some(&port) = port.get() {
                        unsafe { CGEventTapEnable(port as *mut c_void, true) };
                    }
                }
                // secure input or a revoked permission: the watch ends
                CGEventType::TapDisabledByUserInput => {
                    log::warn!("physical input: event tap disabled");
                    stop.store(true, Ordering::SeqCst);
                    CFRunLoop::get_current().stop();
                }
                _ => {
                    if event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID)
                        == HID_SYSTEM_STATE
                    {
                        events.send_replace(());
                    }
                }
            }
            CallbackResult::Keep
        }
    };
    let tap = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::TailAppendEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
        ],
        callback,
    ) {
        Ok(tap) => tap,
        Err(()) => {
            let _ = ready.send(Err(PhysicalError::Unsupported(
                "cannot create an event tap (Input Monitoring or Accessibility permission)".into(),
            )));
            return;
        }
    };
    let _ = port.set(tap.mach_port().as_concrete_TypeRef() as usize);
    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            let _ = ready.send(Err(PhysicalError::Failed("run loop source".into())));
            return;
        }
    };
    let run_loop = CFRunLoop::get_current();
    unsafe { run_loop.add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();
    let _ = ready.send(Ok(run_loop));

    while !stop.load(Ordering::SeqCst) {
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, RUN_SLICE, false) };
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

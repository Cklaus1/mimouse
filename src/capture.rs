use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use crate::proto::ProtoEvent;
use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, Chord, ChordMatcher, EdgeRange, InputCapture,
    InputCaptureError, Position,
};
use input_event::{
    Event, KeyboardEvent, PointerEvent,
    scancode::Linux::{self, KeyLeftCtrl, KeyRightCtrl},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::{
    sync::watch,
    task::{JoinHandle, spawn_local},
    time::sleep_until,
};
use tokio_util::sync::CancellationToken;

use crate::{
    approval,
    config::ReleaseBind,
    connect::{ConnEvent, MiMouseConnection},
};

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The capture of the entered client was released: control is back on
    /// this machine.
    Released,
    /// the backend waits for the user to approve capturing input (or no
    /// longer does)
    AwaitingApproval(bool),
    /// The backend cannot start capture on its own: move the local pointer
    /// out through the `pos` edge, in the middle of `range`, so it crosses
    /// the barrier there.
    PushPointer {
        pos: Position,
        range: Option<EdgeRange>,
    },
    /// whether a focus (`cli focus`, a hotkey) moved control to the client
    FocusResult(CaptureHandle, Result<(), String>),
}

/// Chords that move control, while it is on a client: `local` (and the
/// release bind) back to this machine, `clients` to that client. Client
/// hotkeys also start capture while local, where the backend sees keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hotkeys {
    pub(crate) local: Option<Vec<Linux>>,
    pub(crate) clients: Vec<(CaptureHandle, Vec<Linux>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType, Option<EdgeRange>),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(ReleaseBind),
    /// move control to a client without a crossing
    Focus(CaptureHandle),
    /// set the hotkeys
    SetHotkeys(Hotkeys),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: MiMouseConnection,
        release_bind: ReleaseBind,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (permission_pending, pending_rx) = watch::channel(false);
        let approval_tx = event_tx.clone();
        // ends with the capture task, which owns the sender
        spawn_local(async move {
            approval::forward(
                pending_rx,
                "input capture",
                approval::GRACE,
                approval::WARN_EVERY,
                |waiting| {
                    let _ = approval_tx.send(ICaptureEvent::AwaitingApproval(waiting));
                },
            )
            .await
        });
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            announced: false,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            permission_pending,
            request_rx,
            hotkey_watch: HotkeyWatch::default(),
            hotkeys: Hotkeys::default(),
            pending_focus: None,
            release_bind,
            ctrl_taps: Default::default(),
            enter_at: None,
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    /// `range`: `[start, end]` percent of the edge that activates the client
    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: crate::ipc::Position,
        capture_type: CaptureType,
        range: Option<[u8; 2]>,
    ) {
        let pos = to_capture_pos(pos);
        let range = range.and_then(|[start, end]| EdgeRange::new(start, end));
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type, range))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: ReleaseBind) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    /// moves control to `handle`; answered by [`ICaptureEvent::FocusResult`]
    pub(crate) fn focus(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Focus(handle))
            .expect("channel closed");
    }

    pub(crate) fn set_hotkeys(&self, hotkeys: Hotkeys) {
        self.request_tx
            .send(CaptureRequest::SetHotkeys(hotkeys))
            .expect("channel closed");
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    /// `ClientEntered` was sent for `active_client` (after the first event
    /// reached it), so its end must be announced with `Released`
    announced: bool,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType, Option<EdgeRange>)>,
    conn: MiMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    /// `true` while the backend waits for a permission request
    permission_pending: watch::Sender<bool>,
    release_bind: ReleaseBind,
    /// the configured hotkeys
    hotkeys: Hotkeys,
    /// release bind and hotkeys, watched while control is on a client
    hotkey_watch: HotkeyWatch,
    /// a focus waiting for its `Begin`
    pending_focus: Option<PendingFocus>,
    ctrl_taps: CtrlDoubleTap,
    /// where the cursor crossed into the active client, if the backend knows
    enter_at: Option<u16>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(
        &mut self,
        handle: CaptureHandle,
        pos: Position,
        capture_type: CaptureType,
        range: Option<EdgeRange>,
    ) {
        self.captures.push((handle, pos, capture_type, range));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t, _)| p == pos && t == CaptureType::Default)
    }

    fn get_capture(&self, handle: CaptureHandle) -> Option<(Position, CaptureType)> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|&(_, pos, capture_type, _)| (pos, capture_type))
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t, r) => self.add_capture(h, p, t, r),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => self.release_bind = bind,
                        CaptureRequest::SetHotkeys(hotkeys) => self.hotkeys = hotkeys,
                        CaptureRequest::Focus(h) => self.focus_done(h, Err("input capture is disabled".into())),
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend, self.permission_pending.clone()) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let mut r = self.create_captures(&mut capture).await;
        if r.is_ok() {
            r = self.apply_hotkeys(&mut capture).await;
        }
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // capture ended by an error, the end of the stream or termination:
        // the peer must not keep keys held
        self.leave_client(&mut capture).await;
        if let Some(pending) = self.pending_focus.take() {
            self.focus_done(pending.handle, Err("input capture stopped".into()));
        }

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, capture_type, range) in captures {
            let enter_only = capture_type == CaptureType::EnterOnly;
            tokio::select! {
                r = capture.create(handle, pos, range, enter_only) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            let focus_deadline = self.pending_focus.as_ref().map(|p| p.deadline);
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ConnEvent::Received(ProtoEvent::Ack(_)) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                        }
                        // client disconnected
                        ConnEvent::Received(ProtoEvent::Leave(_)) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        // the client we control is gone: give control back
                        ConnEvent::Closed if self.active_client == Some(handle) => {
                            log::info!("releasing capture: connection to client {handle} closed");
                            self.release_capture(capture).await?;
                        }
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::Create(h, p, t, r) => {
                        self.add_capture(h, p, t, r);
                        capture.create(h, p, r, t == CaptureType::EnterOnly).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        // the backend's `End` for a destroyed client never
                        // maps back to it, so leave it while it still exists
                        if self.active_client == Some(h) {
                            log::info!("releasing capture: client {h} removed");
                            self.release_capture(capture).await?;
                        }
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind = bind;
                        self.hotkey_watch.set_chords(chords(&self.release_bind, &self.hotkeys));
                    }
                    CaptureRequest::SetHotkeys(hotkeys) => {
                        self.hotkeys = hotkeys;
                        self.apply_hotkeys(capture).await?;
                    }
                    CaptureRequest::Focus(h) => self.focus(capture, h).await?,
                },
                _ = sleep_until(focus_deadline.unwrap_or_else(tokio::time::Instant::now)),
                    if focus_deadline.is_some() => self.focus_timed_out(capture),
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if event == CaptureEvent::End {
            if self.active_client.is_some() {
                log::info!("releasing capture: ended by the compositor");
            }
            // the backend already stopped capturing; releasing it again
            // could end the next activation
            self.leave_client(capture).await;
            return Ok(());
        }

        let Some((pos, capture_type)) = self.get_capture(handle) else {
            log::warn!("dropping {event:?} for unknown capture {handle}");
            return Ok(());
        };

        if let CaptureEvent::Input(e) = event {
            match self.hotkey_watch.action(&e, self.active_client) {
                HotkeyAction::Forward => {}
                HotkeyAction::Ignore => return Ok(()),
                HotkeyAction::Release => {
                    log::info!("releasing capture: hotkey pressed");
                    return self.release_capture(capture).await;
                }
                HotkeyAction::Switch(h) => {
                    log::info!("hotkey pressed: moving control to client {h}");
                    return self.focus(capture, h).await;
                }
            }
        }

        if matches!(event, CaptureEvent::Begin(_)) {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if capture_type == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(pos) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client; announced once an event reaches it
        if matches!(event, CaptureEvent::Begin(_)) && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
            self.announced = false;
        }

        // a focus for this client started capture
        if matches!(event, CaptureEvent::Begin(_))
            && self
                .pending_focus
                .as_ref()
                .is_some_and(|p| p.handle == handle)
        {
            self.pending_focus = None;
            self.focus_done(handle, Ok(()));
        }

        // checked before sending, applied after: the final Ctrl release must
        // reach the peer or Ctrl stays held there
        let double_tapped = match event {
            CaptureEvent::Begin(at) => {
                self.enter_at = at;
                self.ctrl_taps = Default::default();
                false
            }
            CaptureEvent::Input(e) => {
                self.release_bind.double_tap_ctrl && self.ctrl_taps.feed(&e, Instant::now())
            }
            CaptureEvent::End => unreachable!("handled above"),
        };

        let opposite_pos = to_proto_pos(pos.opposite());

        let event = match event {
            CaptureEvent::Begin(_) => enter_event(opposite_pos, self.enter_at),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => enter_event(opposite_pos, self.enter_at),
                State::Sending => ProtoEvent::Input(e),
            },
            CaptureEvent::End => unreachable!("handled above"),
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            return self.release_capture(capture).await;
        }

        if !self.announced && self.active_client == Some(handle) {
            self.announced = true;
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        if double_tapped {
            log::info!("releasing capture: Ctrl double-tap");
            return self.release_capture(capture).await;
        }
        Ok(())
    }

    /// Key-ups for every key still held, a modifier reset and `Leave` for
    /// the active client. Every path that ends capturing goes through here.
    async fn leave_client(&mut self, capture: &mut InputCapture) {
        // key-ups from now on are not seen
        self.hotkey_watch.reset();
        if let Some(handle) = self.active_client.take() {
            self.send_leave(handle, capture.take_pressed_keys()).await;
            // control is local again, even if the peer was unreachable
            if std::mem::take(&mut self.announced) {
                self.event_tx
                    .send(ICaptureEvent::Released)
                    .expect("channel closed");
            }
        }
    }

    /// Moves control to `handle` without a crossing (a hotkey, `cli focus`):
    /// leaves the active client, then lets the backend start capture, or
    /// pushes the local pointer through the barrier where it cannot. The
    /// result follows as [`ICaptureEvent::FocusResult`].
    async fn focus(
        &mut self,
        capture: &mut InputCapture,
        handle: CaptureHandle,
    ) -> Result<(), CaptureError> {
        if self.active_client == Some(handle) {
            self.focus_done(handle, Ok(()));
            return Ok(());
        }
        if let Some(pending) = self.pending_focus.take() {
            capture.clear_focus();
            self.focus_done(
                pending.handle,
                Err(format!("control moved to client {handle} instead")),
            );
        }
        let Some(&(_, pos, _, range)) = self
            .captures
            .iter()
            .find(|&&(h, _, t, _)| h == handle && t == CaptureType::Default)
        else {
            self.focus_done(handle, Err(format!("client {handle} is not active")));
            return Ok(());
        };
        if self.active_client.is_some() {
            // the hotkey's modifiers are still held: the next hotkey that
            // shares them must fire without pressing them again
            let watch = self.hotkey_watch.clone();
            self.release_capture(capture).await?;
            self.hotkey_watch = watch;
        }
        let deadline = tokio::time::Instant::now() + FOCUS_TIMEOUT;
        let pushed = match capture.focus(handle).await {
            Ok(()) => false,
            Err(CaptureError::FocusByPointerPush) => {
                self.event_tx
                    .send(ICaptureEvent::PushPointer { pos, range })
                    .expect("channel closed");
                true
            }
            Err(e) => {
                log::warn!("cannot move control to client {handle}: {e}");
                self.focus_done(handle, Err(e.to_string()));
                return Ok(());
            }
        };
        self.pending_focus = Some(PendingFocus {
            handle,
            deadline,
            pushed,
        });
        Ok(())
    }

    /// a focus got no `Begin` in time
    fn focus_timed_out(&mut self, capture: &mut InputCapture) {
        let Some(pending) = self.pending_focus.take() else {
            return;
        };
        capture.clear_focus();
        let error = if pending.pushed {
            log::warn!(
                "focus on client {}: the pushed pointer did not activate the barrier",
                pending.handle
            );
            FOCUS_BY_PUSH_UNAVAILABLE.to_string()
        } else {
            format!("capture for client {} did not start", pending.handle)
        };
        self.focus_done(pending.handle, Err(error));
    }

    fn focus_done(&self, handle: CaptureHandle, result: Result<(), String>) {
        self.event_tx
            .send(ICaptureEvent::FocusResult(handle, result))
            .expect("channel closed");
    }

    /// applies release bind and hotkeys: to the chords watched while
    /// capturing, and the client hotkeys to the backend
    async fn apply_hotkeys(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.hotkey_watch
            .set_chords(chords(&self.release_bind, &self.hotkeys));
        let backend = self
            .hotkeys
            .clients
            .iter()
            .filter_map(|(h, keys)| Some((Chord::new(keys.iter().copied())?, *h)))
            .collect();
        capture.set_hotkeys(backend).await
    }

    async fn send_leave(&self, handle: CaptureHandle, pressed: impl IntoIterator<Item = Linux>) {
        log::info!("sending Leave event to client {handle}");
        for event in leave_events(pressed) {
            if let Err(e) = self.conn.send(event, handle).await {
                // the remaining events would fail the same way
                log::warn!("failed to send {event} to client {handle}: {e}");
                return;
            }
        }
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.leave_client(capture).await;
        capture.release().await
    }
}

/// `Enter`, with the crossing point when the backend reported it; Lan Mouse
/// peers read both as `Enter`
fn enter_event(pos: crate::proto::Position, at: Option<u16>) -> ProtoEvent {
    match at {
        Some(at) => ProtoEvent::EnterAt(pos, at),
        None => ProtoEvent::Enter(pos),
    }
}

/// Buttons released on leave: capture tracks keys but not buttons, and a
/// peer drops a release for a button it does not hold.
const LEAVE_BUTTONS: [u32; 5] = [
    input_event::BTN_LEFT,
    input_event::BTN_RIGHT,
    input_event::BTN_MIDDLE,
    input_event::BTN_BACK,
    input_event::BTN_FORWARD,
];

/// What a peer needs to stop being controlled: key-ups for every key that
/// was forwarded as pressed (the release-bind chord, or keys held when the
/// compositor ended capture, would otherwise stay held there until its
/// watchdog fires), button-ups (a drag in progress), then a modifier reset
/// (its XKB state is separate from pressed keys, e.g. a locked CapsLock),
/// then `Leave`, which may be lost.
fn leave_events(pressed: impl IntoIterator<Item = Linux>) -> Vec<ProtoEvent> {
    let key_ups = pressed.into_iter().map(|key| {
        ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state: 0,
        }))
    });
    let button_ups = LEAVE_BUTTONS.map(|button| {
        ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state: 0,
        }))
    });
    let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
        depressed: 0,
        latched: 0,
        locked: 0,
        group: 0,
    }));
    key_ups
        .chain(button_ups)
        .chain([mods_zero, ProtoEvent::Leave(0)])
        .collect()
}

/// how long a focus waits for capture to start
const FOCUS_TIMEOUT: Duration = Duration::from_secs(1);

/// a pointer pushed through the barrier did not start capture: GNOME, at
/// least, ignores barriers for emulated motion
const FOCUS_BY_PUSH_UNAVAILABLE: &str = "focus is not available on this desktop session (the \
     compositor, e.g. GNOME, ignores barriers for emulated motion): move the pointer across \
     the edge";

/// a focus waiting for capture to start
#[derive(Debug)]
struct PendingFocus {
    handle: CaptureHandle,
    deadline: tokio::time::Instant,
    /// the local pointer was pushed through the barrier
    pushed: bool,
}

/// where a hotkey moves control
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Local,
    Client(CaptureHandle),
}

/// the chords watched while control is on a client: the release bind and
/// the local hotkey return it, client hotkeys move it on
fn chords(release_bind: &ReleaseBind, hotkeys: &Hotkeys) -> Vec<(Chord, Target)> {
    let local = [Some(&release_bind.keys), hotkeys.local.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|keys| Some((Chord::new(keys.iter().copied())?, Target::Local)));
    let clients = hotkeys
        .clients
        .iter()
        .filter_map(|(h, keys)| Some((Chord::new(keys.iter().copied())?, Target::Client(*h))));
    local.chain(clients).collect()
}

/// what an input event does while control is on `active`
#[derive(Debug, PartialEq, Eq)]
enum HotkeyAction {
    /// no hotkey: send it to the client
    Forward,
    /// the hotkey of the active client: nothing to do, the key is dropped
    Ignore,
    /// return control to this machine
    Release,
    /// move control to this client
    Switch(CaptureHandle),
}

/// The hotkeys watched while control is on a client. A key that completed
/// a hotkey is dropped until it goes up, auto-repeat included.
#[derive(Clone, Debug, Default)]
struct HotkeyWatch {
    chords: ChordMatcher<Target>,
    swallowed: Vec<u32>,
}

impl HotkeyWatch {
    fn set_chords(&mut self, chords: Vec<(Chord, Target)>) {
        self.chords.set_chords(chords);
    }

    /// forget held keys: their key-ups will not be seen
    fn reset(&mut self) {
        self.chords.reset();
        self.swallowed.clear();
    }

    /// what `event` does while control is on `active`
    fn action(&mut self, event: &Event, active: Option<CaptureHandle>) -> HotkeyAction {
        let Event::Keyboard(KeyboardEvent::Key { key, state, .. }) = *event else {
            return HotkeyAction::Forward;
        };
        let target = self.chords.feed(key, state);
        if self.swallowed.contains(&key) {
            if state == 0 {
                self.swallowed.retain(|&k| k != key);
            }
            return HotkeyAction::Ignore;
        }
        let Some(target) = target else {
            return HotkeyAction::Forward;
        };
        self.swallowed.push(key);
        match target {
            Target::Local => HotkeyAction::Release,
            Target::Client(h) if Some(h) == active => HotkeyAction::Ignore,
            Target::Client(h) => HotkeyAction::Switch(h),
        }
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Ctrl (left or right) pressed and released twice in quick succession, with
/// no other key, button or scroll in between, so Ctrl+C or Ctrl+click never
/// trigger it
#[derive(Debug, Default)]
struct CtrlDoubleTap {
    /// when the currently held Ctrl went down
    down: Option<Instant>,
    /// when the previous tap ended
    tapped: Option<Instant>,
}

impl CtrlDoubleTap {
    /// a tap is a press held at most this long
    const MAX_HOLD: Duration = Duration::from_millis(300);
    /// the second tap must start at most this long after the first ended
    const MAX_GAP: Duration = Duration::from_millis(400);

    /// returns true when `event` completes a double tap
    fn feed(&mut self, event: &Event, now: Instant) -> bool {
        let (key, state) = match *event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => (key, state),
            // modifier updates accompany every Ctrl press; motion is harmless
            Event::Keyboard(KeyboardEvent::Modifiers { .. })
            | Event::Pointer(PointerEvent::Motion { .. }) => return false,
            _ => {
                *self = Self::default();
                return false;
            }
        };
        if key != KeyLeftCtrl as u32 && key != KeyRightCtrl as u32 {
            *self = Self::default();
            return false;
        }
        if state != 0 {
            // auto-repeat keeps the original press time
            if self.down.is_none() {
                if self
                    .tapped
                    .is_some_and(|t| now.duration_since(t) > Self::MAX_GAP)
                {
                    self.tapped = None;
                }
                self.down = Some(now);
            }
            return false;
        }
        let Some(down) = self.down.take() else {
            return false;
        };
        if now.duration_since(down) > Self::MAX_HOLD {
            self.tapped = None;
            return false;
        }
        if self.tapped.take().is_some() {
            return true;
        }
        self.tapped = Some(now);
        false
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: crate::ipc::Position) -> input_capture::Position {
    match pos {
        crate::ipc::Position::Left => input_capture::Position::Left,
        crate::ipc::Position::Right => input_capture::Position::Right,
        crate::ipc::Position::Top => input_capture::Position::Top,
        crate::ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> crate::proto::Position {
    match pos {
        input_capture::Position::Left => crate::proto::Position::Left,
        input_capture::Position::Right => crate::proto::Position::Right,
        input_capture::Position::Top => crate::proto::Position::Top,
        input_capture::Position::Bottom => crate::proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: input_event::scancode::Linux, down: bool) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state: down as u8,
        })
    }

    /// feeds `(ms, event)` pairs; returns the times at which a double tap fired
    fn fired(events: &[(u64, Event)]) -> Vec<u64> {
        let start = Instant::now();
        let mut taps = CtrlDoubleTap::default();
        events
            .iter()
            .filter(|(ms, e)| taps.feed(e, start + Duration::from_millis(*ms)))
            .map(|(ms, _)| *ms)
            .collect()
    }

    #[test]
    fn quick_double_tap_fires_on_second_release() {
        let ctrl = |down| key(KeyLeftCtrl, down);
        let repeat = ctrl(true);
        let events = [
            (0, ctrl(true)),
            (80, ctrl(false)),
            (200, ctrl(true)),
            (230, repeat),
            (280, ctrl(false)),
        ];
        assert_eq!(fired(&events), [280]);
        // left then right Ctrl counts too
        let mixed = [
            (0, ctrl(true)),
            (80, ctrl(false)),
            (200, key(KeyRightCtrl, true)),
            (280, key(KeyRightCtrl, false)),
        ];
        assert_eq!(fired(&mixed), [280]);
    }

    #[test]
    fn other_input_between_taps_cancels() {
        let ctrl = |down| key(KeyLeftCtrl, down);
        let click = Event::Pointer(PointerEvent::Button {
            time: 0,
            button: 0x110,
            state: 1,
        });
        let motion = Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 0.0,
        });
        // Ctrl+C, Ctrl+V typed fast
        let copy_paste = [
            (0, ctrl(true)),
            (20, key(input_event::scancode::Linux::KeyC, true)),
            (40, ctrl(false)),
            (100, ctrl(true)),
            (120, key(input_event::scancode::Linux::KeyV, true)),
            (140, ctrl(false)),
        ];
        assert!(fired(&copy_paste).is_empty());
        let ctrl_click = [
            (0, ctrl(true)),
            (20, click),
            (40, ctrl(false)),
            (100, ctrl(true)),
            (140, ctrl(false)),
        ];
        assert!(fired(&ctrl_click).is_empty());
        // moving the mouse between taps is fine
        let moving = [
            (0, ctrl(true)),
            (40, ctrl(false)),
            (60, motion),
            (100, ctrl(true)),
            (140, ctrl(false)),
        ];
        assert_eq!(fired(&moving), [140]);
    }

    #[test]
    fn slow_taps_or_long_holds_do_not_fire() {
        let ctrl = |down| key(KeyLeftCtrl, down);
        // gap just over MAX_GAP; the third tap pairs with the second
        let slow = [
            (0, ctrl(true)),
            (50, ctrl(false)),
            (451, ctrl(true)),
            (500, ctrl(false)),
            (600, ctrl(true)),
            (650, ctrl(false)),
        ];
        assert_eq!(fired(&slow), [650]);
        // second press held past MAX_HOLD
        let held = [
            (0, ctrl(true)),
            (50, ctrl(false)),
            (100, ctrl(true)),
            (401, ctrl(false)),
        ];
        assert!(fired(&held).is_empty());
    }

    #[test]
    fn leaving_releases_every_held_key_before_leave() {
        let held = [KeyLeftCtrl, Linux::KeyLeftShift, Linux::KeyA];
        let events = leave_events(held);
        let buttons = LEAVE_BUTTONS.len();
        assert_eq!(events.len(), held.len() + buttons + 2);
        let mut released: Vec<u32> = events[..held.len()]
            .iter()
            .map(|e| match e {
                ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    key, state: 0, ..
                })) => *key,
                e => panic!("expected a key-up, got {e}"),
            })
            .collect();
        released.sort();
        let mut expected: Vec<u32> = held.iter().map(|&k| k as u32).collect();
        expected.sort();
        assert_eq!(released, expected);
        // a drag in progress ends too
        let button_ups: Vec<u32> = events[held.len()..held.len() + buttons]
            .iter()
            .map(|e| match e {
                ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
                    button,
                    state: 0,
                    ..
                })) => *button,
                e => panic!("expected a button-up, got {e}"),
            })
            .collect();
        assert!(button_ups.contains(&input_event::BTN_LEFT));
        assert!(matches!(
            events[held.len() + buttons],
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0
            }))
        ));
        assert!(matches!(
            events[held.len() + buttons + 1],
            ProtoEvent::Leave(_)
        ));
        // nothing held: still release buttons, reset modifiers and leave
        assert_eq!(leave_events([]).len(), buttons + 2);
    }

    /// release bind Ctrl+Shift+Alt+Meta, local hotkey Ctrl+Alt+0, client 1
    /// on Ctrl+Alt+1, client 2 on Ctrl+Alt+2
    fn hotkey_watch() -> HotkeyWatch {
        use Linux::*;
        let release_bind = ReleaseBind {
            keys: vec![KeyLeftCtrl, KeyLeftShift, KeyLeftAlt, KeyLeftMeta],
            double_tap_ctrl: true,
        };
        let hotkeys = Hotkeys {
            local: Some(vec![KeyLeftCtrl, KeyLeftAlt, Key0]),
            clients: vec![
                (1, vec![KeyLeftCtrl, KeyLeftAlt, Key1]),
                (2, vec![KeyLeftCtrl, KeyLeftAlt, Key2]),
                // no keys: never matches
                (3, vec![]),
            ],
        };
        let mut watch = HotkeyWatch::default();
        watch.set_chords(chords(&release_bind, &hotkeys));
        watch
    }

    /// presses `keys` in order while control is on `active`; returns the
    /// action of the last press
    fn press(keys: &[Linux], active: CaptureHandle) -> HotkeyAction {
        let mut watch = hotkey_watch();
        let mut action = HotkeyAction::Forward;
        for &k in keys {
            action = watch.action(&key(k, true), Some(active));
        }
        action
    }

    #[test]
    fn hotkey_moves_control_to_another_client() {
        use Linux::*;
        assert_eq!(
            press(&[KeyLeftCtrl, KeyLeftAlt, Key2], 1),
            HotkeyAction::Switch(2)
        );
        // the modifiers alone go to the client
        assert_eq!(press(&[KeyLeftCtrl, KeyLeftAlt], 1), HotkeyAction::Forward);
        // the active client's own hotkey does nothing
        assert_eq!(
            press(&[KeyLeftAlt, KeyLeftCtrl, Key1], 1),
            HotkeyAction::Ignore
        );
    }

    #[test]
    fn local_hotkey_and_release_bind_return_control() {
        use Linux::*;
        assert_eq!(
            press(&[KeyLeftCtrl, KeyLeftAlt, Key0], 1),
            HotkeyAction::Release
        );
        assert_eq!(
            press(&[KeyLeftMeta, KeyLeftCtrl, KeyLeftShift, KeyLeftAlt], 1),
            HotkeyAction::Release
        );
    }

    #[test]
    fn unknown_keys_and_supersets_are_forwarded() {
        use Linux::*;
        assert_eq!(
            press(&[KeyLeftCtrl, KeyLeftAlt, Key3], 1),
            HotkeyAction::Forward
        );
        // an extra Shift: Ctrl+Shift+Alt+2 is not Ctrl+Alt+2
        assert_eq!(
            press(&[KeyLeftCtrl, KeyLeftShift, KeyLeftAlt, Key2], 1),
            HotkeyAction::Forward
        );
        // non-key events never match
        let mut watch = hotkey_watch();
        let motion = Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.,
            dy: 0.,
        });
        assert_eq!(watch.action(&motion, Some(1)), HotkeyAction::Forward);
    }

    #[test]
    fn the_final_key_is_dropped_until_it_goes_up() {
        use Linux::*;
        let mut watch = hotkey_watch();
        for k in [KeyLeftCtrl, KeyLeftAlt, Key2] {
            watch.action(&key(k, true), Some(1));
        }
        // auto-repeat and the key-up of the final key
        assert_eq!(
            watch.action(&key(Key2, true), Some(2)),
            HotkeyAction::Ignore
        );
        assert_eq!(
            watch.action(&key(Key2, false), Some(2)),
            HotkeyAction::Ignore
        );
        // Key1 with the modifiers still held, now that control is on 2
        assert_eq!(
            watch.action(&key(Key1, true), Some(2)),
            HotkeyAction::Switch(1)
        );
        // after a reset (control went local) the held keys are forgotten
        watch.reset();
        assert_eq!(
            watch.action(&key(Key1, false), Some(1)),
            HotkeyAction::Forward
        );
        assert_eq!(
            watch.action(&key(Key2, true), Some(1)),
            HotkeyAction::Forward
        );
    }
}

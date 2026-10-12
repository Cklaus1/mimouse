use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use crate::proto::ProtoEvent;
use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{
    Event, KeyboardEvent, PointerEvent,
    scancode::Linux::{KeyLeftCtrl, KeyRightCtrl},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::{
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
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(ReleaseBind),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: MiMouseConnection,
        release_bind: ReleaseBind,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            ctrl_taps: Default::default(),
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

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: crate::ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
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
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: MiMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<ReleaseBind>>,
    ctrl_taps: CtrlDoubleTap,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
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
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
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
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                },
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

        if capture.keys_pressed(&self.release_bind.borrow().keys) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        // checked before sending, applied after: the final Ctrl release must
        // reach the peer or Ctrl stays held there
        let double_tapped = match event {
            CaptureEvent::Begin => {
                self.ctrl_taps = Default::default();
                false
            }
            CaptureEvent::Input(e) => {
                self.release_bind.borrow().double_tap_ctrl
                    && self.ctrl_taps.feed(&e, Instant::now())
            }
        };

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter(opposite_pos),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos),
                State::Sending => ProtoEvent::Input(e),
            },
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            return capture.release().await;
        }

        if double_tapped {
            log::info!("releasing capture: Ctrl double-tap");
            return self.release_capture(capture).await;
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
            self.event_tx
                .send(ICaptureEvent::Released)
                .expect("channel closed");
        }
        capture.release().await
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
}

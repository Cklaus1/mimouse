//! The target side of agent control: grants, the one session, preemption,
//! and carrying out requests (tasks/agent-control.md A2, A5–A8, K1–K3).
//! Requests come from the local IPC ([`Requester::Local`]) and from peers
//! over the agent channel (`serve`).

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use input_event::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::{JoinHandle, spawn_blocking, spawn_local},
    time::{self, sleep_until},
};

use super::proto::{
    self, Blob, Button, Capabilities, EndReason, ErrorCode, Frame, Grant, Grants, Hello, Observe,
    Region, Request, Response, Scope, SessionState, Shot, State, TextSupport,
};
use crate::config::Requester;

/// most keys in one chord
pub(crate) const MAX_CHORD: usize = 8;
/// most characters in one `type` request
pub(crate) const MAX_TEXT: usize = 10_000;
/// most clicks in one `click` request
pub(crate) const MAX_CLICKS: u8 = 3;
/// most wheel notches in one `scroll` request, per axis
pub(crate) const MAX_NOTCHES: i32 = 100;

/// why something could not be done, for the agent to act on
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl Refused {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<Refused> for Response {
    fn from(r: Refused) -> Self {
        Response::error(r.code, r.message)
    }
}

impl From<desktop::ScreenError> for Refused {
    fn from(e: desktop::ScreenError) -> Self {
        let code = match e {
            desktop::ScreenError::Unsupported(_) => ErrorCode::Unsupported,
            desktop::ScreenError::Unavailable(_) => ErrorCode::Unavailable,
            desktop::ScreenError::Failed(_) => ErrorCode::Internal,
        };
        Refused::new(code, e.to_string())
    }
}

/// the running emulation backend, as agents see it
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Emulating {
    pub(crate) backend: String,
    pub(crate) absolute: bool,
    pub(crate) text: TextSupport,
}

/// clipboard contents
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clip {
    pub(crate) mime: String,
    pub(crate) data: Vec<u8>,
    pub(crate) available: Vec<String>,
}

/// What the target drives: emulation, the screen, the clipboard and the
/// detector of a person's input. Sessions are numbered by the target;
/// emulation keeps what each one holds and releases it on [`Desk::end`].
pub(crate) trait Desk: 'static {
    type Watch: Watch;
    async fn input(&self, session: u64, event: Event) -> Result<(), Refused>;
    /// places the pointer at `x`,`y` of a desktop of `size` desktop pixels
    async fn pointer_to(
        &self,
        session: u64,
        x: i32,
        y: i32,
        size: (u32, u32),
    ) -> Result<(), Refused>;
    /// Ok: the characters the layout could not produce
    async fn type_text(&self, session: u64, text: &str) -> Result<String, Refused>;
    /// this machine's name
    async fn hostname(&self) -> String;
    /// releases everything `session` holds
    fn end(&self, session: u64);
    /// `None` while emulation is not running
    async fn emulation(&self) -> Option<Emulating>;
    fn can_capture(&self) -> bool;
    async fn capture(&self) -> Result<desktop::Image, Refused>;
    /// the screen once it has not changed for `settle`, at most `timeout`
    async fn settled(&self, settle: Duration, timeout: Duration)
    -> Result<desktop::Image, Refused>;
    async fn geometry(&self) -> Result<desktop::Geometry, Refused>;
    async fn locked(&self) -> Option<bool>;
    async fn idle(&self) -> Option<Duration>;
    fn has_clipboard(&self) -> bool;
    async fn clip_get(&self, mime: Option<String>) -> Result<Clip, Refused>;
    async fn clip_set(&self, mime: String, data: Vec<u8>) -> Result<(), Refused>;
    /// starts watching for a person's input; `None`: not detectable here
    async fn watch(&self) -> Option<Self::Watch>;
}

/// detects a person's input while a session runs
pub(crate) trait Watch: 'static {
    /// this process injected input at `at`
    fn injected(&self, at: Instant);
    /// a person used the machine; `None`: the detector ended
    async fn next(&mut self) -> Option<()>;
}

/// grants and the idle timeout, from `[agent]` in the config
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) local: Grants,
    pub(crate) peers: HashMap<String, Grants>,
    pub(crate) idle_timeout: Duration,
}

impl Policy {
    pub(crate) fn grants(&self, requester: &Requester) -> Grants {
        match requester {
            Requester::Local => self.local,
            Requester::Peer(fingerprint) => {
                self.peers.get(fingerprint).copied().unwrap_or_default()
            }
        }
    }
}

/// a session started or ended, for notices, `watch` and the audit log
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetEvent {
    Started {
        requester: Requester,
        reason: Option<String>,
    },
    Ended {
        requester: Requester,
        reason: EndReason,
    },
}

struct Session {
    id: u64,
    requester: Requester,
    /// end of the latest request
    last: time::Instant,
    /// requests in progress: the session is not idle while one runs
    busy: usize,
    /// the latest injection, for the preemption detector
    injected: watch::Sender<Instant>,
    supervisor: JoinHandle<()>,
}

/// a request in progress in session `id`; dropping it ends the request
struct Busy<'a, D: Desk> {
    target: &'a Target<D>,
    id: u64,
}

impl<'a, D: Desk> Busy<'a, D> {
    fn new(target: &'a Target<D>, id: u64) -> Self {
        if let Some(s) = target.0.state.borrow_mut().session.as_mut() {
            if s.id == id {
                s.busy += 1;
            }
        }
        Self { target, id }
    }
}

impl<D: Desk> Drop for Busy<'_, D> {
    fn drop(&mut self) {
        if let Some(s) = self.target.0.state.borrow_mut().session.as_mut() {
            if s.id == self.id {
                s.busy -= 1;
                s.last = time::Instant::now();
            }
        }
    }
}

struct TargetState {
    policy: Policy,
    session: Option<Session>,
    next_id: u64,
    /// requesters whose session a person ended: refused until they `open`
    preempted: HashSet<Requester>,
    /// the session that ended last, and why
    last_end: Option<(u64, EndReason)>,
    capturing: bool,
    /// desktop size from the last capture or geometry
    size: Option<(u32, u32)>,
}

struct Inner<D: Desk> {
    desk: D,
    state: RefCell<TargetState>,
    events: mpsc::UnboundedSender<TargetEvent>,
    ended: broadcast::Sender<(Requester, EndReason)>,
}

pub(crate) struct Target<D: Desk>(Rc<Inner<D>>);

impl<D: Desk> Clone for Target<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<D: Desk> Target<D> {
    pub(crate) fn new(desk: D, policy: Policy) -> (Self, mpsc::UnboundedReceiver<TargetEvent>) {
        let (events, events_rx) = mpsc::unbounded_channel();
        let (ended, _) = broadcast::channel(16);
        let inner = Inner {
            desk,
            state: RefCell::new(TargetState {
                policy,
                session: None,
                next_id: 1,
                preempted: HashSet::new(),
                last_end: None,
                capturing: false,
                size: None,
            }),
            events,
            ended,
        };
        (Self(Rc::new(inner)), events_rx)
    }

    /// sessions ending, with whose they were
    pub(crate) fn ended(&self) -> broadcast::Receiver<(Requester, EndReason)> {
        self.0.ended.subscribe()
    }

    pub(crate) fn idle_timeout(&self) -> Duration {
        self.0.state.borrow().policy.idle_timeout
    }

    /// New grants or idle timeout. A session whose requester lost a scope
    /// it had ends.
    pub(crate) fn set_policy(&self, policy: Policy) {
        let revoked = {
            let mut state = self.0.state.borrow_mut();
            let revoked = state.session.as_ref().and_then(|s| {
                let before = state.policy.grants(&s.requester);
                let after = policy.grants(&s.requester);
                let lost = [Scope::Input, Scope::Screen, Scope::Clipboard]
                    .into_iter()
                    .any(|scope| {
                        before.get(scope) == Grant::Allow && after.get(scope) != Grant::Allow
                    });
                lost.then_some(s.id)
            });
            state.policy = policy;
            revoked
        };
        if let Some(id) = revoked {
            self.end(id, EndReason::Revoked);
        }
    }

    /// Capture started or stopped. While this machine controls another,
    /// agent input is refused (D11); starting capture ends the session.
    pub(crate) fn set_capturing(&self, capturing: bool) {
        self.0.state.borrow_mut().capturing = capturing;
        if capturing {
            self.end_current(EndReason::Capture);
        }
    }

    /// a peer's KVM input arrived: a person is controlling this machine
    pub(crate) fn person_input(&self) {
        self.end_current(EndReason::Preempted);
    }

    pub(crate) fn shutdown(&self) {
        self.end_current(EndReason::Shutdown);
    }

    /// ends the session of `requester`, if it holds it
    pub(crate) fn end_requester(&self, requester: &Requester, reason: EndReason) {
        let id = {
            let state = self.0.state.borrow();
            state
                .session
                .as_ref()
                .filter(|s| s.requester == *requester)
                .map(|s| s.id)
        };
        if let Some(id) = id {
            self.end(id, reason);
        }
    }

    fn end_current(&self, reason: EndReason) {
        let id = self.0.state.borrow().session.as_ref().map(|s| s.id);
        if let Some(id) = id {
            self.end(id, reason);
        }
    }

    fn end(&self, id: u64, reason: EndReason) {
        let session = {
            let mut state = self.0.state.borrow_mut();
            if state.session.as_ref().is_none_or(|s| s.id != id) {
                return;
            }
            let session = state.session.take().expect("checked");
            if reason == EndReason::Preempted {
                state.preempted.insert(session.requester.clone());
            }
            state.last_end = Some((id, reason));
            session
        };
        // may be the supervisor itself, which returns right after
        session.supervisor.abort();
        self.0.desk.end(id);
        log::info!("agent session of {} ended: {reason}", session.requester);
        let _ = self.0.ended.send((session.requester.clone(), reason));
        let _ = self.0.events.send(TargetEvent::Ended {
            requester: session.requester,
            reason,
        });
    }

    /// the session of `requester`, started if there is none
    fn ensure_session(
        &self,
        requester: &Requester,
        reason: Option<String>,
    ) -> Result<u64, Refused> {
        let mut state = self.0.state.borrow_mut();
        match &mut state.session {
            Some(s) if s.requester == *requester => {
                s.last = time::Instant::now();
                return Ok(s.id);
            }
            Some(_) => {
                return Err(Refused::new(
                    ErrorCode::Busy,
                    "another agent holds the session on this machine; retry later",
                ));
            }
            None => {}
        }
        let id = state.next_id;
        state.next_id += 1;
        let (injected, injected_rx) = watch::channel(Instant::now());
        let supervisor = spawn_local(supervise(Rc::downgrade(&self.0), id, injected_rx));
        state.session = Some(Session {
            id,
            requester: requester.clone(),
            last: time::Instant::now(),
            busy: 0,
            injected,
            supervisor,
        });
        drop(state);
        log::info!("agent session of {requester} started");
        let _ = self.0.events.send(TargetEvent::Started {
            requester: requester.clone(),
            reason,
        });
        Ok(id)
    }

    /// Err if session `id` ended, saying why. Checked right before input
    /// is handed to emulation, so nothing is injected for an ended session
    /// (whose keys emulation already released).
    fn alive(&self, id: u64) -> Result<(), Refused> {
        let state = self.0.state.borrow();
        if state.session.as_ref().is_some_and(|s| s.id == id) {
            return Ok(());
        }
        let reason = state.last_end.filter(|(e, _)| *e == id).map(|(_, r)| r);
        let code = match reason {
            Some(EndReason::Preempted) => ErrorCode::Preempted,
            Some(EndReason::Capture) => ErrorCode::Capturing,
            Some(EndReason::Revoked) => ErrorCode::Denied,
            _ => ErrorCode::Unavailable,
        };
        let why = reason.map_or_else(|| "ended".to_owned(), |r| r.to_string());
        Err(Refused::new(
            code,
            format!("the session ended during the request ({why}); the rest was not done"),
        ))
    }

    fn grants(&self, requester: &Requester) -> Grants {
        self.0.state.borrow().policy.grants(requester)
    }

    /// carries out `request` from `requester`
    pub(crate) async fn handle(&self, requester: &Requester, request: Request) -> Response {
        match self.dispatch(requester, request).await {
            Ok(response) => response,
            Err(refused) => refused.into(),
        }
    }

    async fn dispatch(&self, requester: &Requester, request: Request) -> Result<Response, Refused> {
        let grants = self.grants(requester);
        match request {
            Request::Hello => return Ok(Response::Hello(self.hello(requester, grants).await)),
            Request::Close => {
                self.end_requester(requester, EndReason::Closed);
                return Ok(Response::Ok { frame: None });
            }
            Request::Open { reason } => {
                if grants == Grants::default() {
                    return Err(denied(requester, None));
                }
                self.0.state.borrow_mut().preempted.remove(requester);
                self.ensure_session(requester, reason)?;
                return Ok(Response::Ok { frame: None });
            }
            _ => {}
        }
        let scope = request.scope().expect("actions need a grant");
        if grants.get(scope) != Grant::Allow {
            return Err(denied(requester, Some(scope)));
        }
        if request.observe().is_some() && grants.screen != Grant::Allow {
            return Err(denied(requester, Some(Scope::Screen)));
        }
        if self.0.state.borrow().preempted.contains(requester) {
            return Err(Refused::new(
                ErrorCode::Preempted,
                "a person used this machine and ended the session; ask the user, then send `open` to start a new one",
            ));
        }
        if scope == Scope::Input && self.0.state.borrow().capturing {
            return Err(Refused::new(
                ErrorCode::Capturing,
                "this machine is controlling another one; agent input is refused until control returns",
            ));
        }
        if scope != Scope::Clipboard && self.0.desk.locked().await == Some(true) {
            return Err(Refused::new(
                ErrorCode::Unavailable,
                "the screen is locked; ask the user to unlock it",
            ));
        }
        let id = self.ensure_session(requester, None)?;
        let _busy = Busy::new(self, id);
        let observe = request.observe().cloned();
        let response = self.act(id, request).await?;
        let Some(observe) = observe else {
            return Ok(response);
        };
        let frame = self.observe(&observe).await.map_err(|e| {
            Refused::new(
                e.code,
                format!(
                    "the action was carried out, but observing failed: {}",
                    e.message
                ),
            )
        })?;
        Ok(response.with_frame(frame))
    }

    async fn act(&self, id: u64, request: Request) -> Result<Response, Refused> {
        let done = Response::Ok { frame: None };
        match request {
            Request::Key { keys, .. } => {
                if keys.is_empty() || keys.len() > MAX_CHORD {
                    return Err(bad(format!("a chord has 1 to {MAX_CHORD} keys")));
                }
                let mut pressed = 0;
                let mut result = Ok(());
                for &key in &keys {
                    result = self.inject(id, key_event(key as u32, 1)).await;
                    if result.is_err() {
                        break;
                    }
                    pressed += 1;
                }
                for &key in keys[..pressed].iter().rev() {
                    let released = self.inject(id, key_event(key as u32, 0)).await;
                    result = result.and(released);
                }
                result.map(|()| done)
            }
            Request::Type { text, .. } => {
                if text.chars().count() > MAX_TEXT {
                    return Err(bad(format!("at most {MAX_TEXT} characters per request")));
                }
                self.alive(id)?;
                self.injected(id);
                let unmapped = self.0.desk.type_text(id, &text).await;
                self.injected(id);
                Ok(Response::Typed {
                    unmapped: unmapped?,
                    frame: None,
                })
            }
            Request::PointerTo { x, y, .. } => {
                let (width, height) = self.size().await?;
                if x < 0 || y < 0 || x as u32 >= width || y as u32 >= height {
                    return Err(bad(format!(
                        "{x},{y} is outside the desktop of {width}x{height} pixels"
                    )));
                }
                self.alive(id)?;
                self.injected(id);
                let placed = self.0.desk.pointer_to(id, x, y, (width, height)).await;
                self.injected(id);
                placed.map(|()| done)
            }
            Request::PointerMove { dx, dy, .. } => {
                let motion = PointerEvent::Motion {
                    time: 0,
                    dx: dx.into(),
                    dy: dy.into(),
                };
                self.inject(id, Event::Pointer(motion)).await.map(|()| done)
            }
            Request::Click { button, count, .. } => {
                if !(1..=MAX_CLICKS).contains(&count) {
                    return Err(bad(format!("count is 1 to {MAX_CLICKS}")));
                }
                for _ in 0..count {
                    self.inject(id, button_event(button, 1)).await?;
                    self.inject(id, button_event(button, 0)).await?;
                }
                Ok(done)
            }
            Request::Button {
                button, pressed, ..
            } => self
                .inject(id, button_event(button, pressed.into()))
                .await
                .map(|()| done),
            Request::Scroll { dx, dy, .. } => {
                if dx.abs() > MAX_NOTCHES || dy.abs() > MAX_NOTCHES {
                    return Err(bad(format!("at most {MAX_NOTCHES} notches per axis")));
                }
                for (axis, notches) in [(0, dy), (1, dx)] {
                    if notches != 0 {
                        let scroll = PointerEvent::AxisDiscrete120 {
                            axis,
                            value: notches * 120,
                        };
                        self.inject(id, Event::Pointer(scroll)).await?;
                    }
                }
                Ok(done)
            }
            Request::Screenshot(shot) => {
                let image = self.0.desk.capture().await?;
                Ok(Response::Frame(self.encode(image, shot).await?))
            }
            Request::ClipGet { mime } => {
                let clip = self.0.desk.clip_get(mime).await?;
                Ok(Response::Clip {
                    mime: clip.mime,
                    data: Blob(clip.data),
                    available: clip.available,
                })
            }
            Request::ClipSet { mime, data } => {
                if data.0.len() > crate::clipboard::MAX_CLIPBOARD_SIZE {
                    return Err(bad(format!(
                        "at most {} bytes",
                        crate::clipboard::MAX_CLIPBOARD_SIZE
                    )));
                }
                self.0.desk.clip_set(mime, data.0).await.map(|()| done)
            }
            Request::Hello | Request::Open { .. } | Request::Close => {
                unreachable!("handled in dispatch")
            }
        }
    }

    async fn inject(&self, id: u64, event: Event) -> Result<(), Refused> {
        self.alive(id)?;
        self.injected(id);
        let result = self.0.desk.input(id, event).await;
        self.injected(id);
        result
    }

    /// tells the preemption detector input was injected now
    fn injected(&self, id: u64) {
        if let Some(s) = self.0.state.borrow().session.as_ref() {
            if s.id == id {
                s.injected.send_replace(Instant::now());
            }
        }
    }

    async fn observe(&self, observe: &Observe) -> Result<Frame, Refused> {
        let settle = observe.settle_ms.unwrap_or(Observe::DEFAULT_SETTLE_MS);
        let timeout = observe
            .timeout_ms
            .unwrap_or(Observe::DEFAULT_TIMEOUT_MS)
            .min(Observe::MAX_TIMEOUT_MS);
        let image = self
            .0
            .desk
            .settled(
                Duration::from_millis(settle.into()),
                Duration::from_millis(timeout.into()),
            )
            .await?;
        self.encode(image, observe.shot.clone()).await
    }

    /// crops, scales and encodes off the LocalSet
    async fn encode(&self, image: desktop::Image, shot: Shot) -> Result<Frame, Refused> {
        self.0.state.borrow_mut().size = Some((image.width, image.height));
        let format = match shot.format {
            proto::Format::Png => desktop::Format::Png,
            proto::Format::Jpeg => desktop::Format::Jpeg,
        };
        let region = shot.region.map(|r| desktop::Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        });
        let encoded =
            spawn_blocking(move || desktop::encode(&image, region, shot.max_edge, format))
                .await
                .map_err(|e| Refused::new(ErrorCode::Internal, e.to_string()))??;
        Ok(Frame {
            format: shot.format,
            width: encoded.width,
            height: encoded.height,
            scale: encoded.scale,
            x: encoded.x,
            y: encoded.y,
            data: Blob(encoded.bytes),
        })
    }

    /// desktop size in desktop pixels, measured once
    async fn size(&self) -> Result<(u32, u32), Refused> {
        if let Some(size) = self.0.state.borrow().size {
            return Ok(size);
        }
        let geometry = self.geometry().await?;
        Ok((geometry.width, geometry.height))
    }

    async fn geometry(&self) -> Result<proto::Geometry, Refused> {
        let g = self.0.desk.geometry().await?;
        self.0.state.borrow_mut().size = Some((g.width, g.height));
        let region = |r: &desktop::Rect| Region {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        };
        Ok(proto::Geometry {
            width: g.width,
            height: g.height,
            monitors: g.monitors.iter().map(region).collect(),
        })
    }

    async fn hello(&self, requester: &Requester, grants: Grants) -> Hello {
        let desk = &self.0.desk;
        let emulating = desk.emulation().await;
        let (capturing, session) = {
            let state = self.0.state.borrow();
            let session = match &state.session {
                None => SessionState::None,
                Some(s) if s.requester == *requester => SessionState::Yours,
                Some(_) => SessionState::Other,
            };
            (state.capturing, session)
        };
        let geometry = match grants.screen {
            Grant::Allow => self.geometry().await.ok(),
            Grant::Deny => None,
        };
        Hello {
            version: proto::VERSION,
            os: std::env::consts::OS.to_owned(),
            hostname: desk.hostname().await,
            emulation_backend: emulating.as_ref().map(|e| e.backend.clone()),
            capabilities: Capabilities {
                input: emulating.is_some() && !capturing,
                absolute_pointer: emulating.as_ref().is_some_and(|e| e.absolute),
                text: emulating.as_ref().map_or(TextSupport::None, |e| e.text),
                screen: desk.can_capture(),
                clipboard: desk.has_clipboard(),
                ui_tree: false,
            },
            grants,
            state: State {
                locked: desk.locked().await,
                capturing,
                idle_ms: desk
                    .idle()
                    .await
                    .map(|d| d.as_millis().try_into().unwrap_or(u64::MAX)),
                session,
            },
            geometry,
        }
    }
}

/// Ends session `id` when a person uses the machine or it goes without a
/// request for the idle timeout.
async fn supervise<D: Desk>(
    target: Weak<Inner<D>>,
    id: u64,
    mut injected: watch::Receiver<Instant>,
) {
    let mut watch = match target.upgrade() {
        Some(inner) => {
            let watch = inner.desk.watch().await;
            if watch.is_none() {
                log::info!("agent session {id}: a person's input is not detectable here");
            }
            watch
        }
        None => return,
    };
    if let Some(w) = &watch {
        w.injected(*injected.borrow_and_update());
    }
    loop {
        let deadline = {
            let Some(inner) = target.upgrade() else {
                return;
            };
            let state = inner.state.borrow();
            let idle = state.policy.idle_timeout;
            match &state.session {
                // checked again once the running requests are done
                Some(s) if s.id == id && s.busy > 0 => time::Instant::now() + idle,
                Some(s) if s.id == id => s.last + idle,
                _ => return,
            }
        };
        let person = async {
            match watch.as_mut() {
                Some(w) => w.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            used = person => match used {
                Some(()) => return end(&target, id, EndReason::Preempted),
                None => {
                    log::warn!("agent session {id}: input detector ended");
                    watch = None;
                }
            },
            changed = injected.changed() => match changed {
                Ok(()) => {
                    if let Some(w) = &watch {
                        w.injected(*injected.borrow_and_update());
                    }
                }
                Err(_) => return,
            },
            _ = sleep_until(deadline) => {
                let idle = target.upgrade().is_some_and(|inner| {
                    let state = inner.state.borrow();
                    state.session.as_ref().is_some_and(|s| {
                        s.id == id && s.busy == 0 && s.last + state.policy.idle_timeout <= time::Instant::now()
                    })
                });
                if idle {
                    return end(&target, id, EndReason::Idle);
                }
            }
        }
    }
}

fn end<D: Desk>(target: &Weak<Inner<D>>, id: u64, reason: EndReason) {
    if let Some(inner) = target.upgrade() {
        Target(inner).end(id, reason);
    }
}

fn denied(requester: &Requester, scope: Option<Scope>) -> Refused {
    let what = scope.map_or("nothing".to_owned(), |s| format!("no {s} access"));
    let fix = match scope {
        Some(s) => format!("mimouse cli agent grant {requester} --{s} allow"),
        None => format!("mimouse cli agent grant {requester} --input allow"),
    };
    let message = match scope {
        Some(_) => format!("{requester} has {what} on this machine; its user can run `{fix}`"),
        None => format!("{requester} is granted nothing on this machine; its user can run `{fix}`"),
    };
    Refused::new(ErrorCode::Denied, message)
}

fn bad(message: String) -> Refused {
    Refused::new(ErrorCode::BadRequest, message)
}

fn key_event(key: u32, state: u8) -> Event {
    Event::Keyboard(KeyboardEvent::Key {
        time: 0,
        key,
        state,
    })
}

fn button_event(button: Button, state: u32) -> Event {
    let button = match button {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
    };
    Event::Pointer(PointerEvent::Button {
        time: 0,
        button,
        state,
    })
}

#[cfg(test)]
mod tests {
    use input_event::scancode::Linux;
    use tokio::{sync::Notify, task::LocalSet};

    use super::*;
    use crate::agent::fake::{FakeDesk, allow_all};

    const IDLE: Duration = Duration::from_secs(60);

    fn policy(local: Grants) -> Policy {
        Policy {
            local,
            peers: HashMap::new(),
            idle_timeout: IDLE,
        }
    }

    fn target(
        local: Grants,
    ) -> (
        Target<FakeDesk>,
        FakeDesk,
        mpsc::UnboundedReceiver<TargetEvent>,
    ) {
        let desk = FakeDesk::default();
        let (target, events) = Target::new(desk.clone(), policy(local));
        (target, desk, events)
    }

    fn chord(keys: &[Linux]) -> Request {
        Request::Key {
            keys: keys.to_vec(),
            observe: None,
        }
    }

    fn error_code(response: &Response) -> Option<ErrorCode> {
        match response {
            Response::Error { code, .. } => Some(*code),
            _ => None,
        }
    }

    fn ended(events: &mut mpsc::UnboundedReceiver<TargetEvent>) -> Vec<EndReason> {
        let mut reasons = vec![];
        while let Ok(event) = events.try_recv() {
            if let TargetEvent::Ended { reason, .. } = event {
                reasons.push(reason);
            }
        }
        reasons
    }

    async fn local<F: std::future::Future>(f: F) -> F::Output {
        LocalSet::new().run_until(f).await
    }

    #[tokio::test]
    async fn without_a_grant_nothing_is_injected_and_no_session_starts() {
        local(async {
            let grants = Grants {
                screen: Grant::Allow,
                ..Default::default()
            };
            let (target, desk, mut events) = target(grants);
            let response = target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            assert_eq!(error_code(&response), Some(ErrorCode::Denied));
            assert!(desk.0.input.borrow().is_empty());
            assert!(events.try_recv().is_err(), "no session event");
        })
        .await;
    }

    #[tokio::test]
    async fn a_chord_is_pressed_in_order_and_released_in_reverse() {
        local(async {
            let (target, desk, _events) = target(allow_all());
            let response = target
                .handle(
                    &Requester::Local,
                    chord(&[Linux::KeyLeftCtrl, Linux::KeyLeftShift, Linux::KeyT]),
                )
                .await;
            assert_eq!(response, Response::Ok { frame: None });
            let keys: Vec<(u32, u8)> = desk
                .0
                .input
                .borrow()
                .iter()
                .map(|(_, e)| match e {
                    Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => (*key, *state),
                    other => panic!("{other:?}"),
                })
                .collect();
            let (ctrl, shift, t) = (
                Linux::KeyLeftCtrl as u32,
                Linux::KeyLeftShift as u32,
                Linux::KeyT as u32,
            );
            assert_eq!(
                keys,
                [(ctrl, 1), (shift, 1), (t, 1), (t, 0), (shift, 0), (ctrl, 0)]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn losing_a_grant_ends_the_session_and_releases_its_input() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            let screen_only = Grants {
                screen: Grant::Allow,
                ..Default::default()
            };
            target.set_policy(policy(screen_only));
            assert_eq!(ended(&mut events), [EndReason::Revoked]);
            assert_eq!(*desk.0.ended.borrow(), [1]);
            // keeping every grant does not end the next session
            let (target, _, mut events) = self::target(allow_all());
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            target.set_policy(policy(allow_all()));
            assert!(ended(&mut events).is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn a_person_ends_the_session_until_the_agent_opens_a_new_one() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            tokio::task::yield_now().await;
            let person = desk.0.person.borrow().clone().expect("watching");
            person.send(()).unwrap();
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(ended(&mut events), [EndReason::Preempted]);
            assert_eq!(*desk.0.ended.borrow(), [1]);

            let response = target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            assert_eq!(error_code(&response), Some(ErrorCode::Preempted));
            let open = Request::Open { reason: None };
            assert_eq!(
                target.handle(&Requester::Local, open).await,
                Response::Ok { frame: None }
            );
            let response = target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            assert_eq!(response, Response::Ok { frame: None });
        })
        .await;
    }

    #[tokio::test]
    async fn starting_capture_ends_the_session_and_refuses_input() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            target.set_capturing(true);
            assert_eq!(ended(&mut events), [EndReason::Capture]);
            assert_eq!(*desk.0.ended.borrow(), [1]);
            let response = target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            assert_eq!(error_code(&response), Some(ErrorCode::Capturing));
            target.set_capturing(false);
            let response = target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            assert_eq!(response, Response::Ok { frame: None });
        })
        .await;
    }

    #[tokio::test]
    async fn input_after_the_session_ended_mid_request_is_not_injected() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            let gate = Rc::new(Notify::new());
            *desk.0.gate.borrow_mut() = Some(gate.clone());
            let request = {
                let target = target.clone();
                spawn_local(async move {
                    target
                        .handle(&Requester::Local, chord(&[Linux::KeyLeftCtrl, Linux::KeyA]))
                        .await
                })
            };
            while desk.0.inputs_started.get() == 0 {
                tokio::task::yield_now().await;
            }
            // the ctrl press is with emulation; a person takes over
            target.person_input();
            *desk.0.gate.borrow_mut() = None;
            gate.notify_one();
            let response = request.await.unwrap();
            assert_eq!(error_code(&response), Some(ErrorCode::Preempted));
            assert_eq!(ended(&mut events), [EndReason::Preempted]);
            // only the press already handed over; emulation releases it on `end`
            assert_eq!(desk.0.input.borrow().len(), 1);
            assert_eq!(desk.0.inputs_started.get(), 1);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_without_requests_ends_after_the_idle_timeout() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
            assert!(ended(&mut events).is_empty());
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(ended(&mut events), [EndReason::Idle]);
            assert_eq!(*desk.0.ended.borrow(), [1]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_running_longer_than_the_idle_timeout_keeps_the_session() {
        local(async {
            let (target, desk, mut events) = target(allow_all());
            target
                .handle(&Requester::Local, Request::Open { reason: None })
                .await;
            let gate = Rc::new(Notify::new());
            *desk.0.gate.borrow_mut() = Some(gate.clone());
            let request = {
                let target = target.clone();
                spawn_local(async move {
                    target
                        .handle(&Requester::Local, chord(&[Linux::KeyA]))
                        .await
                })
            };
            tokio::time::sleep(IDLE * 2).await;
            assert!(ended(&mut events).is_empty());
            *desk.0.gate.borrow_mut() = None;
            gate.notify_one();
            assert_eq!(request.await.unwrap(), Response::Ok { frame: None });
            tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
            assert!(
                ended(&mut events).is_empty(),
                "idle counts from the request's end"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(ended(&mut events), [EndReason::Idle]);
        })
        .await;
    }

    #[tokio::test]
    async fn another_requester_is_busy_while_a_session_runs() {
        local(async {
            let mut policy = policy(allow_all());
            policy.peers.insert("peer".into(), allow_all());
            let desk = FakeDesk::default();
            let (target, _events) = Target::new(desk, policy);
            target
                .handle(&Requester::Local, chord(&[Linux::KeyA]))
                .await;
            let peer = Requester::Peer("peer".into());
            let response = target.handle(&peer, chord(&[Linux::KeyA])).await;
            assert_eq!(error_code(&response), Some(ErrorCode::Busy));
            target.handle(&Requester::Local, Request::Close).await;
            let response = target.handle(&peer, chord(&[Linux::KeyA])).await;
            assert_eq!(response, Response::Ok { frame: None });
        })
        .await;
    }
}

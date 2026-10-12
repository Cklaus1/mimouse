use ashpd::{
    desktop::{
        PersistMode, Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSessionOptions,
            InputCapture, Region, ReleaseOptions, StartOptions, Zones,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    env, fs, io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
        watch,
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use input_event::Event;

use crate::{
    CaptureEvent, EdgeRange,
    geometry::{self, Rect},
};

use super::{
    Capture as MiMouseInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * Therefore the session needs to be recreated when the barriers are updated */

/// events that necessitate restarting the capture session
#[derive(Clone, Copy, Debug)]
enum LibeiNotifyEvent {
    Create(Position, Option<EdgeRange>),
    Destroy(Position),
}

/// why an activated capture ended
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEnd {
    /// MiMouse released it
    Released,
    /// EI devices changed; the session has to be recreated
    DevicesChanged,
    /// the compositor deactivated the capture (e.g. the user pressed Super)
    Deactivated,
    /// the compositor disabled the session
    Disabled,
    /// barriers are being updated or capture is terminating
    Cancelled,
}

#[allow(dead_code)]
pub struct LibeiInputCapture {
    input_capture: Pin<Box<InputCapture>>,
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    cancellation_token: CancellationToken,
    terminated: bool,
}

/// The barrier on `r`'s `pos` side covering pixels `[first, end)` along it:
/// (start pos, end pos), inclusive
fn pos_to_barrier(r: &Region, pos: Position, (first, end): (i32, i32)) -> (i32, i32, i32, i32) {
    let (x, y) = (r.x_offset(), r.y_offset());
    let (w, h) = (r.width() as i32, r.height() as i32);
    match pos {
        Position::Left => (x, first, x, end - 1),
        Position::Right => (x + w, first, x + w, end - 1),
        Position::Top => (first, y, end - 1, y),
        Position::Bottom => (first, y + h, end - 1, y + h),
    }
}

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

fn select_barriers(
    zones: &Zones,
    clients: &[(Position, Option<EdgeRange>)],
    next_barrier_id: &mut NonZeroU32,
) -> (Vec<ICBarrier>, HashMap<BarrierID, Position>) {
    let mut pos_for_barrier = HashMap::new();
    let mut barriers: Vec<ICBarrier> = vec![];
    let displays: Vec<Rect> = zones.regions().iter().map(to_rect).collect();

    for &(pos, range) in clients {
        for (r, display) in zones.regions().iter().zip(&displays) {
            // a range shortens the barriers; displays it misses get none
            let Some((first, end)) = geometry::barrier_segment(&displays, display, pos, range)
            else {
                continue;
            };
            let id = *next_barrier_id;
            *next_barrier_id = next_barrier_id
                .checked_add(1)
                .expect("barrier id out of range");
            let position = pos_to_barrier(r, pos, (first as i32, end as i32));
            pos_for_barrier.insert(id, pos);
            barriers.push(ICBarrier::new(id, position));
        }
    }
    (barriers, pos_for_barrier)
}

/// barriers, the client for each barrier id, and the displays (zones)
type Barriers = (Vec<ICBarrier>, HashMap<BarrierID, Position>, Vec<Rect>);

async fn update_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[(Position, Option<EdgeRange>)],
    next_barrier_id: &mut NonZeroU32,
) -> Result<Barriers, ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");
    let displays = zones.regions().iter().map(to_rect).collect();

    let (barriers, id_map) = select_barriers(&zones, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let ashpd_barriers: Vec<Barrier> = barriers.iter().copied().map(|b| b.into()).collect();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok((barriers, id_map, displays))
}

fn to_rect(r: &Region) -> Rect {
    Rect {
        x: f64::from(r.x_offset()),
        y: f64::from(r.y_offset()),
        width: f64::from(r.width()),
        height: f64::from(r.height()),
    }
}

fn capabilities() -> BitFlags<Capabilities> {
    Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen
}

/// Creates a session. With InputCapture v2 the portal can remember the
/// grant (restore token), so only the first session asks the user; v1
/// (GNOME <= 50) asks for every session.
async fn create_session(
    input_capture: &InputCapture,
    permission_pending: &watch::Sender<bool>,
) -> std::result::Result<Session<InputCapture>, ashpd::Error> {
    let version = input_capture.version();
    log::debug!("creating input capture session (portal version {version})");
    if version < 2 {
        let options = CreateSessionOptions::default().set_capabilities(capabilities());
        let _pending = Pending::new(permission_pending);
        let (session, _) = input_capture.create_session(None, options).await?;
        return Ok(session);
    }

    let session = input_capture.create_session2(Default::default()).await?;
    let options = StartOptions::default()
        .set_capabilities(capabilities())
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(read_token());
    let response = {
        let _pending = Pending::new(permission_pending);
        input_capture
            .start(&session, None, options)
            .await?
            .response()?
    };
    // a restore token is valid once; the portal hands out the next one
    if let Some(token) = response.restore_token() {
        if let Err(e) = write_token(token) {
            log::warn!("failed to save input capture restore token: {e}");
        }
    }
    Ok(session)
}

/// flags a permission request as pending while alive, also when the
/// request is cancelled
struct Pending<'a>(&'a watch::Sender<bool>);

impl<'a> Pending<'a> {
    fn new(pending: &'a watch::Sender<bool>) -> Self {
        pending.send_replace(true);
        Self(pending)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

/// next to the RemoteDesktop tokens of emulation and clipboard
fn token_path() -> PathBuf {
    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache"));
    cache.join("mimouse").join("input-capture.token")
}

fn read_token() -> Option<String> {
    let token = fs::read_to_string(token_path()).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn write_token(token: &str) -> io::Result<()> {
    let path = token_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, token)
}

async fn connect_to_eis(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("io.github.cklaus1.MiMouse", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(Position, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_pos: Rc<Cell<Option<Position>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_pos.get();
        handle_ei_event(ei_event, client, &context, &event_tx, &release_session).await?;
    }
}

impl LibeiInputCapture {
    pub async fn new(
        permission_pending: watch::Sender<bool>,
    ) -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Box::pin(InputCapture::new().await?);
        let input_capture_ptr = input_capture.as_ref().get_ref() as *const InputCapture;
        let first_session =
            Some(create_session(unsafe { &*input_capture_ptr }, &permission_pending).await?);

        let (event_tx, event_rx) = mpsc::channel(1);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let notify_release = Arc::new(Notify::new());

        let cancellation_token = CancellationToken::new();

        let capture = do_capture(
            input_capture_ptr,
            notify_rx,
            notify_release.clone(),
            first_session,
            event_tx,
            cancellation_token.clone(),
            permission_pending,
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            input_capture,
            event_rx,
            capture_task,
            notify_capture,
            notify_release,
            cancellation_token,
            terminated: false,
        };

        Ok(producer)
    }
}

async fn do_capture(
    input_capture: *const InputCapture,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    mut session: Option<Session<InputCapture>>,
    event_tx: Sender<(Position, CaptureEvent)>,
    cancellation_token: CancellationToken,
    permission_pending: watch::Sender<bool>,
) -> Result<(), CaptureError> {
    /* safety: libei_task does not outlive Self */
    let input_capture = unsafe { &*input_capture };
    let mut active_clients: Vec<(Position, Option<EdgeRange>)> = vec![];
    let mut next_barrier_id = NonZeroU32::new(1).expect("id must be non-zero");

    let mut zones_changed = input_capture.receive_zones_changed().await?;

    loop {
        // do capture session
        let cancel_session = CancellationToken::new();
        let cancel_update = CancellationToken::new();

        let mut capture_event_occured: Option<LibeiNotifyEvent> = None;
        let mut zones_have_changed = false;

        // kill session if clients need to be updated
        let handle_session_update_request = async {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    log::debug!("cancelled")
                }, /* exit requested */
                _ = cancel_update.cancelled() => {
                    log::debug!("update task cancelled");
                }, /* session exited */
                _ = zones_changed.next() => {
                    log::debug!("zones changed!");
                    zones_have_changed = true
                }, /* zones have changed */
                e = capture_event.recv() => if let Some(e) = e { /* clients changed */
                    log::debug!("capture event: {e:?}");
                    capture_event_occured.replace(e);
                },
            }
            // kill session (might already be dead!)
            log::debug!("=> cancelling session");
            cancel_session.cancel();
        };

        if !active_clients.is_empty() {
            // create session
            let mut session = match session.take() {
                Some(s) => s,
                None => create_session(input_capture, &permission_pending).await?,
            };

            let capture_session = do_capture_session(
                input_capture,
                &mut session,
                &event_tx,
                &active_clients,
                &mut next_barrier_id,
                &notify_release,
                (cancel_session.clone(), cancel_update.clone()),
            );

            let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
            log::debug!("capture session + session_update task done!");

            // disable capture
            log::debug!("disabling input capture");
            if let Err(e) = input_capture.disable(&session, Default::default()).await {
                log::warn!("input_capture.disable(&session) {e}");
            }
            if let Err(e) = session.close().await {
                log::warn!("session.close(): {e}");
            }

            // propagate error from capture session
            capture_result?;
        } else {
            handle_session_update_request.await;
        }

        // update clients if requested
        if let Some(event) = capture_event_occured.take() {
            match event {
                LibeiNotifyEvent::Create(p, range) => {
                    active_clients.retain(|&(pos, _)| pos != p);
                    active_clients.push((p, range));
                }
                LibeiNotifyEvent::Destroy(p) => active_clients.retain(|&(pos, _)| pos != p),
            }
        }

        // break
        if cancellation_token.is_cancelled() {
            break Ok(());
        }
    }
}

async fn do_capture_session(
    input_capture: &InputCapture,
    session: &mut Session<InputCapture>,
    event_tx: &Sender<(Position, CaptureEvent)>,
    active_clients: &[(Position, Option<EdgeRange>)],
    next_barrier_id: &mut NonZeroU32,
    notify_release: &Notify,
    cancel: (CancellationToken, CancellationToken),
) -> Result<(), CaptureError> {
    let (cancel_session, cancel_update) = cancel;
    // current client
    let current_pos = Rc::new(Cell::new(None));

    // connect to eis server
    let (context, _conn, ei_event_stream) = connect_to_eis(input_capture, session).await?;

    // set barriers
    let (barriers, pos_for_barrier_id, displays) =
        update_barriers(input_capture, session, active_clients, next_barrier_id).await?;

    log::debug!("enabling session");
    input_capture.enable(session, Default::default()).await?;

    // cancellation token to release session
    let release_session = Arc::new(Notify::new());

    // async event task
    let cancel_ei_handler = CancellationToken::new();
    let event_chan = event_tx.clone();
    let pos = current_pos.clone();
    let cancel_session_clone = cancel_session.clone();
    let release_session_clone = release_session.clone();
    let cancel_ei_handler_clone = cancel_ei_handler.clone();
    let ei_task = async move {
        tokio::select! {
            r = libei_event_handler(
                ei_event_stream,
                context,
                event_chan,
                release_session_clone,
                pos,
            ) => {
                log::debug!("libei exited: {r:?} cancelling session task");
                cancel_session_clone.cancel();
            }
            _ = cancel_ei_handler_clone.cancelled() => {},
        }
        Ok::<(), CaptureError>(())
    };

    let capture_session_task = async {
        // receiver for activation tokens
        let mut activated = input_capture.receive_activated().await?;
        let mut deactivated = input_capture.receive_deactivated().await?;
        let mut disabled = input_capture.receive_disabled().await?;
        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    // get barrier id from activation
                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => id,
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => find_corresponding_client(&barriers, activated.cursor_position().expect("no cursor position reported by compositor")),
                    };

                    // find client corresponding to barrier
                    let pos = *pos_for_barrier_id.get(&barrier_id).expect("invalid barrier id");
                    current_pos.replace(Some(pos));

                    // client entered => send event
                    let at = activated.cursor_position().and_then(|(x, y)| {
                        geometry::edge_fraction(&displays, pos, (f64::from(x), f64::from(y)))
                    });
                    event_tx.send((pos, CaptureEvent::Begin(at))).await.expect("no channel");

                    let end = loop {
                        tokio::select! {
                            _ = notify_release.notified() => break SessionEnd::Released,
                            _ = release_session.notified() => break SessionEnd::DevicesChanged,
                            Some(d) = deactivated.next() => {
                                // a late signal for an earlier activation is not ours
                                if d.activation_id().is_none() || d.activation_id() == activated.activation_id() {
                                    break SessionEnd::Deactivated;
                                }
                            }
                            Some(_) = disabled.next() => break SessionEnd::Disabled,
                            _ = cancel_session.cancelled() => break SessionEnd::Cancelled,
                        }
                    };
                    log::debug!("capture ended: {end:?}");
                    current_pos.replace(None);
                    if end != SessionEnd::Released {
                        // the remote side must get its key-ups, so capture is told
                        event_tx.send((pos, CaptureEvent::End)).await.expect("no channel");
                    }
                    match end {
                        SessionEnd::Released => release_capture(input_capture, session, activated, pos).await?,
                        SessionEnd::Deactivated => {}
                        SessionEnd::DevicesChanged => {
                            release_capture(input_capture, session, activated, pos).await?;
                            /* for whatever reason, GNOME seems to kill the session
                             * as soon as devices are added or removed, so we need
                             * to cancel */
                            break;
                        }
                        SessionEnd::Disabled | SessionEnd::Cancelled => break,
                    }
                }
                _ = notify_release.notified() => { /* capture release -> we are not capturing anyway, so ignore */
                    log::debug!("release session requested");
                },
                _ = release_session.notified() => { /* release session */
                    log::debug!("ei devices changed");
                    break;
                },
                Some(_) = deactivated.next() => { /* signal for an activation we already released */ },
                Some(_) = disabled.next() => {
                    log::info!("input capture session disabled by the compositor, recreating it");
                    break;
                },
                _ = cancel_session.cancelled() => { /* kill session notify */
                    log::debug!("session cancel requested");
                    break
                },
            }
        }
        // cancel libei task
        log::debug!("session exited: killing libei task");
        cancel_ei_handler.cancel();
        Ok::<(), CaptureError>(())
    };

    let (a, b) = tokio::join!(ei_task, capture_session_task);

    cancel_update.cancel();

    log::debug!("both session and ei task finished!");
    a?;
    b?;

    Ok(())
}

async fn release_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: Activated,
    current_pos: Position,
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let (x, y) = activated
        .cursor_position()
        .expect("compositor did not report cursor position!");
    log::debug!("client entered @ ({x}, {y})");
    let (dx, dy) = match current_pos {
        // offset cursor position to not enter again immediately
        Position::Left => (1., 0.),
        Position::Right => (-1., 0.),
        Position::Top => (0., 1.),
        Position::Bottom => (0., -1.),
    };
    // release 1px to the right of the entered zone
    let cursor_position = (x as f64 + dx, y as f64 + dy);
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(Some(cursor_position));
    input_capture.release(session, release_options).await?;
    Ok(())
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> BarrierID {
    barriers
        .iter()
        .copied()
        .min_by_key(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            distance_to_line(((x1, y1), (x2, y2)), pos) as i32
        })
        .expect("could not find barrier corresponding to client")
        .barrier_id
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

static ALL_CAPABILITIES: &[DeviceCapability] = &[
    DeviceCapability::Pointer,
    DeviceCapability::PointerAbsolute,
    DeviceCapability::Keyboard,
    DeviceCapability::Touch,
    DeviceCapability::Scroll,
    DeviceCapability::Button,
];

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<Position>,
    context: &ei::Context,
    event_tx: &Sender<(Position, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(ALL_CAPABILITIES);
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) | /* EiEvent::DeviceAdded(_) | */ EiEvent::DeviceRemoved(_) => {
            log::debug!("releasing session: {ei_event:?}");
            release_session.notify_waiters();
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)))
        }
        _ => {
            if let Some(pos) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    event_tx.send((pos, CaptureEvent::Input(event))).await.expect("no channel");
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl MiMouseInputCapture for LibeiInputCapture {
    async fn create(
        &mut self,
        pos: Position,
        range: Option<EdgeRange>,
    ) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Create(pos, range))
            .await;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Destroy(pos))
            .await;
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.notify_release.notify_waiters();
        Ok(())
    }

    /// the portal activates capture only when the pointer hits a barrier
    async fn focus(&mut self, _: Position, _: Option<EdgeRange>) -> Result<(), CaptureError> {
        Err(CaptureError::FocusByPointerPush)
    }

    /// local keys are invisible here: a desktop shortcut runs `cli focus`
    async fn set_hotkeys(&mut self, _: Vec<(crate::Chord, Position)>) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        let task = &mut self.capture_task;
        log::debug!("waiting for capture to terminate...");
        let res = if !task.is_finished() {
            task.await.expect("libei task panic")
        } else {
            Ok(())
        };
        self.terminated = true;
        log::debug!("done!");
        res
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            /* this workaround is needed until async drop is stabilized */
            panic!("LibeiInputCapture dropped without being terminated!");
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.capture_task.poll_unpin(cx) {
            Poll::Ready(r) => match r.expect("failed to join") {
                Ok(()) => Poll::Ready(None),
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            Poll::Pending => self.event_rx.poll_recv(cx).map(|e| e.map(Result::Ok)),
        }
    }
}

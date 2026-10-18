use crate::approval;
use crate::connect::Backoff;
use crate::ipc::BackendInUse;
use crate::listen::{ListenEvent, ListenerCreationError, MiMouseListener};
use crate::proto::{Position, ProtoEvent};
use futures::StreamExt;
use futures::future::{FutureExt, LocalBoxFuture};
use input_emulation::{
    Edge, EmulationCreationError, EmulationError, EmulationHandle, InputEmulation,
    InputEmulationError, TextSupport,
};
use input_event::Event;
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::{oneshot, watch},
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
    agent: EmulationAgent,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: crate::ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled, on this backend
    EmulationEnabled(BackendInUse),
    /// capture should be released
    ReleaseNotify,
    /// the backend waits for the user to approve emulating input (or no
    /// longer does)
    AwaitingApproval(bool),
    /// a peer's input was emulated; sent at most once per
    /// [`PEER_INPUT_EVERY`], so an agent session can yield to the user
    PeerInput,
}

/// how often [`EmulationEvent::PeerInput`] is sent at most
const PEER_INPUT_EVERY: Duration = Duration::from_millis(100);

/// an agent's emulation session: its own held input, released when it ends
pub(crate) type AgentSession = u64;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error("input emulation is off")]
    EmulationOff,
    #[error("the emulation backend cannot do this")]
    Unsupported,
    #[error("input emulation failed: {0}")]
    Failed(String),
}

/// what the active emulation backend offers an agent
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentEmulation {
    pub backend: String,
    /// whether it places the pointer at screenshot coordinates
    pub absolute: bool,
    pub text: TextSupport,
}

/// Drives emulation for agent sessions; clones share the emulation task.
#[derive(Clone)]
pub(crate) struct EmulationAgent {
    request_tx: Sender<ProxyRequest>,
}

impl EmulationAgent {
    /// A request whose reply is dropped unanswered when the emulation task
    /// is gone, which reads as [`AgentError::EmulationOff`].
    fn request<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<T>) -> AgentRequest,
    ) -> oneshot::Receiver<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(ProxyRequest::Agent(request(tx)));
        rx
    }

    /// emulates `event` for `session`
    pub(crate) async fn agent_event(
        &self,
        session: AgentSession,
        event: Event,
    ) -> Result<(), AgentError> {
        let reply = self.request(|tx| AgentRequest::Event(session, event, tx));
        reply.await.unwrap_or(Err(AgentError::EmulationOff))
    }

    /// Moves the pointer to `(x, y)` of a `size` screenshot of the whole
    /// desktop; [`AgentError::Unsupported`] if the backend cannot place it.
    pub(crate) async fn agent_pointer_to(
        &self,
        session: AgentSession,
        x: i32,
        y: i32,
        size: (u32, u32),
    ) -> Result<(), AgentError> {
        let reply = self.request(|tx| AgentRequest::PointerTo(session, x, y, size, tx));
        reply.await.unwrap_or(Err(AgentError::EmulationOff))
    }

    /// types `text` for `session`; the characters it could not type
    pub(crate) async fn agent_type(
        &self,
        session: AgentSession,
        text: String,
    ) -> Result<String, AgentError> {
        let reply = self.request(|tx| AgentRequest::Type(session, text, tx));
        reply.await.unwrap_or(Err(AgentError::EmulationOff))
    }

    /// ends `session`, releasing what it holds
    pub(crate) fn agent_end(&self, session: AgentSession) {
        let _ = self
            .request_tx
            .send(ProxyRequest::Agent(AgentRequest::End(session)));
    }

    /// the active backend, `None` while emulation is off
    pub(crate) async fn agent_info(&self) -> Option<AgentEmulation> {
        self.request(AgentRequest::Info).await.ok().flatten()
    }
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    ChangePort(u16),
    /// move the local pointer through `edge` at a fraction of it
    PushPointer(Edge, u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: MiMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let agent = emulation_proxy.agent();
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
            enter_gate: Default::default(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
            agent,
        }
    }

    /// a handle for agent sessions, usable while the emulation runs
    pub(crate) fn agent(&self) -> EmulationAgent {
        self.agent.clone()
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    /// moves the local pointer through `edge` at `fraction` of it, so a
    /// capture barrier there activates (GNOME `cli focus`)
    pub(crate) fn push_pointer(&self, edge: Edge, fraction: u16) {
        self.request_tx
            .send(EmulationRequest::PushPointer(edge, fraction))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: MiMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    enter_gate: EnterGate,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(WATCHDOG_TICK);
        let mut last_response = HashMap::new();
        let mut rejections = RecentRejections::default();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos) | ProtoEvent::EnterAt(pos, _) => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    log::info!("releasing capture: {addr} entered this device");
                                    self.event_tx.send(EmulationEvent::ReleaseNotify).expect("channel closed");
                                    self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                    if let ProtoEvent::EnterAt(_, fraction) = event {
                                        if self.enter_gate.crossing(addr, Instant::now()) {
                                            self.emulation_proxy.enter_at(addr, to_edge(pos), fraction);
                                        }
                                    }
                                    self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).expect("channel closed");
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                self.enter_gate.left(addr);
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejections.report(&fingerprint, Instant::now()) {
                            self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                        }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr) => {
                        self.enter_gate.left(addr);
                        self.listener.reply(addr, ProtoEvent::Leave(0)).await
                    }
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::PushPointer(edge, fraction) => self.emulation_proxy.push_pointer(edge, fraction),
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    for addr in take_expired(&mut last_response, Instant::now(), WATCHDOG_TIMEOUT) {
                        log::warn!("releasing keys: {addr} not responding!");
                        crate::telemetry::COUNTERS.watchdog_releases.bump();
                        self.emulation_proxy.remove(addr);
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                    }
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// A connected peer pings every `PING_INTERVAL` (500 ms), so 3 s of silence
/// is 6 lost datagrams in a row, not a short loss burst (lan-mouse#307)
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(3);
/// keys of a vanished peer stay held at most `WATCHDOG_TIMEOUT + WATCHDOG_TICK`
const WATCHDOG_TICK: Duration = Duration::from_millis(500);
const _: () = assert!(
    WATCHDOG_TIMEOUT.as_nanos() > 2 * crate::connect::PING_INTERVAL.as_nanos(),
    "one lost ping must not release the keys"
);
const _: () = assert!(WATCHDOG_TICK.as_nanos() < WATCHDOG_TIMEOUT.as_nanos());

/// removes and returns the peers not heard from for more than `timeout`
fn take_expired(
    last_response: &mut HashMap<SocketAddr, Instant>,
    now: Instant,
    timeout: Duration,
) -> Vec<SocketAddr> {
    let expired: Vec<SocketAddr> = last_response
        .iter()
        .filter(|&(_, &seen)| now.saturating_duration_since(seen) > timeout)
        .map(|(&addr, _)| addr)
        .collect();
    for addr in &expired {
        last_response.remove(addr);
    }
    expired
}

/// Fingerprints refused recently: a peer dialing several addresses at once
/// is reported once.
#[derive(Default)]
struct RecentRejections {
    at: HashMap<String, Instant>,
}

/// refusals of one fingerprint closer than this are reported once
const REJECTION_REPEAT_WINDOW: Duration = Duration::from_secs(2);
/// remembered fingerprints (fresh certificates are free to make); refusals
/// beyond are reported without dedup
const MAX_RECENT_REJECTIONS: usize = 64;

impl RecentRejections {
    /// records a refusal of `fingerprint`; true if it is to be reported
    fn report(&mut self, fingerprint: &str, now: Instant) -> bool {
        self.at
            .retain(|_, at| now.saturating_duration_since(*at) < REJECTION_REPEAT_WINDOW);
        if self.at.contains_key(fingerprint) {
            return false;
        }
        if self.at.len() < MAX_RECENT_REJECTIONS {
            self.at.insert(fingerprint.to_owned(), now);
        }
        true
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    /// place the pointer where the remote cursor enters
    EnterAt(SocketAddr, Edge, u16),
    /// move the local pointer through an edge
    PushPointer(Edge, u16),
    Remove(SocketAddr),
    Terminate,
    Reenable,
    Agent(AgentRequest),
}

/// requests of [`EmulationAgent`], answered on their reply channel
enum AgentRequest {
    Event(AgentSession, Event, oneshot::Sender<Result<(), AgentError>>),
    PointerTo(
        AgentSession,
        i32,
        i32,
        (u32, u32),
        oneshot::Sender<Result<(), AgentError>>,
    ),
    Type(
        AgentSession,
        String,
        oneshot::Sender<Result<String, AgentError>>,
    ),
    End(AgentSession),
    Info(oneshot::Sender<Option<AgentEmulation>>),
}

impl AgentRequest {
    /// answers as emulation is off; an ended session is forgotten
    fn refuse(self, agents: &mut HashMap<AgentSession, EmulationHandle>) {
        match self {
            AgentRequest::Event(.., reply) | AgentRequest::PointerTo(.., reply) => {
                let _ = reply.send(Err(AgentError::EmulationOff));
            }
            AgentRequest::Type(.., reply) => {
                let _ = reply.send(Err(AgentError::EmulationOff));
            }
            AgentRequest::End(session) => {
                agents.remove(&session);
            }
            AgentRequest::Info(reply) => {
                let _ = reply.send(None);
            }
        }
    }
}

/// Replies `result` to the agent; an error also ends the backend, so it is
/// returned after the agent heard of it.
fn answer<T>(
    reply: oneshot::Sender<Result<T, AgentError>>,
    result: Result<T, EmulationError>,
) -> Result<(), EmulationError> {
    match result {
        Ok(value) => {
            let _ = reply.send(Ok(value));
            Ok(())
        }
        Err(e) => {
            let _ = reply.send(Err(AgentError::Failed(e.to_string())));
            Err(e)
        }
    }
}

/// creates a backend, reporting a pending permission request on the sender
type CreateBackend = Box<
    dyn Fn(
        watch::Sender<bool>,
    ) -> LocalBoxFuture<'static, Result<InputEmulation, EmulationCreationError>>,
>;

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        Self::with_backend(Box::new(move |permission_pending| {
            InputEmulation::new(backend, permission_pending).boxed_local()
        }))
    }

    fn with_backend(create: CreateBackend) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (permission_pending, pending_rx) = watch::channel(false);
        let approval_tx = event_tx.clone();
        // ends with the emulation task, which owns the sender
        spawn_local(async move {
            approval::forward(
                pending_rx,
                "input emulation",
                approval::GRACE,
                approval::WARN_EVERY,
                |waiting| {
                    let _ = approval_tx.send(EmulationEvent::AwaitingApproval(waiting));
                },
            )
            .await
        });
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            create,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            permission_pending,
            handles: Default::default(),
            agents: Default::default(),
            next_id: 0,
            peer_input_at: None,
            backoff: Backoff::default(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    fn agent(&self) -> EmulationAgent {
        EmulationAgent {
            request_tx: self.request_tx.clone(),
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled(_) = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn enter_at(&self, addr: SocketAddr, edge: Edge, fraction: u16) {
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::EnterAt(addr, edge, fraction))
                .expect("channel closed");
        }
    }

    fn push_pointer(&self, edge: Edge, fraction: u16) {
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::PushPointer(edge, fraction))
                .expect("channel closed");
        } else {
            log::warn!("cannot push the pointer: input emulation is disabled");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    create: CreateBackend,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    /// `true` while the backend waits for a permission request
    permission_pending: watch::Sender<bool>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    /// agent sessions' handles, numbered with the peers'
    agents: HashMap<AgentSession, EmulationHandle>,
    next_id: EmulationHandle,
    /// when [`EmulationEvent::PeerInput`] was last sent
    peer_input_at: Option<tokio::time::Instant>,
    /// delay before recreating a backend that ended with a transient error
    backoff: Backoff,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            let retry = match self.do_emulation().await {
                Ok(()) => None,
                Err(e) => {
                    log::warn!("input emulation exited: {e}");
                    match recovery(&e, &mut self.backoff) {
                        Recovery::Retry(delay) => {
                            log::info!("retrying input emulation in {delay:?}");
                            Some(delay)
                        }
                        Recovery::Denied => {
                            log::warn!(
                                "input emulation denied, `mimouse cli enable-emulation` asks again"
                            );
                            None
                        }
                        Recovery::Stop => None,
                    }
                }
            };
            if self.exit_requested.get() {
                break;
            }
            // wait for the retry or a reenable request
            let retry = async {
                match retry {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(retry);
            loop {
                select! {
                    _ = &mut retry => break,
                    request = self.request_rx.recv() => match request.expect("channel closed") {
                        ProxyRequest::Reenable => {
                            self.backoff.reset();
                            break;
                        }
                        ProxyRequest::Terminate => return,
                        ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                        ProxyRequest::EnterAt(..) => { /* emulation inactive => ignore */ }
                        ProxyRequest::PushPointer(..) => { /* emulation inactive => ignore */ }
                        ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                        ProxyRequest::Agent(request) => request.refuse(&mut self.agents),
                    },
                }
            }
            // the previous attempt's permission request ended with it, so no
            // second dialog can be opened next to a pending one
            debug_assert!(!*self.permission_pending.borrow());
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = (self.create)(self.permission_pending.clone()) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx, &mut self.agents) => return Ok(()),
        };
        self.backoff.reset();

        // used to send enabled and disabled events
        let selected = emulation.selected();
        let backend = BackendInUse {
            name: selected.backend.to_string(),
            auto: selected.auto,
        };
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled(backend),
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        let handles: Vec<_> = self
            .handles
            .values()
            .chain(self.agents.values())
            .copied()
            .collect();
        for &handle in &handles {
            tokio::select! {
                _ = emulation.create(handle) => {},
                _ = wait_for_termination(&mut self.request_rx, &mut self.agents) => return Ok(()),
            }
        }
        // sessions that ended meanwhile hold nothing, but have a handle
        for handle in handles {
            if !self.agents.values().any(|&h| h == handle)
                && !self.handles.values().any(|&h| h == handle)
            {
                emulation.destroy(handle).await;
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = self.handle(emulation, addr).await;
                        emulation.consume(event, handle).await?;
                        self.peer_input();
                    },
                    ProxyRequest::EnterAt(addr, edge, fraction) => {
                        let handle = self.handle(emulation, addr).await;
                        // placement is best effort: the motion that follows
                        // still works from wherever the cursor is
                        if let Err(e) = emulation.enter_at(handle, edge, fraction) {
                            log::warn!("could not place the cursor for {addr}: {e}");
                        }
                    }
                    ProxyRequest::PushPointer(edge, fraction) => {
                        emulation.create(PUSH_HANDLE).await;
                        // best effort like enter_at: a failed push must not end emulation
                        match emulation.push_through(PUSH_HANDLE, edge, fraction).await {
                            Ok(true) => {}
                            Ok(false) => log::warn!("cannot push the pointer: the emulation backend cannot place it"),
                            Err(e) => log::warn!("cannot push the pointer: {e}"),
                        }
                    }
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                    ProxyRequest::Agent(request) => self.agent_request(emulation, request).await?,
                },
            }
        }
    }

    /// the emulation handle for `addr`, created on first use
    async fn handle(
        &mut self,
        emulation: &mut InputEmulation,
        addr: SocketAddr,
    ) -> EmulationHandle {
        if let Some(&handle) = self.handles.get(&addr) {
            return handle;
        }
        let handle = self.new_handle(emulation).await;
        self.handles.insert(addr, handle);
        handle
    }

    /// the emulation handle for an agent's `session`, created on first use
    async fn agent_handle(
        &mut self,
        emulation: &mut InputEmulation,
        session: AgentSession,
    ) -> EmulationHandle {
        if let Some(&handle) = self.agents.get(&session) {
            return handle;
        }
        let handle = self.new_handle(emulation).await;
        self.agents.insert(session, handle);
        handle
    }

    async fn new_handle(&mut self, emulation: &mut InputEmulation) -> EmulationHandle {
        let handle = self.next_id;
        self.next_id += 1;
        debug_assert_ne!(handle, PUSH_HANDLE);
        emulation.create(handle).await;
        handle
    }

    /// reports emulated peer input, at most once per [`PEER_INPUT_EVERY`]
    fn peer_input(&mut self) {
        let now = tokio::time::Instant::now();
        if self
            .peer_input_at
            .is_none_or(|at| now.duration_since(at) >= PEER_INPUT_EVERY)
        {
            self.peer_input_at = Some(now);
            self.event_tx
                .send(EmulationEvent::PeerInput)
                .expect("channel closed");
        }
    }

    /// Serves an agent; a backend error is the agent's answer and ends the
    /// backend, as for peer input.
    async fn agent_request(
        &mut self,
        emulation: &mut InputEmulation,
        request: AgentRequest,
    ) -> Result<(), EmulationError> {
        match request {
            AgentRequest::Event(session, event, reply) => {
                let handle = self.agent_handle(emulation, session).await;
                answer(reply, emulation.consume(event, handle).await)
            }
            AgentRequest::PointerTo(session, x, y, size, reply) => {
                let handle = self.agent_handle(emulation, session).await;
                match emulation.pointer_to(handle, x, y, size) {
                    Ok(false) => {
                        let _ = reply.send(Err(AgentError::Unsupported));
                        Ok(())
                    }
                    placed => answer(reply, placed.map(drop)),
                }
            }
            AgentRequest::Type(session, text, reply) => {
                let handle = self.agent_handle(emulation, session).await;
                let typed = emulation.type_text(handle, &text).await;
                answer(reply, typed.map(|typed| typed.unmapped))
            }
            AgentRequest::End(session) => {
                if let Some(handle) = self.agents.remove(&session) {
                    emulation.destroy(handle).await;
                }
                Ok(())
            }
            AgentRequest::Info(reply) => {
                let info = AgentEmulation {
                    backend: emulation.selected().backend.to_string(),
                    absolute: emulation.desktop().is_some(),
                    text: emulation.text_support(),
                };
                let _ = reply.send(Some(info));
                Ok(())
            }
        }
    }
}

/// what the emulation task does after the backend ended with an error
#[derive(Debug, PartialEq)]
enum Recovery {
    /// recreate the backend after the delay
    Retry(Duration),
    /// the user denied the permission request: wait for `enable-emulation`
    /// instead of showing the dialog again
    Denied,
    /// wait for `enable-emulation`
    Stop,
}

/// Retries errors the backend can recover from by itself (EIS or portal
/// gone, portal not up yet at login, no backend available yet) with
/// `backoff`; anything else waits for `enable-emulation`.
fn recovery(error: &InputEmulationError, backoff: &mut Backoff) -> Recovery {
    if error.cancelled_by_user() {
        Recovery::Denied
    } else if error.is_transient() {
        Recovery::Retry(backoff.next())
    } else {
        Recovery::Stop
    }
}

/// emulation handle for pushing the local pointer, never a peer's
const PUSH_HANDLE: EmulationHandle = EmulationHandle::MAX;

/// Places the pointer once per crossing: the sender repeats `Enter` until it
/// sees the Ack, and a late duplicate must not pull the pointer back to the
/// edge after relative motion has moved it.
#[derive(Default)]
struct EnterGate {
    last_enter: HashMap<SocketAddr, Instant>,
}

/// `Enter`s from the same peer closer than this belong to one crossing.
const ENTER_REPEAT_WINDOW: Duration = Duration::from_secs(1);

impl EnterGate {
    /// records an `Enter` from `addr`; true if it starts a new crossing
    fn crossing(&mut self, addr: SocketAddr, now: Instant) -> bool {
        let new = self
            .last_enter
            .get(&addr)
            .is_none_or(|&last| now.saturating_duration_since(last) > ENTER_REPEAT_WINDOW);
        self.last_enter.insert(addr, now);
        new
    }

    /// the cursor left this device for `addr`
    fn left(&mut self, addr: SocketAddr) {
        self.last_enter.remove(&addr);
    }
}

fn to_edge(pos: Position) -> Edge {
    match pos {
        Position::Left => Edge::Left,
        Position::Right => Edge::Right,
        Position::Top => Edge::Top,
        Position::Bottom => Edge::Bottom,
    }
}

fn to_ipc_pos(pos: Position) -> crate::ipc::Position {
    match pos {
        Position::Left => crate::ipc::Position::Left,
        Position::Right => crate::ipc::Position::Right,
        Position::Top => crate::ipc::Position::Top,
        Position::Bottom => crate::ipc::Position::Bottom,
    }
}

/// Answers requests as emulation is off until `Terminate`.
async fn wait_for_termination(
    rx: &mut Receiver<ProxyRequest>,
    agents: &mut HashMap<AgentSession, EmulationHandle>,
) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::EnterAt(..) | ProxyRequest::PushPointer(..) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
            ProxyRequest::Agent(request) => request.refuse(agents),
        }
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

    #[test]
    fn watchdog_tolerates_loss_bursts_but_bounds_stuck_keys() {
        let start = Instant::now();
        let quiet: SocketAddr = "192.0.2.1:4242".parse().unwrap();
        let lossy: SocketAddr = "192.0.2.2:4242".parse().unwrap();
        let mut seen = HashMap::from([(quiet, start), (lossy, start)]);
        let at = |ms| start + Duration::from_millis(ms);

        // 1.5 s without datagrams (3 pings lost) is a loss burst, not a dead peer
        assert!(take_expired(&mut seen, at(1500), WATCHDOG_TIMEOUT).is_empty());
        seen.insert(lossy, at(2900));
        assert!(take_expired(&mut seen, at(3000), WATCHDOG_TIMEOUT).is_empty());

        // the first tick past the timeout releases the silent peer only
        assert_eq!(take_expired(&mut seen, at(3500), WATCHDOG_TIMEOUT), [quiet]);
        assert_eq!(seen.keys().collect::<Vec<_>>(), [&lossy]);
        // ... once
        assert!(take_expired(&mut seen, at(4000), WATCHDOG_TIMEOUT).is_empty());
        assert_eq!(take_expired(&mut seen, at(6400), WATCHDOG_TIMEOUT), [lossy]);
        assert!(seen.is_empty());
    }

    /// B5: refusals of fresh certificates do not grow the map without bound
    #[test]
    fn recent_rejections_stay_bounded() {
        let mut r = RecentRejections::default();
        let t0 = Instant::now();
        assert!(r.report("a", t0));
        assert!(!r.report("a", t0 + Duration::from_millis(1999)));
        assert!(r.report("a", t0 + REJECTION_REPEAT_WINDOW));
        // a flood within the window: every fresh fingerprint is reported,
        // but only a bounded number is remembered
        for i in 0..10_000 {
            assert!(r.report(&format!("fp{i}"), t0 + Duration::from_millis(i)));
            assert!(r.at.len() <= MAX_RECENT_REJECTIONS);
        }
        // expired entries are dropped
        assert!(r.report("b", t0 + Duration::from_secs(100)));
        assert_eq!(r.at.len(), 1);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use input_emulation::{EmulationCreationError, EmulationError};

    #[test]
    fn transient_errors_retry_with_backoff_until_a_backend_comes_up() {
        let mut backoff = Backoff::default();
        let none = InputEmulationError::from(EmulationCreationError::NoAvailableBackend);
        let gone = InputEmulationError::from(EmulationError::EndOfStream);
        let secs = |s| Recovery::Retry(Duration::from_secs(s));
        assert_eq!(recovery(&none, &mut backoff), secs(1));
        assert_eq!(recovery(&none, &mut backoff), secs(2));
        assert_eq!(recovery(&none, &mut backoff), secs(4));
        // a later attempt created the backend (`do_emulation` resets) and
        // the session ended: the next retry starts over
        backoff.reset();
        assert_eq!(recovery(&gone, &mut backoff), secs(1));
        for _ in 0..8 {
            recovery(&gone, &mut backoff);
        }
        assert_eq!(recovery(&gone, &mut backoff), secs(30));
    }

    #[test]
    fn other_errors_wait_for_enable_emulation() {
        let mut backoff = Backoff::default();
        let io = InputEmulationError::from(EmulationError::Io(std::io::ErrorKind::Other.into()));
        assert_eq!(recovery(&io, &mut backoff), Recovery::Stop);
        // no attempt consumed a delay
        let none = InputEmulationError::from(EmulationCreationError::NoAvailableBackend);
        assert_eq!(
            recovery(&none, &mut backoff),
            Recovery::Retry(Duration::from_secs(1))
        );
    }

    #[cfg(libei_emulation)]
    #[test]
    fn denied_dialog_is_not_shown_again() {
        use ashpd::desktop::ResponseError;
        let mut backoff = Backoff::default();
        let denied = InputEmulationError::from(EmulationCreationError::Libei(
            ashpd::Error::Response(ResponseError::Cancelled).into(),
        ));
        assert_eq!(recovery(&denied, &mut backoff), Recovery::Denied);
        // a transient error afterwards still starts at the first delay
        let gone = InputEmulationError::from(EmulationError::EndOfStream);
        assert_eq!(
            recovery(&gone, &mut backoff),
            Recovery::Retry(Duration::from_secs(1))
        );
    }
}

#[cfg(test)]
mod enter_tests {
    use super::*;

    #[test]
    fn places_once_per_crossing() {
        let a: SocketAddr = "10.0.0.1:4242".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:4242".parse().unwrap();
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut gate = EnterGate::default();
        assert!(gate.crossing(a, t0));
        // repeats while the sender waits for the Ack, incl. a late one
        assert!(!gate.crossing(a, ms(5)));
        assert!(!gate.crossing(a, ms(900)));
        assert!(!gate.crossing(a, ms(1800)));
        // other peers are independent
        assert!(gate.crossing(b, ms(10)));
        // the cursor left and comes back quickly: a new crossing
        gate.left(a);
        assert!(gate.crossing(a, ms(1850)));
        // the Leave got lost, but the next crossing is seconds later
        assert!(gate.crossing(a, ms(6000)));
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;
    use input_emulation::{DummyRecord, Received};
    use input_event::KeyboardEvent;
    use tokio::task::LocalSet;

    const KEY_A: u32 = 30;

    fn key(state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: KEY_A,
            state,
        })
    }

    /// a proxy on recording dummy backends
    fn proxy(record: &DummyRecord) -> EmulationProxy {
        let record = record.clone();
        EmulationProxy::with_backend(Box::new(move |_| {
            let emulation = InputEmulation::dummy(record.clone());
            async move { Ok(emulation) }.boxed_local()
        }))
    }

    /// the next event of `proxy` that is not about approval
    async fn next_event(proxy: &mut EmulationProxy) -> EmulationEvent {
        loop {
            match proxy.event().await {
                EmulationEvent::AwaitingApproval(_) => continue,
                event => return event,
            }
        }
    }

    async fn enabled(proxy: &mut EmulationProxy) {
        let event = next_event(proxy).await;
        assert!(matches!(event, EmulationEvent::EmulationEnabled(_)));
    }

    /// the handles created on the backend
    fn created(received: &[Received]) -> Vec<EmulationHandle> {
        let created = received.iter().filter_map(|r| match r {
            Received::Create(handle) => Some(*handle),
            _ => None,
        });
        created.collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_drives_its_own_handle_and_ending_it_releases_it() {
        LocalSet::new()
            .run_until(async {
                let record = DummyRecord::default();
                let mut proxy = proxy(&record);
                let agent = proxy.agent();
                enabled(&mut proxy).await;
                agent.agent_event(1, key(1)).await.unwrap();
                let typed = agent.agent_type(1, "hi\r\n".into()).await.unwrap();
                assert_eq!(typed, "");
                assert_eq!(
                    record.take(),
                    [
                        Received::Create(0),
                        Received::Event(0, key(1)),
                        Received::Typed(0, "hi\n".into()),
                    ]
                );
                agent.agent_end(1);
                // answered in order: the session ended before
                let info = agent.agent_info().await.unwrap();
                let received = record.take();
                assert!(
                    received.contains(&Received::Event(0, key(0))),
                    "{received:?}"
                );
                assert_eq!(received.last(), Some(&Received::Destroy(0)));
                let expected = AgentEmulation {
                    backend: "dummy".into(),
                    absolute: false,
                    text: TextSupport::Exact,
                };
                assert_eq!(info, expected);
                // the dummy cannot place the pointer: no session is opened
                let placed = agent.agent_pointer_to(2, 5, 5, (10, 10)).await;
                assert!(matches!(placed, Err(AgentError::Unsupported)), "{placed:?}");
                proxy.terminate().await;
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn sessions_never_share_a_handle_with_peers_or_the_push() {
        LocalSet::new()
            .run_until(async {
                let record = DummyRecord::default();
                let mut proxy = proxy(&record);
                let agent = proxy.agent();
                enabled(&mut proxy).await;
                let peer: SocketAddr = "192.0.2.1:4242".parse().unwrap();
                proxy.consume(key(1), peer);
                agent.agent_event(1, key(1)).await.unwrap();
                agent.agent_event(2, key(1)).await.unwrap();
                proxy.push_pointer(Edge::Left, 0);
                agent.agent_event(1, key(0)).await.unwrap();
                assert_eq!(created(&record.take()), [0, 1, 2, PUSH_HANDLE]);
                proxy.terminate().await;
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn peer_input_is_reported_at_most_every_100_ms() {
        LocalSet::new()
            .run_until(async {
                let record = DummyRecord::default();
                let mut proxy = proxy(&record);
                let agent = proxy.agent();
                enabled(&mut proxy).await;
                let peer: SocketAddr = "192.0.2.1:4242".parse().unwrap();
                let mut reports = 0;
                for _ in 0..30 {
                    proxy.consume(key(1), peer);
                    proxy.consume(key(0), peer);
                    // processed in order: the peer's input is emulated
                    agent.agent_info().await.unwrap();
                    while let Some(event) = proxy.event().now_or_never() {
                        assert!(matches!(event, EmulationEvent::PeerInput));
                        reports += 1;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                // 300 ms of input: at 0, 100 and 200 ms
                assert_eq!(reports, 3);
                // an agent's input is not a peer's
                agent.agent_event(1, key(1)).await.unwrap();
                tokio::time::sleep(PEER_INPUT_EVERY).await;
                agent.agent_event(1, key(0)).await.unwrap();
                assert!(proxy.event().now_or_never().is_none());
                proxy.terminate().await;
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn while_emulation_is_off_agents_are_told_so() {
        LocalSet::new()
            .run_until(async {
                // creating the backend never ends (an unanswered dialog)
                let pending = EmulationProxy::with_backend(Box::new(|_| {
                    std::future::pending().boxed_local()
                }));
                // no backend works yet: retried with backoff
                let failing = EmulationProxy::with_backend(Box::new(|_| {
                    async { Err(EmulationCreationError::NoAvailableBackend) }.boxed_local()
                }));
                for mut proxy in [pending, failing] {
                    let agent = proxy.agent();
                    let sent = agent.agent_event(1, key(1)).await;
                    assert!(matches!(sent, Err(AgentError::EmulationOff)), "{sent:?}");
                    let typed = agent.agent_type(1, "a".into()).await;
                    assert!(matches!(typed, Err(AgentError::EmulationOff)), "{typed:?}");
                    let placed = agent.agent_pointer_to(1, 0, 0, (1, 1)).await;
                    assert!(
                        matches!(placed, Err(AgentError::EmulationOff)),
                        "{placed:?}"
                    );
                    agent.agent_end(1);
                    assert_eq!(agent.agent_info().await, None);
                    proxy.terminate().await;
                    // the emulation task is gone
                    let sent = agent.agent_event(1, key(1)).await;
                    assert!(matches!(sent, Err(AgentError::EmulationOff)), "{sent:?}");
                }
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_backend_tells_the_agent_and_restarts_with_its_handles() {
        LocalSet::new()
            .run_until(async {
                let record = DummyRecord::default();
                let mut proxy = proxy(&record);
                let agent = proxy.agent();
                enabled(&mut proxy).await;
                agent.agent_event(1, key(1)).await.unwrap();
                agent.agent_event(2, key(1)).await.unwrap();
                record.fail_next();
                let failed = agent.agent_event(1, key(0)).await;
                assert!(matches!(failed, Err(AgentError::Failed(_))), "{failed:?}");
                agent.agent_end(2);
                assert!(matches!(
                    next_event(&mut proxy).await,
                    EmulationEvent::EmulationDisabled
                ));
                enabled(&mut proxy).await;
                // a new backend after the backoff: session 1 keeps its
                // handle, the ended session 2 holds none
                let received = record.take();
                let restarted = received.iter().position(|r| *r == Received::Terminate);
                assert_eq!(created(&received[restarted.unwrap()..]), [0]);
                agent.agent_event(1, key(1)).await.unwrap();
                assert_eq!(record.take(), [Received::Event(0, key(1))]);
                agent.agent_event(3, key(1)).await.unwrap();
                assert_eq!(created(&record.take()), [2]);
                proxy.terminate().await;
            })
            .await;
    }
}

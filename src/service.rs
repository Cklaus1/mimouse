use crate::ipc::{
    AsyncFrontendListener, BackendInUse, ClientHandle, ConnId, FrontendEvent, FrontendRequest,
    IpcEvent, IpcListenerCreationError, Peer, Position, Reply, Request, Status,
};
use crate::{
    approval::reported_status,
    audit::AuditLog,
    capture::{Capture, CaptureType, Hotkeys, ICaptureEvent},
    client::{Barrier, ClientManager},
    clients::{Change, Clients, Sinks},
    clipboard::{Clipboard, ClipboardEvent},
    config::Config,
    connect::MiMouseConnection,
    crypto,
    dns::DnsResolver,
    emulation::{Emulation, EmulationEvent},
    inhibit::IdleInhibit,
    listen::{ListenerCreationError, MiMouseListener},
    notify::Notifier,
    pair::{self, PairEvent, Pairing},
    telemetry::{self, Settings, Telemetry},
};
use hickory_resolver::ResolveError;
use input_capture::EdgeRange;
use input_emulation::Edge;
use log;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Dns(#[from] ResolveError),
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// the clients, their barriers, connections and hotkeys, and the config
    clients: Clients,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// clipboard sharing
    clipboard: Clipboard,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// audit log of control requests
    audit: AuditLog,
    /// `ChangePort` requests waiting for the listener to rebind
    port_requests: Vec<(ConnId, u64, Peer, FrontendRequest)>,
    /// `Focus` requests waiting for the outcome, with the client they move
    /// control to
    focus_requests: Vec<(ConnId, u64, Peer, FrontendRequest, ClientHandle)>,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// frontend events queued for sending
    frontend: Rc<FrontendQueue>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// the capture backend waits for the user to approve a permission request
    capture_awaiting: bool,
    /// the capture backend in use while enabled
    capture_backend: Option<BackendInUse>,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// the emulation backend waits for the user to approve a permission request
    emulation_awaiting: bool,
    /// the emulation backend in use while enabled
    emulation_backend: Option<BackendInUse>,
    /// peers controlling this machine, each with the capture barrier that
    /// gives control back to it
    incoming: HashMap<SocketAddr, Incoming>,
    next_trigger_handle: u64,
    /// desktop notifications (switches, unauthorized peers)
    notifier: Notifier,
    /// keeps this machine awake while it forwards input
    inhibit: IdleInhibit,
    /// pairing with other machines (`mimouse cli pair`)
    pairing: Pairing,
    /// opt-in usage statistics
    telemetry: Telemetry,
}

#[derive(Debug)]
struct Incoming {
    /// the barrier that gives control back to the peer
    handle: ClientHandle,
    fingerprint: String,
    pos: Position,
}

/// frontend events queued for sending, to one connection or all
#[derive(Default)]
pub(crate) struct FrontendQueue {
    events: RefCell<VecDeque<(Option<ConnId>, FrontendEvent)>>,
    pending: Notify,
}

impl FrontendQueue {
    pub(crate) fn send(&self, to: Option<ConnId>, event: FrontendEvent) {
        self.events.borrow_mut().push_back((to, event));
        self.pending.notify_one();
    }

    pub(crate) fn broadcast(&self, event: FrontendEvent) {
        self.send(None, event);
    }

    async fn ready(&self) {
        self.pending.notified().await
    }

    fn pop(&self) -> Option<(Option<ConnId>, FrontendEvent)> {
        self.events.borrow_mut().pop_front()
    }
}

/// client changes take effect in input capture and the clipboard
struct Live<'a> {
    capture: &'a Capture,
    clipboard: &'a Clipboard,
}

impl Sinks for Live<'_> {
    fn create_barrier(&self, handle: ClientHandle, barrier: Barrier) {
        self.capture
            .create(handle, barrier.pos, CaptureType::Default, barrier.range);
    }

    fn destroy_barrier(&self, handle: ClientHandle) {
        self.capture.destroy(handle);
    }

    fn set_hotkeys(&self, hotkeys: Hotkeys) {
        self.capture.set_hotkeys(hotkeys);
    }

    fn client_removed(&self, handle: ClientHandle) {
        self.clipboard.client_removed(handle);
    }
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;
        // the running service only: crashes of CLI commands are not recorded
        telemetry::install_panic_hook(config.config_dir());
        let telemetry = Telemetry::new(&config);

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        let listener =
            MiMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;

        // after the already-running check, so a second instance never prompts
        #[cfg(target_os = "macos")]
        crate::macos_permissions::request_permissions();

        Self::assemble(
            config,
            cert,
            frontend_listener,
            telemetry,
            authorized_keys,
            listener,
        )
    }

    /// the service around the parts that reach outside the process: the
    /// certificate, the IPC and peer listeners, telemetry
    fn assemble(
        config: Config,
        cert: Certificate,
        frontend_listener: AsyncFrontendListener,
        telemetry: Telemetry,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        listener: MiMouseListener,
    ) -> Result<Self, ServiceError> {
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);
        log::info!("certificate fingerprint: {public_key_fingerprint}");
        telemetry::COUNTERS.set_peers(config.clients().len());

        let client_manager = ClientManager::default();
        let conn = MiMouseConnection::new(cert.clone(), client_manager.clone());
        let connector = conn.connector();

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        let (pairing, pair_tx) = Pairing::new(&cert).map_err(io::Error::other)?;
        let clipboard = Clipboard::new(
            config.clipboard(),
            config.port(),
            config.max_file_size(),
            &cert,
            authorized_keys.clone(),
            client_manager.clone(),
            pair_tx,
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;
        connector.set_resolver(resolver.requester());

        let port = config.port();
        let audit = AuditLog::new(config.config_dir());
        let notifier = Notifier::new(config.notify_switch());
        let inhibit = IdleInhibit::new(config.idle_inhibit());
        let frontend = Rc::new(FrontendQueue::default());
        let clients = Clients::new(
            config,
            client_manager,
            connector,
            resolver.requester(),
            authorized_keys,
            frontend.clone(),
        );
        let service = Self {
            clients,
            audit,
            port_requests: Default::default(),
            focus_requests: Default::default(),
            capture,
            emulation,
            clipboard,
            frontend_listener,
            resolver,
            public_key_fingerprint,
            frontend,
            port,
            capture_status: Default::default(),
            capture_awaiting: false,
            capture_backend: None,
            emulation_status: Default::default(),
            emulation_awaiting: false,
            emulation_backend: None,
            incoming: Default::default(),
            next_trigger_handle: 0,
            notifier,
            inhibit,
            pairing,
            telemetry,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let mut shutdown = Shutdown::new()?;
        self.clients.start(&Live {
            capture: &self.capture,
            clipboard: &self.clipboard,
        });

        loop {
            tokio::select! {
                event = self.frontend_listener.next() => self.handle_ipc_event(event).await,
                _ = self.frontend.ready() => self.handle_frontend_pending().await,
                event = self.clipboard.event() => self.handle_clipboard_event(event),
                event = self.emulation.event() => self.handle_emulation_event(event).await,
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.clients.resolved(event),
                event = self.pairing.event() => self.handle_pair_event(event).await,
                change = self.clients.changed() => match change {
                    Change::Config => self.handle_config_change(),
                    Change::Pinned => self.save_pins().await,
                },
                signal = shutdown.recv() => {
                    let signal = signal.expect("failed to wait for a signal");
                    log::info!("{signal} received");
                    break;
                }
            }
        }

        self.terminate().await;
        shutdown.done();
        Ok(())
    }

    /// ends capture (held keys released), emulation, the clipboard and the
    /// resolver, and saves the telemetry counters
    async fn terminate(&mut self) {
        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating clipboard ...");
        self.clipboard.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        log::debug!("saving telemetry counters ...");
        self.telemetry.shutdown().await;
    }

    async fn handle_ipc_event(&mut self, event: IpcEvent) {
        match event {
            IpcEvent::Connected(conn) => {
                for event in self.sync_events() {
                    self.frontend.send(Some(conn), event);
                }
            }
            IpcEvent::Request(conn, Request { seq, request }) => {
                let peer = self
                    .frontend_listener
                    .peer(conn)
                    .cloned()
                    .unwrap_or_default();
                self.telemetry.count_request(&request);
                if let FrontendRequest::ChangePort { port } = request {
                    if let Err(e) = self.clients.config().check_port(port) {
                        let error = e.to_string();
                        self.audit.record(&peer, &request, &Err(error.clone()));
                        self.reply(conn, seq, Err(error));
                        return;
                    }
                    if port != self.port {
                        // answered once the listener has rebound
                        self.emulation.request_port_change(port);
                        self.port_requests.push((conn, seq, peer, request));
                        return;
                    }
                }
                if let FrontendRequest::Focus { id } = request {
                    match self.focus_error(id) {
                        // answered once capture started, or failed to
                        None => {
                            self.capture.focus(id);
                            crate::telemetry::COUNTERS.switch_cli_focus.bump();
                            self.focus_requests.push((conn, seq, peer, request, id));
                        }
                        Some(e) => {
                            self.audit.record(&peer, &request, &Err(e.clone()));
                            self.reply(conn, seq, Err(e));
                        }
                    }
                    return;
                }
                let result = self.handle_request(&request).await;
                if !request.is_read_only() {
                    let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
                    self.audit.record(&peer, &request, &outcome);
                }
                self.reply(conn, seq, result);
                self.reload_if_merged();
                telemetry::COUNTERS.set_peers(self.clients.infos().len());
            }
        }
    }

    /// apply a request; events it causes are queued before its reply.
    /// Requests that change the config apply to the runtime only once it is
    /// saved, so a failed save changes nothing
    async fn handle_request(&mut self, request: &FrontendRequest) -> Result<Reply, String> {
        if let Some(handle) = request.client() {
            if !self.clients.exists(handle) {
                return Err(format!("no such client: {handle}"));
            }
        }
        let fx = Live {
            capture: &self.capture,
            clipboard: &self.clipboard,
        };
        let clients = &mut self.clients;
        let reply = match request.clone() {
            FrontendRequest::AddClient {
                hostname,
                port,
                ips,
                enter_hook,
            } => {
                let handle = clients.add(&fx, hostname, port, ips, enter_hook).await?;
                clients.reply(handle)
            }
            FrontendRequest::RemoveClient { id } => {
                clients.remove(&fx, id).await?;
                Reply::Done
            }
            FrontendRequest::Activate { id } => {
                clients.activate(&fx, id).await?;
                clients.reply(id)
            }
            FrontendRequest::Deactivate { id } => {
                clients.edit(&fx, id, |c| c.active = false).await?;
                clients.reply(id)
            }
            FrontendRequest::List => Reply::Clients(clients.infos()),
            FrontendRequest::Config => Reply::Config(clients.config().report()),
            FrontendRequest::SetHost { id, hostname } => {
                clients.edit(&fx, id, |c| c.hostname = hostname).await?;
                clients.reply(id)
            }
            FrontendRequest::SetPort { id, port } => {
                clients.edit(&fx, id, |c| c.port = port).await?;
                clients.reply(id)
            }
            FrontendRequest::SetPosition { id, position } => {
                clients.edit(&fx, id, |c| c.pos = position).await?;
                clients.reply(id)
            }
            FrontendRequest::SetIps { id, ips } => {
                clients.set_ips(&fx, id, ips).await?;
                clients.reply(id)
            }
            FrontendRequest::SetEnterHook { id, enter_hook } => {
                clients.edit(&fx, id, |c| c.enter_hook = enter_hook).await?;
                clients.reply(id)
            }
            // the port in use: nothing to rebind, but the file may hold
            // another (a `--port` flag, an edit awaiting a restart)
            FrontendRequest::ChangePort { port } => {
                clients.save_port(port).await?;
                Reply::Done
            }
            // focus is handled in `handle_ipc_event`
            FrontendRequest::Focus { .. } => Reply::Done,
            FrontendRequest::EnableCapture => {
                self.capture.reenable();
                Reply::Done
            }
            FrontendRequest::EnableEmulation => {
                self.emulation.reenable();
                Reply::Done
            }
            FrontendRequest::AuthorizeKey {
                description,
                fingerprint,
            } => {
                clients.authorize_key(description, fingerprint).await?;
                Reply::Done
            }
            FrontendRequest::RemoveAuthorizedKey { fingerprint } => {
                clients.remove_key(fingerprint).await?;
                Reply::Done
            }
            FrontendRequest::SaveConfig => {
                clients.save().await?;
                Reply::Done
            }
            FrontendRequest::Pair {
                host,
                ips,
                port,
                position,
            } => {
                let target = pair::Target {
                    host,
                    ips,
                    port,
                    position,
                };
                self.pairing.start(target, self.port)?;
                Reply::Done
            }
            FrontendRequest::PairAccept { timeout } => {
                self.pairing.accept(Duration::from_secs(timeout))?;
                Reply::Done
            }
            FrontendRequest::PairConfirm { code } => {
                self.pairing.confirm(&code)?;
                Reply::Done
            }
            FrontendRequest::PairCancel => {
                self.pairing.cancel()?;
                Reply::Done
            }
            FrontendRequest::TelemetryOn => {
                // fails, saying why, if there is no endpoint to send to
                telemetry::endpoint(clients.config().telemetry_endpoint())?;
                clients.set_telemetry(true).await?;
                if let Err(e) = self.telemetry.apply(Settings::of(clients.config())).await {
                    if let Err(revert) = clients.set_telemetry(false).await {
                        log::warn!("telemetry: {revert}");
                    }
                    return Err(e);
                }
                Reply::Telemetry(self.telemetry.status())
            }
            FrontendRequest::TelemetryOff => {
                clients.set_telemetry(false).await?;
                self.telemetry.apply(Settings::of(clients.config())).await?;
                Reply::Telemetry(self.telemetry.status())
            }
            FrontendRequest::TelemetryStatus => Reply::Telemetry(self.telemetry.status()),
            FrontendRequest::TelemetryShow => Reply::TelemetryShow(self.telemetry.show()),
            // counted in `handle_ipc_event`
            FrontendRequest::CountCommand { .. } => Reply::Done,
            FrontendRequest::Agent { .. }
            | FrontendRequest::AgentTargets
            | FrontendRequest::AgentGrant { .. } => {
                return Err("agent control is being built".into());
            }
        };
        Ok(reply)
    }

    fn reply(&self, conn: ConnId, seq: u64, result: Result<Reply, String>) {
        self.frontend
            .send(Some(conn), FrontendEvent::Reply { seq, result });
    }

    /// saves fingerprints pinned on first use
    async fn save_pins(&mut self) {
        self.clients.save_pins().await;
        self.reload_if_merged();
    }

    /// a save merged changes from the file the runtime lacks: apply them
    fn reload_if_merged(&mut self) {
        if self.clients.take_reload() {
            self.handle_config_change();
        }
    }

    fn handle_config_change(&mut self) {
        self.clients.reload(&Live {
            capture: &self.capture,
            clipboard: &self.clipboard,
        });
        let config = self.clients.config();
        self.capture.set_release_bind(config.release_bind());
        self.notifier.set_switch_enabled(config.notify_switch());
        self.inhibit.set_enabled(config.idle_inhibit());
        self.telemetry.configure(Settings::of(config));
        telemetry::COUNTERS.set_peers(config.clients().len());
        let pending = config.awaiting_restart();
        if !pending.is_empty() {
            let keys: Vec<_> = pending.iter().map(|p| p.key.as_str()).collect();
            log::warn!(
                "{:?}: changes to {} take effect after a restart of the service (`mimouse cli config` compares them)",
                config.config_path(),
                keys.join(", ")
            );
        }
        for event in self.sync_events() {
            self.notify_frontend(event);
        }
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some((to, event)) = self.frontend.pop() {
            self.frontend_listener.send(to, &event).await;
        }
    }

    fn handle_clipboard_event(&mut self, event: ClipboardEvent) {
        self.notify_frontend(match event {
            ClipboardEvent::Transfer(t) => FrontendEvent::Clipboard {
                direction: t.direction,
                peer: t.peer,
                mime: t.mime,
                size: t.size,
            },
            ClipboardEvent::Status(status) => FrontendEvent::ClipboardStatus(status),
        });
    }

    async fn handle_pair_event(&mut self, event: PairEvent) {
        let event = match event {
            PairEvent::Code {
                code,
                peer,
                fingerprint,
                position,
            } => {
                log::info!("pairing with {peer}: waiting for both users to confirm the code");
                FrontendEvent::PairCode {
                    code,
                    peer,
                    fingerprint,
                    position,
                }
            }
            PairEvent::Trusted(trusted) => {
                let fx = Live {
                    capture: &self.capture,
                    clipboard: &self.clipboard,
                };
                self.clients.paired(&fx, trusted).await
            }
            PairEvent::Failed(reason) => {
                log::warn!("pairing failed: {reason}");
                FrontendEvent::PairFailed { reason }
            }
        };
        self.notify_frontend(event);
        self.reload_if_merged();
        telemetry::COUNTERS.set_peers(self.clients.infos().len());
    }

    async fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                log::warn!(
                    "connection attempt from unauthorized fingerprint {fingerprint}; approve with `mimouse cli authorize-key <name> {fingerprint}`"
                );
                self.notifier.unauthorized(&fingerprint);
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => self.entered(addr, pos, fingerprint),
            EmulationEvent::Disconnected { addr } => {
                if let Some(incoming) = self.incoming.remove(&addr) {
                    self.capture.destroy(incoming.handle);
                    self.notify_frontend(FrontendEvent::Disconnected { addr });
                }
            }
            EmulationEvent::PortChanged(port) => {
                let result = match port {
                    Ok(port) => {
                        self.port = port;
                        self.clipboard.port_changed(port);
                        self.notify_frontend(FrontendEvent::Port { port, error: None });
                        if let Err(e) = self.clients.save_port(port).await {
                            log::warn!(
                                "listening on port {port}, but the config was not saved ({e}); fix it, then run `mimouse cli save-config`"
                            );
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let error = format!("{e}");
                        self.notify_frontend(FrontendEvent::Port {
                            port: self.port,
                            error: Some(error.clone()),
                        });
                        Err(error)
                    }
                };
                for (conn, seq, peer, request) in std::mem::take(&mut self.port_requests) {
                    self.audit.record(&peer, &request, &result);
                    self.reply(conn, seq, result.clone().map(|_| Reply::Done));
                }
                self.reload_if_merged();
            }
            EmulationEvent::EmulationDisabled => {
                crate::telemetry::COUNTERS.emulation_disables.bump();
                crate::telemetry::COUNTERS.set_emulation(None);
                self.emulation_status = Status::Disabled;
                self.emulation_backend = None;
                self.notify_frontend(self.emulation_event());
            }
            EmulationEvent::EmulationEnabled(backend) => {
                self.emulation_status = Status::Enabled;
                self.emulation_backend = Some(backend);
                crate::telemetry::COUNTERS.set_emulation(self.emulation_backend.as_ref());
                self.notify_frontend(self.emulation_event());
            }
            EmulationEvent::AwaitingApproval(awaiting) => {
                self.emulation_awaiting = awaiting;
                if awaiting {
                    self.notifier.awaiting_approval("input emulation");
                }
                self.notify_frontend(self.emulation_event());
            }
            EmulationEvent::ReleaseNotify => {
                // a peer took control of this machine
                self.capture.release();
                self.control_switched(None);
            }
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::Connection { addr, fingerprint });
            }
            EmulationEvent::PeerInput => {}
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                let peer = self.incoming.iter().find(|(_, i)| i.handle == handle);
                if let Some((addr, _)) = peer {
                    self.emulation.send_leave_event(*addr);
                }
            }
            ICaptureEvent::PushPointer { pos, range } => {
                let (edge, fraction) = push_target(pos, range);
                self.emulation.push_pointer(edge, fraction);
            }
            ICaptureEvent::FocusResult(handle, result) => {
                let (done, waiting) = std::mem::take(&mut self.focus_requests)
                    .into_iter()
                    .partition(|r| r.4 == handle);
                self.focus_requests = waiting;
                for (conn, seq, peer, request, _) in done {
                    self.audit.record(&peer, &request, &result);
                    let reply = result.clone().map(|()| self.clients.reply(handle));
                    self.reply(conn, seq, reply);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                crate::telemetry::COUNTERS.capture_disables.bump();
                crate::telemetry::COUNTERS.set_capture(None);
                self.capture_status = Status::Disabled;
                self.capture_backend = None;
                self.notify_frontend(self.capture_event());
                self.control_switched(None);
            }
            ICaptureEvent::CaptureEnabled(backend) => {
                self.capture_status = Status::Enabled;
                self.capture_backend = backend;
                crate::telemetry::COUNTERS.set_capture(self.capture_backend.as_ref());
                self.notify_frontend(self.capture_event());
            }
            ICaptureEvent::AwaitingApproval(awaiting) => {
                self.capture_awaiting = awaiting;
                if awaiting {
                    self.notifier.awaiting_approval("input capture");
                }
                self.notify_frontend(self.capture_event());
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.clipboard.client_entered(handle);
                self.spawn_hook_command(handle);
                self.control_switched(Some(handle));
            }
            ICaptureEvent::Released => self.control_switched(None),
        }
    }

    /// full state, sent to each new frontend connection
    fn sync_events(&self) -> Vec<FrontendEvent> {
        vec![
            FrontendEvent::Clients {
                clients: self.clients.infos(),
            },
            self.emulation_event(),
            self.capture_event(),
            FrontendEvent::ClipboardStatus(self.clipboard.status()),
            FrontendEvent::Port {
                port: self.port,
                error: None,
            },
            FrontendEvent::Fingerprint {
                fingerprint: self.public_key_fingerprint.clone(),
            },
            FrontendEvent::AuthorizedKeys {
                keys: self.clients.authorized_keys(),
            },
        ]
    }

    fn capture_event(&self) -> FrontendEvent {
        let status = reported_status(self.capture_status, self.capture_awaiting);
        FrontendEvent::Capture {
            enabled: status.into(),
            status,
            backend: self.capture_backend.clone(),
        }
    }

    fn emulation_event(&self) -> FrontendEvent {
        let status = reported_status(self.emulation_status, self.emulation_awaiting);
        FrontendEvent::Emulation {
            enabled: status.into(),
            status,
            backend: self.emulation_backend.clone(),
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    /// a peer took control from `addr`: place (or move) the barrier that
    /// gives it back
    fn entered(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let known = self.incoming.get(&addr);
        if known.is_some_and(|i| i.pos == pos && i.fingerprint == fingerprint) {
            return;
        }
        if let Some(incoming) = self.incoming.remove(&addr) {
            self.capture.destroy(incoming.handle);
            self.notify_frontend(FrontendEvent::Disconnected { addr });
        }
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture
            .create(handle, pos, CaptureType::EnterOnly, None);
        self.incoming.insert(
            addr,
            Incoming {
                handle,
                fingerprint: fingerprint.clone(),
                pos,
            },
        );
        self.notify_frontend(FrontendEvent::Entered {
            fingerprint,
            addr,
            position: pos,
        });
    }

    fn notify_frontend(&self, event: FrontendEvent) {
        self.frontend.broadcast(event);
    }

    /// control moved to the remote client `to`, or back to this machine (`None`)
    fn control_switched(&mut self, to: Option<ClientHandle>) {
        let host = to.map(|h| {
            self.clients
                .hostname(h)
                .unwrap_or_else(|| format!("client {h}"))
        });
        self.notifier.switched(host.as_deref());
        self.inhibit.set(to.is_some());
    }

    /// why control cannot move to `handle`, if it cannot. Capture reads
    /// requests only once enabled, so a focus forwarded while the portal
    /// dialog waits would never be answered.
    fn focus_error(&self, handle: ClientHandle) -> Option<String> {
        match self.clients.is_active(handle) {
            None => Some(format!("no such client: {handle}")),
            Some(false) => Some(format!("client {handle} is not active")),
            Some(true) if self.capture_awaiting => {
                Some("input capture is waiting for permission: approve the system dialog".into())
            }
            Some(true) if self.capture_status != Status::Enabled => {
                Some("input capture is disabled".into())
            }
            Some(true) => None,
        }
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.clients.enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }
}

/// where to push the local pointer to cross the barrier at `pos`: that edge
/// of this desktop, in the middle of `range` (`None`: the whole edge)
fn push_target(pos: input_capture::Position, range: Option<EdgeRange>) -> (Edge, u16) {
    let edge = match pos {
        input_capture::Position::Left => Edge::Left,
        input_capture::Position::Right => Edge::Right,
        input_capture::Position::Top => Edge::Top,
        input_capture::Position::Bottom => Edge::Bottom,
    };
    let (start, end) = range.map_or((0, 100), |r| (r.start(), r.end()));
    // percent sum / 200 = the middle as a fraction of the edge
    let fraction = (u32::from(start) + u32::from(end)) * u32::from(u16::MAX) / 200;
    (edge, u16::try_from(fraction).unwrap_or(u16::MAX))
}

/// the signals that stop the service: CTRL+C, and what the system sends to
/// end it: SIGTERM and SIGHUP on Unix; on Windows a closed console
/// (CTRL_CLOSE), and logoff and shutdown as WM_QUERYENDSESSION /
/// WM_ENDSESSION (see `end_session`). Each ends the run loop, so the
/// controlled peer is left and held keys are released
struct Shutdown {
    #[cfg(unix)]
    terminate: signal::unix::Signal,
    #[cfg(unix)]
    hangup: signal::unix::Signal,
    #[cfg(windows)]
    close: signal::windows::CtrlClose,
    #[cfg(windows)]
    end_session: end_session::EndSession,
}

impl Shutdown {
    fn new() -> io::Result<Self> {
        #[cfg(unix)]
        use signal::unix::{SignalKind, signal as listen};
        Ok(Self {
            #[cfg(unix)]
            terminate: listen(SignalKind::terminate())?,
            #[cfg(unix)]
            hangup: listen(SignalKind::hangup())?,
            #[cfg(windows)]
            close: signal::windows::ctrl_close()?,
            #[cfg(windows)]
            end_session: end_session::EndSession::start(),
        })
    }

    /// waits for the first of the signals, returning its name
    async fn recv(&mut self) -> io::Result<&'static str> {
        #[cfg(unix)]
        tokio::select! {
            r = signal::ctrl_c() => r.map(|()| "CTRL+C"),
            Some(()) = self.terminate.recv() => Ok("SIGTERM"),
            Some(()) = self.hangup.recv() => Ok("SIGHUP"),
        }
        #[cfg(windows)]
        tokio::select! {
            r = signal::ctrl_c() => r.map(|()| "CTRL+C"),
            Some(()) = self.close.recv() => Ok("CTRL_CLOSE"),
            Some(message) = self.end_session.recv() => Ok(message),
        }
    }

    /// the service has cleaned up: on Windows, a session end held for that
    /// goes on
    fn done(self) {}
}

/// logoff and shutdown on Windows. A console process that loads user32 or
/// gdi32 (mimouse does: SendInput, hooks, the tray icon) gets no
/// CTRL_LOGOFF_EVENT or CTRL_SHUTDOWN_EVENT (SetConsoleCtrlHandler docs);
/// it learns of them from WM_QUERYENDSESSION and WM_ENDSESSION sent to its
/// top-level windows, which a message-only window never gets. A hidden
/// top-level window of class `MiMouseEndSession`, on its own thread, turns
/// WM_QUERYENDSESSION and WM_ENDSESSION with wParam TRUE into a stop. Windows
/// may end the process once WM_ENDSESSION returns, so that waits until the
/// service has cleaned up, at most `WAIT`
#[cfg(windows)]
mod end_session {
    use std::{
        cell::OnceCell,
        sync::mpsc::{self, RecvTimeoutError},
        time::Duration,
    };
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG,
                RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION,
                WNDCLASSW,
            },
        },
        core::w,
    };

    /// how long WM_ENDSESSION holds the session end for the cleanup
    const WAIT: Duration = Duration::from_secs(2);

    /// the window thread's side
    struct Link {
        stop: UnboundedSender<&'static str>,
        /// disconnected once the service has cleaned up
        done: mpsc::Receiver<()>,
    }

    thread_local! {
        static LINK: OnceCell<Link> = const { OnceCell::new() };
    }

    /// the service's side; dropping it lets a held session end go on
    pub(super) struct EndSession {
        stop: UnboundedReceiver<&'static str>,
        _done: mpsc::Sender<()>,
    }

    impl EndSession {
        /// starts the window thread
        pub(super) fn start() -> Self {
            let (stop_tx, stop) = unbounded_channel();
            let (done_tx, done) = mpsc::channel();
            let link = Link {
                stop: stop_tx,
                done,
            };
            if let Err(e) = std::thread::Builder::new()
                .name("end-session".into())
                .spawn(move || run(link))
            {
                log::warn!("logoff and shutdown will not stop the service cleanly: {e}");
            }
            Self {
                stop,
                _done: done_tx,
            }
        }

        /// the message that ends the session
        pub(super) async fn recv(&mut self) -> Option<&'static str> {
            self.stop.recv().await
        }
    }

    fn run(link: Link) {
        LINK.with(|l| {
            let _ = l.set(link);
        });
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: w!("MiMouseEndSession"),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            let e = std::io::Error::last_os_error();
            log::warn!("logoff and shutdown will not stop the service cleanly: {e}");
            return;
        }
        // top-level (no parent) and never shown
        if let Err(e) = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MiMouseEndSession"),
                w!("MiMouse"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        } {
            log::warn!("logoff and shutdown will not stop the service cleanly: {e}");
            return;
        }
        let mut msg = MSG::default();
        // -1 is an error
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
            unsafe { DispatchMessageW(&msg) };
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_QUERYENDSESSION => {
                stop("WM_QUERYENDSESSION", false);
                // the session may end
                LRESULT(1)
            }
            WM_ENDSESSION => {
                // FALSE: the end was cancelled
                if wparam.0 != 0 {
                    stop("WM_ENDSESSION", true);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    /// asks the service to stop; `wait`: until it has cleaned up, at most
    /// `WAIT`
    fn stop(message: &'static str, wait: bool) {
        LINK.with(|link| {
            let Some(link) = link.get() else { return };
            // the service may have stopped already
            let _ = link.stop.send(message);
            if wait && matches!(link.done.recv_timeout(WAIT), Err(RecvTimeoutError::Timeout)) {
                log::warn!("{message}: the session ends before the service cleaned up");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Dirs;
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tokio::task::LocalSet;

    type Keys = Arc<RwLock<HashMap<String, String>>>;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mimouse-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// a service on dummy backends running the config file
    /// `dir/config.toml` that holds `toml`, and the authorized keys in
    /// effect; it writes nothing outside `dir`
    async fn service(dir: &Path, toml: &str) -> (Service, Keys) {
        let quiet = "capture_backend = \"dummy\"\nemulation_backend = \"dummy\"\nnotify_switch = false\nidle_inhibit = false\n";
        let path = dir.join("config.toml");
        fs::write(&path, format!("{quiet}{toml}")).unwrap();
        let config = Config::at(&path).unwrap();
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        let keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        let (listener, _) = MiMouseListener::on_free_port(cert.clone(), keys.clone()).await;
        let dirs = Dirs {
            cache: dir.to_owned(),
            config: dir.to_owned(),
        };
        let telemetry = Telemetry::for_test(&config, dirs);
        let frontend = AsyncFrontendListener::detached();
        let service =
            Service::assemble(config, cert, frontend, telemetry, keys.clone(), listener).unwrap();
        (service, keys)
    }

    /// `path` as an external editor leaves it, `f` applied
    fn edit(path: &Path, f: impl FnOnce(&mut toml_edit::DocumentMut)) {
        let mut doc: toml_edit::DocumentMut = fs::read_to_string(path).unwrap().parse().unwrap();
        f(&mut doc);
        fs::write(path, doc.to_string()).unwrap();
    }

    /// R-7: the save after a port change merges file edits the watcher has
    /// not reported yet; they are applied right away, not left for a
    /// reload that finds nothing changed
    #[tokio::test]
    async fn port_change_applies_the_file_edits_its_save_merged() {
        LocalSet::new()
            .run_until(async {
                let dir = temp_dir("service-port-merge");
                let path = dir.join("config.toml");
                let (mut service, keys) = service(&dir, "port = 47301\n").await;
                edit(&path, |doc| {
                    doc["authorized_fingerprints"]["aa:01"] = toml_edit::value("laptop");
                });
                let changed = EmulationEvent::PortChanged(Ok(47302));
                service.handle_emulation_event(changed).await;
                let saved = fs::read_to_string(&path).unwrap();
                assert!(saved.contains("port = 47302"), "{saved}");
                assert!(saved.contains("aa:01"), "{saved}");
                assert!(
                    keys.read().expect("lock").contains_key("aa:01"),
                    "merged key not in effect"
                );
                service.terminate().await;
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    /// R-9: `change-port` to the port in use saves it when the file holds
    /// another one (here an edit awaiting a restart), so the next start
    /// stays on it
    #[tokio::test]
    async fn change_port_to_the_port_in_use_is_saved() {
        LocalSet::new()
            .run_until(async {
                let dir = temp_dir("service-same-port");
                let path = dir.join("config.toml");
                let (mut service, _) = service(&dir, "port = 47311\n").await;
                edit(&path, |doc| doc["port"] = toml_edit::value(47312));
                let wait = Duration::from_secs(10);
                let change = tokio::time::timeout(wait, service.clients.changed()).await;
                assert!(matches!(change, Ok(Change::Config)), "edit not reloaded");
                service.handle_config_change();
                assert_eq!(service.clients.config().awaiting_restart().len(), 1);
                let request = FrontendRequest::ChangePort { port: 47311 };
                let event = IpcEvent::Request(0, Request { seq: 1, request });
                service.handle_ipc_event(event).await;
                let replied = std::iter::from_fn(|| service.frontend.pop()).any(|(_, e)| {
                    matches!(
                        e,
                        FrontendEvent::Reply {
                            seq: 1,
                            result: Ok(Reply::Done)
                        }
                    )
                });
                assert!(replied, "change-port failed");
                let saved = fs::read_to_string(&path).unwrap();
                assert!(saved.contains("port = 47311"), "{saved}");
                assert!(service.clients.config().awaiting_restart().is_empty());
                service.terminate().await;
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    #[test]
    fn pointer_is_pushed_through_the_middle_of_the_range() {
        use input_capture::Position;
        assert_eq!(push_target(Position::Left, None), (Edge::Left, 32767));
        assert_eq!(
            push_target(Position::Top, EdgeRange::new(0, 40)),
            (Edge::Top, 13107)
        );
        assert_eq!(
            push_target(Position::Bottom, EdgeRange::new(100 - 1, 100)),
            (Edge::Bottom, 65207)
        );
    }

    /// B6: SIGTERM and SIGHUP (systemd stop, closed terminal) end the run
    /// loop like CTRL+C instead of killing the process mid-switch
    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_and_sighup_stop_the_service() {
        let mut shutdown = Shutdown::new().unwrap();
        for (signal, name) in [("-TERM", "SIGTERM"), ("-HUP", "SIGHUP")] {
            let sent = std::process::Command::new("kill")
                .args([signal, &std::process::id().to_string()])
                .status()
                .unwrap();
            assert!(sent.success());
            let received = tokio::time::timeout(Duration::from_secs(10), shutdown.recv())
                .await
                .expect("signal not received");
            assert_eq!(received.unwrap(), name);
        }
    }
}

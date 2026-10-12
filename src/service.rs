use crate::ipc::{
    AsyncFrontendListener, ClientHandle, ClientInfo, ConnId, FrontendEvent, FrontendRequest,
    IpcEvent, IpcListenerCreationError, Peer, Position, Reply, Request, Status,
};
use crate::{
    approval::reported_status,
    audit::AuditLog,
    capture::{Capture, CaptureType, Hotkeys, ICaptureEvent},
    client::{Barrier, ClientManager, reuse_barriers},
    clipboard::{Clipboard, ClipboardEvent},
    config::{Config, ConfigClient},
    connect::{Connector, MiMouseConnection},
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    inhibit::IdleInhibit,
    listen::{ListenerCreationError, MiMouseListener},
    notify::Notifier,
};
use hickory_resolver::ResolveError;
use input_capture::EdgeRange;
use input_emulation::Edge;
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

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
    /// configuration
    config: Config,
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
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// keeps active clients connected
    connector: Connector,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending, to one connection or all
    pending_frontend_events: VecDeque<(Option<ConnId>, FrontendEvent)>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// the capture backend waits for the user to approve a permission request
    capture_awaiting: bool,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// the emulation backend waits for the user to approve a permission request
    emulation_awaiting: bool,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    /// desktop notifications (switches, unauthorized peers)
    notifier: Notifier,
    /// keeps this machine awake while it forwards input
    inhibit: IdleInhibit,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);
        log::info!("certificate fingerprint: {public_key_fingerprint}");

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection
        let listener =
            MiMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let conn = MiMouseConnection::new(cert.clone(), client_manager.clone());
        let connector = conn.connector();

        // after the already-running check, so a second instance never prompts
        #[cfg(target_os = "macos")]
        crate::macos_permissions::request_permissions();

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        let clipboard = Clipboard::new(
            config.clipboard(),
            config.port(),
            config.max_file_size(),
            &cert,
            authorized_keys.clone(),
            client_manager.clone(),
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let audit = AuditLog::new(config.config_dir());
        let notifier = Notifier::new(config.notify_switch());
        let inhibit = IdleInhibit::new(config.idle_inhibit());
        let service = Self {
            config,
            audit,
            port_requests: Default::default(),
            focus_requests: Default::default(),
            capture,
            emulation,
            clipboard,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            connector,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            capture_awaiting: false,
            emulation_status: Default::default(),
            emulation_awaiting: false,
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            notifier,
            inhibit,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }
        self.sync_hotkeys();

        loop {
            tokio::select! {
                event = self.frontend_listener.next() => self.handle_ipc_event(event),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.clipboard.event() => self.handle_clipboard_event(event),
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating clipboard ...");
        self.clipboard.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_ipc_event(&mut self, event: IpcEvent) {
        match event {
            IpcEvent::Connected(conn) => {
                for event in self.sync_events() {
                    self.pending_frontend_events.push_back((Some(conn), event));
                }
                self.frontend_event_pending.notify_one();
            }
            IpcEvent::Request(conn, Request { seq, request }) => {
                let peer = self
                    .frontend_listener
                    .peer(conn)
                    .cloned()
                    .unwrap_or_default();
                if let FrontendRequest::ChangePort { port } = request {
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
                            self.focus_requests.push((conn, seq, peer, request, id));
                        }
                        Some(e) => {
                            self.audit.record(&peer, &request, &Err(e.clone()));
                            self.reply(conn, seq, Err(e));
                        }
                    }
                    return;
                }
                let result = self.handle_request(&request);
                if !request.is_read_only() {
                    let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
                    self.audit.record(&peer, &request, &outcome);
                }
                self.reply(conn, seq, result);
            }
        }
    }

    /// apply a request; events it causes are queued before its reply
    fn handle_request(&mut self, request: &FrontendRequest) -> Result<Reply, String> {
        if let Some(handle) = request.client() {
            if self.client_manager.get_state(handle).is_none() {
                return Err(format!("no such client: {handle}"));
            }
        }
        let reply = match request.clone() {
            FrontendRequest::AddClient {
                hostname,
                port,
                ips,
                enter_hook,
            } => {
                let handle = self.add_client();
                self.update_hostname(handle, hostname);
                self.update_port(handle, port);
                self.update_fix_ips(handle, ips);
                self.update_enter_hook(handle, enter_hook);
                self.save_config()?;
                self.client_reply(handle)
            }
            FrontendRequest::RemoveClient { id } => {
                self.remove_client(id);
                self.save_config()?;
                Reply::Done
            }
            FrontendRequest::Activate { id } => {
                self.activate_client(id);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::Deactivate { id } => {
                self.deactivate_client(id);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::List => Reply::Clients(self.client_infos()),
            FrontendRequest::SetHost { id, hostname } => {
                self.update_hostname(id, hostname);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::SetPort { id, port } => {
                self.update_port(id, port);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::SetPosition { id, position } => {
                self.update_pos(id, position);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::SetIps { id, ips } => {
                self.update_fix_ips(id, ips);
                self.save_config()?;
                self.client_reply(id)
            }
            FrontendRequest::SetEnterHook { id, enter_hook } => {
                self.update_enter_hook(id, enter_hook);
                self.save_config()?;
                self.client_reply(id)
            }
            // a different port and focus are handled in `handle_ipc_event`
            FrontendRequest::ChangePort { .. } | FrontendRequest::Focus { .. } => Reply::Done,
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
                self.add_authorized_key(description, fingerprint);
                self.save_config()?;
                Reply::Done
            }
            FrontendRequest::RemoveAuthorizedKey { fingerprint } => {
                if !self.remove_authorized_key(&fingerprint) {
                    return Err(format!("no such authorized key: {fingerprint}"));
                }
                self.save_config()?;
                Reply::Done
            }
            FrontendRequest::SaveConfig => {
                self.save_config()?;
                Reply::Done
            }
        };
        Ok(reply)
    }

    fn reply(&mut self, conn: ConnId, seq: u64, result: Result<Reply, String>) {
        self.pending_frontend_events
            .push_back((Some(conn), FrontendEvent::Reply { seq, result }));
        self.frontend_event_pending.notify_one();
    }

    fn client_info(&self, handle: ClientHandle) -> Option<ClientInfo> {
        let (config, state) = self.client_manager.get_state(handle)?;
        Some(ClientInfo {
            id: handle,
            config,
            state,
        })
    }

    fn client_infos(&self) -> Vec<ClientInfo> {
        self.client_manager
            .get_client_states()
            .into_iter()
            .map(|(id, config, state)| ClientInfo { id, config, state })
            .collect()
    }

    fn client_reply(&self, handle: ClientHandle) -> Reply {
        self.client_info(handle)
            .map(Reply::Client)
            .unwrap_or(Reply::Done)
    }

    fn save_config(&mut self) -> Result<(), String> {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                range: c.range,
                hotkey: c.hotkey,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        self.config.write_back().map_err(|e| {
            log::warn!("failed to write config: {e}");
            format!("failed to write config: {e}")
        })
    }

    fn handle_config_change(&mut self) {
        let clients = self.config.clients();
        let running: Vec<_> = self
            .client_manager
            .get_client_states()
            .into_iter()
            .filter(|(_, _, s)| s.active)
            .map(|(h, c, _)| (h, Barrier::from(&c)))
            .collect();
        let configured: Vec<_> = clients
            .iter()
            .map(|c| c.active.then(|| Barrier::from(c)))
            .collect();
        match reuse_barriers(&running, &configured) {
            Some(reuse) => {
                log::info!("config changed, capture barriers unchanged");
                self.update_clients(clients, reuse);
            }
            None => self.replace_clients(clients),
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        self.sync_hotkeys();
        self.notifier
            .set_switch_enabled(self.config.notify_switch());
        self.inhibit.set_enabled(self.config.idle_inhibit());
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        for event in self.sync_events() {
            self.notify_frontend(event);
        }
    }

    /// applies reloaded clients without touching capture: active clients keep
    /// their handle and barrier (`reuse`), inactive ones are re-added
    fn update_clients(&mut self, clients: Vec<ConfigClient>, reuse: Vec<Option<ClientHandle>>) {
        let kept: HashSet<_> = reuse.iter().flatten().copied().collect();
        for h in self.client_manager.registered_clients() {
            if !kept.contains(&h) {
                self.remove_client(h);
            }
        }
        for (c, handle) in clients.into_iter().zip(reuse) {
            let Some(h) = handle else {
                let h = self.client_manager.add_with_config(c);
                log::info!("added client {h}");
                if let Some(client) = self.client_info(h) {
                    self.notify_frontend(FrontendEvent::Created { client });
                }
                continue;
            };
            self.client_manager
                .set_fix_ips(h, c.ips.into_iter().collect());
            if self.client_manager.set_hostname(h, c.hostname) {
                self.resolve(h);
            }
            self.client_manager.set_port(h, c.port);
            self.client_manager.set_enter_hook(h, c.enter_hook);
            self.client_manager.set_hotkey(h, c.hotkey);
            self.broadcast_client(h);
        }
    }

    /// re-creates every client, and with them the capture barriers
    fn replace_clients(&mut self, clients: Vec<ConfigClient>) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in clients {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (_, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            if let Some(client) = self.client_info(handle) {
                self.notify_frontend(FrontendEvent::Created { client });
            }
        }
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some((to, event)) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.send(to, &event).await;
        }
    }

    fn handle_clipboard_event(&mut self, event: ClipboardEvent) {
        self.notify_frontend(FrontendEvent::Clipboard {
            direction: event.direction,
            peer: event.peer,
            mime: event.mime,
            size: event.size,
        });
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
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
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::Entered {
                        fingerprint,
                        addr,
                        position: pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::Disconnected { addr });
                }
            }
            EmulationEvent::PortChanged(port) => {
                let result = match port {
                    Ok(port) => {
                        self.port = port;
                        self.clipboard.port_changed(port);
                        self.notify_frontend(FrontendEvent::Port { port, error: None });
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
            }
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(self.emulation_event());
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
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
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
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
                    let reply = result.clone().map(|()| self.client_reply(handle));
                    self.reply(conn, seq, reply);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(self.capture_event());
                self.control_switched(None);
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
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

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    /// full state, sent to each new frontend connection
    fn sync_events(&self) -> Vec<FrontendEvent> {
        vec![
            FrontendEvent::Clients {
                clients: self.client_infos(),
            },
            self.emulation_event(),
            self.capture_event(),
            FrontendEvent::Port {
                port: self.port,
                error: None,
            },
            FrontendEvent::Fingerprint {
                fingerprint: self.public_key_fingerprint.clone(),
            },
            FrontendEvent::AuthorizedKeys {
                keys: self.authorized_keys.read().expect("lock").clone(),
            },
        ]
    }

    fn capture_event(&self) -> FrontendEvent {
        let status = reported_status(self.capture_status, self.capture_awaiting);
        FrontendEvent::Capture {
            enabled: status.into(),
            status,
        }
    }

    fn emulation_event(&self) -> FrontendEvent {
        let status = reported_status(self.emulation_status, self.emulation_awaiting);
        FrontendEvent::Emulation {
            enabled: status.into(),
            status,
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture
            .create(handle, pos, CaptureType::EnterOnly, None);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::Disconnected { addr });
            self.notify_frontend(FrontendEvent::Entered {
                fingerprint,
                addr,
                position: pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back((None, event));
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedKeys { keys });
    }

    /// false if `fp` was not authorized
    fn remove_authorized_key(&mut self, fp: &str) -> bool {
        if self
            .authorized_keys
            .write()
            .expect("lock")
            .remove(fp)
            .is_none()
        {
            return false;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedKeys { keys });
        true
    }

    fn add_client(&mut self) -> ClientHandle {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        if let Some(client) = self.client_info(handle) {
            self.notify_frontend(FrontendEvent::Created { client });
        }
        handle
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.connector.stop(handle);
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            self.sync_hotkeys();
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            let range = self
                .client_manager
                .get_state(handle)
                .and_then(|(c, _)| c.range);
            self.capture
                .create(handle, pos, CaptureType::Default, range);
            self.connector.keep_connected(handle);
            self.broadcast_client(handle);
            self.sync_hotkeys();
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
            self.connector.stop(handle);
            self.sync_hotkeys();
        }
        self.notify_frontend(FrontendEvent::Deleted { id: handle });
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        if let Some(client) = self.client_info(handle) {
            self.notify_frontend(FrontendEvent::State { client });
        }
    }

    /// control moved to the remote client `to`, or back to this machine (`None`)
    fn control_switched(&mut self, to: Option<ClientHandle>) {
        let host = to.map(|h| {
            self.client_manager
                .get_hostname(h)
                .unwrap_or_else(|| format!("client {h}"))
        });
        self.notifier.switched(host.as_deref());
        self.inhibit.set(to.is_some());
    }

    /// why control cannot move to `handle`, if it cannot
    fn focus_error(&self, handle: ClientHandle) -> Option<String> {
        match self.client_manager.get_state(handle) {
            None => Some(format!("no such client: {handle}")),
            Some((_, s)) if !s.active => Some(format!("client {handle} is not active")),
            Some(_) => None,
        }
    }

    /// hands the hotkeys of the active clients and `local_hotkey` to capture
    fn sync_hotkeys(&self) {
        let clients = self
            .client_manager
            .get_client_states()
            .into_iter()
            .filter(|(_, _, s)| s.active)
            .filter_map(|(h, c, _)| Some((h, c.hotkey?)))
            .collect();
        self.capture.set_hotkeys(Hotkeys {
            local: self.config.local_hotkey(),
            clients,
        });
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

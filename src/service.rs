use crate::ipc::{
    AsyncFrontendListener, ClientConfig, ClientHandle, ClientInfo, ClientState, ConnId,
    FrontendEvent, FrontendRequest, IpcEvent, IpcListenerCreationError, Peer, Position, Reply,
    Request, Status,
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
    pair::{self, PairEvent, Pairing},
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
    time::Duration,
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
    /// pairing with other machines (`mimouse cli pair`)
    pairing: Pairing,
    /// a save merged changes from the file that the runtime has yet to apply
    config_reload: bool,
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
            pairing,
            config_reload: false,
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
                event = self.frontend_listener.next() => self.handle_ipc_event(event).await,
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.clipboard.event() => self.handle_clipboard_event(event),
                event = self.emulation.event() => self.handle_emulation_event(event).await,
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                event = self.pairing.event() => self.handle_pair_event(event).await,
                _ = self.config.changed() => self.handle_config_change(),
                _ = self.client_manager.pinned() => self.save_pins().await,
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

    async fn handle_ipc_event(&mut self, event: IpcEvent) {
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
                let result = self.handle_request(&request).await;
                if !request.is_read_only() {
                    let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
                    self.audit.record(&peer, &request, &outcome);
                }
                self.reply(conn, seq, result);
                self.reload_if_merged();
            }
        }
    }

    /// apply a request; events it causes are queued before its reply.
    /// Requests that change the config apply to the runtime only once it is
    /// saved, so a failed save changes nothing
    async fn handle_request(&mut self, request: &FrontendRequest) -> Result<Reply, String> {
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
                // the handle (and the client's place in the saved list) is
                // known only once added: rolled back if the save fails
                let handle = self.add_client();
                self.update_hostname(handle, hostname);
                self.update_port(handle, port);
                self.update_fix_ips(handle, ips);
                self.update_enter_hook(handle, enter_hook);
                if let Err(e) = self.save_config().await {
                    self.remove_client(handle);
                    return Err(e);
                }
                self.client_reply(handle)
            }
            FrontendRequest::RemoveClient { id } => {
                self.save_clients(|c| c.retain(|(h, _)| *h != id)).await?;
                self.remove_client(id);
                Reply::Done
            }
            FrontendRequest::Activate { id } => {
                self.save_clients(|c| plan_activate(c, id)).await?;
                self.activate_client(id);
                self.client_reply(id)
            }
            FrontendRequest::Deactivate { id } => {
                self.save_clients(|c| plan(c, id, |c| c.active = false))
                    .await?;
                self.deactivate_client(id);
                self.client_reply(id)
            }
            FrontendRequest::List => Reply::Clients(self.client_infos()),
            FrontendRequest::SetHost { id, hostname } => {
                let name = hostname.clone();
                self.save_clients(|c| plan(c, id, |c| c.hostname = name))
                    .await?;
                self.update_hostname(id, hostname);
                self.client_reply(id)
            }
            FrontendRequest::SetPort { id, port } => {
                self.save_clients(|c| plan(c, id, |c| c.port = port))
                    .await?;
                self.update_port(id, port);
                self.client_reply(id)
            }
            FrontendRequest::SetPosition { id, position } => {
                self.save_clients(|c| {
                    plan(c, id, |c| c.pos = position);
                    // `update_pos` re-activates an active client at its new place
                    if c.iter().any(|(h, c)| *h == id && c.active) {
                        plan_activate(c, id);
                    }
                })
                .await?;
                self.update_pos(id, position);
                self.client_reply(id)
            }
            FrontendRequest::SetIps { id, ips } => {
                let set = ips.iter().copied().collect();
                self.save_clients(|c| plan(c, id, |c| c.ips = set)).await?;
                self.update_fix_ips(id, ips);
                self.client_reply(id)
            }
            FrontendRequest::SetEnterHook { id, enter_hook } => {
                let hook = enter_hook.clone();
                self.save_clients(|c| plan(c, id, |c| c.enter_hook = hook))
                    .await?;
                self.update_enter_hook(id, enter_hook);
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
                let clients = self.config_clients();
                self.config_reload |=
                    change_keys(&mut self.config, &self.authorized_keys, clients, |keys| {
                        keys.insert(fingerprint, description);
                        Ok(())
                    })
                    .await?;
                self.notify_keys();
                Reply::Done
            }
            FrontendRequest::RemoveAuthorizedKey { fingerprint } => {
                let clients = self.config_clients();
                self.config_reload |=
                    change_keys(&mut self.config, &self.authorized_keys, clients, |keys| {
                        keys.remove(&fingerprint)
                            .map(|_| ())
                            .ok_or_else(|| format!("no such authorized key: {fingerprint}"))
                    })
                    .await?;
                self.notify_keys();
                Reply::Done
            }
            FrontendRequest::SaveConfig => {
                self.save_config().await?;
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
            .map(|info| Reply::Client(Box::new(info)))
            .unwrap_or(Reply::Done)
    }

    /// the clients as the config stores them, with their handles
    fn config_clients(&self) -> Vec<(ClientHandle, ConfigClient)> {
        self.client_manager
            .get_client_states()
            .into_iter()
            .map(|(h, c, s)| {
                let client = ConfigClient {
                    ips: HashSet::from_iter(c.fix_ips),
                    hostname: c.hostname,
                    port: c.port,
                    pos: c.pos,
                    active: s.active,
                    enter_hook: c.cmd,
                    range: c.range,
                    hotkey: c.hotkey,
                    fingerprint: c.fingerprint,
                };
                (h, client)
            })
            .collect()
    }

    /// saves the runtime state as it is
    async fn save_config(&mut self) -> Result<(), String> {
        self.save_clients(|_| {}).await
    }

    /// saves the config with `change` applied to the client list; the
    /// caller applies the same change to the runtime once this succeeded
    async fn save_clients(
        &mut self,
        change: impl FnOnce(&mut Vec<(ClientHandle, ConfigClient)>),
    ) -> Result<(), String> {
        let mut clients = self.config_clients();
        change(&mut clients);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.config_reload |= save(&mut self.config, clients, keys).await?;
        Ok(())
    }

    /// saves fingerprints pinned on first use
    async fn save_pins(&mut self) {
        if let Err(e) = self.save_config().await {
            log::warn!(
                "pinned certificate not saved ({e}); it is trusted until the service restarts"
            );
        }
        self.reload_if_merged();
    }

    /// a save merged changes from the file the runtime lacks: apply them
    fn reload_if_merged(&mut self) {
        if std::mem::take(&mut self.config_reload) {
            self.handle_config_change();
        }
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
            let before = self.connection_target(h);
            self.client_manager
                .set_fix_ips(h, c.ips.into_iter().collect());
            if self.client_manager.set_hostname(h, c.hostname) {
                self.resolve(h);
            }
            self.client_manager.set_port(h, c.port);
            self.reconnect_if_moved(h, before);
            if self.client_manager.set_fingerprint(h, c.fingerprint) {
                self.connector.reconnect(h);
            }
            self.client_manager.set_enter_hook(h, c.enter_hook);
            self.client_manager.set_hotkey(h, c.hotkey);
            self.broadcast_client(h);
        }
    }

    /// the configured fields a connection to `handle` is dialed from
    fn connection_target(
        &self,
        handle: ClientHandle,
    ) -> Option<(Vec<IpAddr>, Option<String>, u16)> {
        self.client_manager
            .get_state(handle)
            .map(|(c, _)| (c.fix_ips, c.hostname, c.port))
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
            PairEvent::Trusted(trusted) => self.paired(trusted).await,
            PairEvent::Failed(reason) => {
                log::warn!("pairing failed: {reason}");
                FrontendEvent::PairFailed { reason }
            }
        };
        self.notify_frontend(event);
        self.reload_if_merged();
    }

    /// trust a paired machine and configure it as a client pinned to its
    /// certificate (see [`paired_client`] for which client). The peer
    /// already trusts this machine, so a failed save keeps the pairing live
    /// and reports it unsaved
    async fn paired(&mut self, peer: pair::Trusted) -> FrontendEvent {
        log::info!(
            "paired with {} ({}), fingerprint {}",
            peer.name,
            peer.position,
            peer.fingerprint
        );
        self.add_authorized_key(peer.name.clone(), peer.fingerprint.clone());
        let existing = paired_client(&self.client_manager.get_client_states(), &peer);
        let handle = existing.unwrap_or_else(|| self.add_client());
        if self
            .client_manager
            .set_fingerprint(handle, Some(peer.fingerprint.clone()))
        {
            // a connection made before pairing trusted any server
            self.connector.reconnect(handle);
        }
        // the address pairing used first, then previously known ones
        let known = self
            .client_manager
            .get_state(handle)
            .map(|(config, _)| config.fix_ips)
            .unwrap_or_default();
        let mut ips = peer.ips;
        let known: Vec<IpAddr> = known.into_iter().filter(|ip| !ips.contains(ip)).collect();
        ips.extend(known);
        if peer.hostname.is_some() {
            self.update_hostname(handle, peer.hostname);
        }
        self.update_port(handle, peer.port);
        self.update_fix_ips(handle, ips);
        self.update_pos(handle, peer.position);
        self.activate_client(handle);
        let not_saved = self.save_config().await.err();
        if let Some(e) = &not_saved {
            log::warn!(
                "paired with {}, but the config was not saved ({e}); fix it, then run `mimouse cli save-config`",
                peer.name
            );
        }
        FrontendEvent::Paired {
            id: handle,
            peer: peer.name,
            fingerprint: peer.fingerprint,
            not_saved,
        }
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
                        self.config.set_port(port);
                        if let Err(e) = self.save_config().await {
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
                match ips {
                    // reconnect attempts re-resolve: a failed lookup keeps
                    // the last addresses (cleared when the host name changes)
                    Err(e) => log::warn!("could not resolve {hostname}: {e}"),
                    Ok(ips) => self.client_manager.set_dns_ips(handle, ips),
                }
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
        self.notify_keys();
    }

    fn notify_keys(&mut self) {
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedKeys { keys });
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
        let before = self.connection_target(handle);
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.reconnect_if_moved(handle, before);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
            self.connector.reconnect(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        let before = self.connection_target(handle);
        self.client_manager.set_port(handle, port);
        self.reconnect_if_moved(handle, before);
        self.broadcast_client(handle);
    }

    /// the open connection still goes to the old address
    fn reconnect_if_moved(
        &self,
        handle: ClientHandle,
        before: Option<(Vec<IpAddr>, Option<String>, u16)>,
    ) {
        if self.connection_target(handle) != before {
            self.connector.reconnect(handle);
        }
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

    /// why control cannot move to `handle`, if it cannot. Capture reads
    /// requests only once enabled, so a focus forwarded while the portal
    /// dialog waits would never be answered.
    fn focus_error(&self, handle: ClientHandle) -> Option<String> {
        match self.client_manager.get_state(handle) {
            None => Some(format!("no such client: {handle}")),
            Some((_, s)) if !s.active => Some(format!("client {handle} is not active")),
            Some(_) if self.capture_awaiting => {
                Some("input capture is waiting for permission: approve the system dialog".into())
            }
            Some(_) if self.capture_status != Status::Enabled => {
                Some("input capture is disabled".into())
            }
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

/// `Config::save` with the error logged and worded for the CLI
async fn save(
    config: &mut Config,
    clients: Vec<(ClientHandle, ConfigClient)>,
    keys: HashMap<String, String>,
) -> Result<bool, String> {
    let clients = clients.into_iter().map(|(_, c)| c).collect();
    config.save(clients, keys).await.map_err(|e| {
        log::warn!("failed to write config: {e}");
        format!("failed to write config: {e}")
    })
}

/// saves the authorized keys as `change` leaves them, then puts them in
/// effect: a failed save leaves `authorized` untouched
async fn change_keys(
    config: &mut Config,
    authorized: &RwLock<HashMap<String, String>>,
    clients: Vec<(ClientHandle, ConfigClient)>,
    change: impl FnOnce(&mut HashMap<String, String>) -> Result<(), String>,
) -> Result<bool, String> {
    let mut keys = authorized.read().expect("lock").clone();
    change(&mut keys)?;
    let reload = save(config, clients, keys.clone()).await?;
    *authorized.write().expect("lock") = keys;
    Ok(reload)
}

/// `change` applied to the planned config of client `id`
fn plan(
    clients: &mut [(ClientHandle, ConfigClient)],
    id: ClientHandle,
    change: impl FnOnce(&mut ConfigClient),
) {
    if let Some((_, c)) = clients.iter_mut().find(|(h, _)| *h == id) {
        change(c);
    }
}

/// `id` active, as `Service::activate_client` leaves it: another active
/// client at its position is deactivated
fn plan_activate(clients: &mut [(ClientHandle, ConfigClient)], id: ClientHandle) {
    let Some(pos) = clients.iter().find(|(h, _)| *h == id).map(|(_, c)| c.pos) else {
        return;
    };
    for (h, c) in clients {
        if *h == id {
            c.active = true;
        } else if c.pos == pos {
            c.active = false;
        }
    }
}

/// the client a paired `peer` configures: the one pinned to its certificate,
/// else an unpinned one with its host name or an address of it. A client
/// pinned to another certificate is another machine (same name, reused
/// address)
fn paired_client(
    clients: &[(ClientHandle, ClientConfig, ClientState)],
    peer: &pair::Trusted,
) -> Option<ClientHandle> {
    let pinned = clients
        .iter()
        .find(|(_, c, _)| c.fingerprint.as_ref() == Some(&peer.fingerprint));
    let same_machine = || {
        clients.iter().find(|(_, c, _)| {
            let same_host = matches!(
                (&c.hostname, &peer.hostname),
                (Some(a), Some(b)) if a.eq_ignore_ascii_case(b)
            );
            c.fingerprint.is_none()
                && (same_host || c.fix_ips.iter().any(|ip| peer.ips.contains(ip)))
        })
    };
    pinned.or_else(same_machine).map(|(h, _, _)| *h)
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

    fn client(pos: Position, active: bool) -> ConfigClient {
        ConfigClient {
            ips: HashSet::new(),
            hostname: None,
            port: crate::ipc::DEFAULT_PORT,
            pos,
            active,
            enter_hook: None,
            range: None,
            hotkey: None,
            fingerprint: None,
        }
    }

    #[test]
    fn activating_deactivates_the_other_client_at_that_position() {
        let mut clients = vec![
            (0, client(Position::Left, true)),
            (2, client(Position::Left, false)),
            (3, client(Position::Right, true)),
        ];
        plan_activate(&mut clients, 2);
        let active: Vec<_> = clients.iter().map(|(_, c)| c.active).collect();
        assert_eq!(active, [false, true, true]);
    }

    /// B4: pairing configures the client pinned to the peer's certificate;
    /// a same-named or same-address client counts only while unpinned
    #[test]
    fn paired_client_matches_fingerprint_first() {
        let ip: IpAddr = "192.168.0.7".parse().unwrap();
        let config =
            |hostname: &str, fix_ips: Vec<IpAddr>, fingerprint: Option<&str>| ClientConfig {
                hostname: Some(hostname.into()),
                fix_ips,
                fingerprint: fingerprint.map(Into::into),
                ..Default::default()
            };
        let peer = |name: &str, fingerprint: &str| pair::Trusted {
            name: name.into(),
            fingerprint: fingerprint.into(),
            hostname: Some(name.into()),
            ips: vec![ip],
            port: crate::ipc::DEFAULT_PORT,
            position: Position::Left,
        };
        let state = ClientState::default;
        let clients = vec![
            (0, config("ubuntu", vec![ip], Some("aa:01")), state()),
            (1, config("other", vec![], Some("bb:02")), state()),
        ];
        // pinned: by fingerprint, whatever the name or address
        assert_eq!(paired_client(&clients, &peer("ubuntu", "aa:01")), Some(0));
        assert_eq!(paired_client(&clients, &peer("renamed", "bb:02")), Some(1));
        // a second machine named `ubuntu` on a reused address is new
        assert_eq!(paired_client(&clients, &peer("ubuntu", "cc:03")), None);

        // an unpinned client (Lan Mouse peer, old config) by name or address
        let clients = vec![
            (0, config("ubuntu", vec![], None), state()),
            (1, config("lap", vec![ip], None), state()),
        ];
        assert_eq!(paired_client(&clients, &peer("UBUNTU", "cc:03")), Some(0));
        assert_eq!(paired_client(&clients, &peer("lap2", "cc:03")), Some(1));
    }

    /// B7: a failed save leaves the key untrusted; once the file is
    /// writable again the same request succeeds
    #[tokio::test]
    async fn authorize_key_applies_only_after_the_save() {
        let dir = std::env::temp_dir().join(format!("mimouse-authorize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "port = 4242\n").unwrap();
        let mut config = Config::at(&path).unwrap();
        let authorized = RwLock::new(config.authorized_fingerprints());
        let set_readonly = |readonly| {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(readonly);
            std::fs::set_permissions(&path, permissions).unwrap();
        };
        let authorize = |keys: &mut HashMap<String, String>| {
            keys.insert("aa:bb".into(), "laptop".into());
            Ok(())
        };

        set_readonly(true);
        let failed = change_keys(&mut config, &authorized, vec![], authorize).await;
        assert!(failed.unwrap_err().contains("read-only"));
        assert!(authorized.read().unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "port = 4242\n");

        set_readonly(false);
        change_keys(&mut config, &authorized, vec![], authorize)
            .await
            .unwrap();
        assert_eq!(authorized.read().unwrap()["aa:bb"], "laptop");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"aa:bb\" = \"laptop\""), "{saved}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

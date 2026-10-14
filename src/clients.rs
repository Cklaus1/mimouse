//! The configured clients and what follows from them.
//!
//! [`Clients`] owns the client list ([`ClientManager`]), the connections kept
//! to active clients ([`Connector`]), the capture barriers and hotkeys they
//! place (through [`Sinks`]), and the config file they are saved to. Every
//! change goes through [`Clients::apply`], which moves the runtime to a
//! planned client list, so a client is active ⇔ it places a barrier ⇔ it is
//! kept connected ⇔ its hotkey is armed. Requests are planned, saved, and
//! applied only once the save succeeded: a failed save changes nothing.
//!
//! Ids follow config order at startup; within a run a client keeps its id
//! across edits and reloads, and an id is never given to another client.

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    rc::Rc,
    sync::{Arc, RwLock},
};

use crate::{
    capture::Hotkeys,
    client::{Barrier, ClientManager, match_clients},
    config::{Config, ConfigClient},
    connect::Connector,
    dns::{DnsEvent, DnsRequester},
    ipc::{ClientConfig, ClientHandle, ClientInfo, ClientState, FrontendEvent, Reply},
    pair,
    service::FrontendQueue,
};

/// where client changes take effect outside the client list
pub(crate) trait Sinks {
    /// places the capture barrier of the active client `handle`
    fn create_barrier(&self, handle: ClientHandle, barrier: Barrier);
    fn destroy_barrier(&self, handle: ClientHandle);
    /// the hotkeys of the active clients and the local one
    fn set_hotkeys(&self, hotkeys: Hotkeys);
    /// `handle` is gone for good
    fn client_removed(&self, handle: ClientHandle);
}

/// the client list as the config stores it, with the handle of each running
/// client (`None`: to be added)
type Plan = Vec<(Option<ClientHandle>, ConfigClient)>;

/// what [`Clients::changed`] reports
pub(crate) enum Change {
    /// the file changed into a different valid config, now in effect in
    /// [`Clients::config`]; [`Clients::reload`] applies its clients
    Config,
    /// a connection pinned a client's certificate on first use
    Pinned,
}

pub(crate) struct Clients {
    manager: ClientManager,
    connector: Connector,
    resolver: DnsRequester,
    config: Config,
    /// authorized public key sha256 fingerprints, shared with the listeners
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    frontend: Rc<FrontendQueue>,
    /// the hotkeys capture has (`None`: none sent yet)
    hotkeys: Option<Hotkeys>,
    /// a save merged changes from the file that the runtime has yet to apply
    reload: bool,
}

impl Clients {
    /// `manager` and `connector` are shared with the connection, `resolver`
    /// answers through the service; no client exists until [`Clients::start`]
    pub(crate) fn new(
        config: Config,
        manager: ClientManager,
        connector: Connector,
        resolver: DnsRequester,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        frontend: Rc<FrontendQueue>,
    ) -> Self {
        Self {
            manager,
            connector,
            resolver,
            config,
            authorized_keys,
            frontend,
            hotkeys: None,
            reload: false,
        }
    }

    /// creates the configured clients, with ids in config order
    pub(crate) fn start(&mut self, fx: &impl Sinks) {
        let plan = self
            .config
            .clients()
            .into_iter()
            .map(|c| (None, c))
            .collect();
        self.apply(fx, plan);
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// waits until the config file changed or a certificate was pinned
    pub(crate) async fn changed(&mut self) -> Change {
        tokio::select! {
            _ = self.config.changed() => Change::Config,
            _ = self.manager.pinned() => Change::Pinned,
        }
    }

    /// whether a save merged changes from the file that [`Clients::reload`]
    /// has yet to apply
    pub(crate) fn take_reload(&mut self) -> bool {
        std::mem::take(&mut self.reload)
    }

    /// applies the clients and authorized keys of the config as loaded; a
    /// client matched to a running one keeps its handle (see
    /// [`match_clients`])
    pub(crate) fn reload(&mut self, fx: &impl Sinks) {
        let configured = self.config.clients();
        let handles = match_clients(&self.running(), &configured);
        self.apply(fx, handles.into_iter().zip(configured).collect());
        let keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .expect("lock")
            .clone_from(&keys);
    }

    pub(crate) fn exists(&self, handle: ClientHandle) -> bool {
        self.manager.get_pos(handle).is_some()
    }

    /// whether `handle` is active (`None`: no such client)
    pub(crate) fn is_active(&self, handle: ClientHandle) -> Option<bool> {
        self.manager.get_state(handle).map(|(_, s)| s.active)
    }

    pub(crate) fn hostname(&self, handle: ClientHandle) -> Option<String> {
        self.manager.get_hostname(handle)
    }

    pub(crate) fn enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.manager.get_enter_cmd(handle)
    }

    pub(crate) fn info(&self, handle: ClientHandle) -> Option<ClientInfo> {
        let (config, state) = self.manager.get_state(handle)?;
        Some(ClientInfo {
            id: handle,
            config,
            state,
        })
    }

    pub(crate) fn infos(&self) -> Vec<ClientInfo> {
        self.manager
            .get_client_states()
            .into_iter()
            .map(|(id, config, state)| ClientInfo { id, config, state })
            .collect()
    }

    pub(crate) fn reply(&self, handle: ClientHandle) -> Reply {
        self.info(handle)
            .map(|info| Reply::Client(Box::new(info)))
            .unwrap_or(Reply::Done)
    }

    pub(crate) fn authorized_keys(&self) -> HashMap<String, String> {
        self.authorized_keys.read().expect("lock").clone()
    }

    /// adds an inactive client
    pub(crate) async fn add(
        &mut self,
        fx: &impl Sinks,
        hostname: Option<String>,
        port: u16,
        ips: Vec<IpAddr>,
        enter_hook: Option<String>,
    ) -> Result<ClientHandle, String> {
        let client = ConfigClient {
            ips: ips.iter().copied().collect(),
            hostname,
            port,
            enter_hook,
            ..unconfigured()
        };
        let handles = self.change(fx, |plan| plan.push((None, client))).await?;
        let handle = *handles.last().expect("added");
        // in the order given
        self.manager.set_fix_ips(handle, ips);
        self.resolve(handle);
        Ok(handle)
    }

    pub(crate) async fn remove(&mut self, fx: &impl Sinks, id: ClientHandle) -> Result<(), String> {
        self.change(fx, |plan| plan.retain(|(h, _)| *h != Some(id)))
            .await
            .map(drop)
    }

    /// activates `id`, deactivating another active client at its position
    pub(crate) async fn activate(
        &mut self,
        fx: &impl Sinks,
        id: ClientHandle,
    ) -> Result<(), String> {
        let was_active = self.is_active(id) == Some(true);
        self.change(fx, |plan| {
            if let Some(i) = index(plan, id) {
                plan_activate(plan, i);
            }
        })
        .await?;
        // activating resolves, also an active client
        if was_active {
            self.resolve(id);
        }
        Ok(())
    }

    /// `change` applied to client `id`; an active client stays active,
    /// taking its (maybe new) position from another client
    pub(crate) async fn edit(
        &mut self,
        fx: &impl Sinks,
        id: ClientHandle,
        change: impl FnOnce(&mut ConfigClient),
    ) -> Result<(), String> {
        self.change(fx, |plan| {
            if let Some(i) = index(plan, id) {
                change(&mut plan[i].1);
                if plan[i].1.active {
                    plan_activate(plan, i);
                }
            }
        })
        .await
        .map(drop)
    }

    pub(crate) async fn set_ips(
        &mut self,
        fx: &impl Sinks,
        id: ClientHandle,
        ips: Vec<IpAddr>,
    ) -> Result<(), String> {
        let set = ips.iter().copied().collect();
        self.edit(fx, id, |c| c.ips = set).await?;
        // in the order given
        self.manager.set_fix_ips(id, ips);
        Ok(())
    }

    /// saves the runtime state as it is
    pub(crate) async fn save(&mut self) -> Result<(), String> {
        let clients = self.running().into_iter().map(|(_, c)| c).collect();
        let keys = self.authorized_keys();
        self.reload |= save(&mut self.config, clients, keys).await?;
        Ok(())
    }

    /// saves `telemetry = enabled`; a failed save changes nothing
    pub(crate) async fn set_telemetry(&mut self, enabled: bool) -> Result<(), String> {
        let clients = self.running().into_iter().map(|(_, c)| c).collect();
        let keys = self.authorized_keys();
        self.reload |= self
            .config
            .save_telemetry(enabled, clients, keys)
            .await
            .map_err(|e| {
                log::warn!("failed to write config: {e}");
                format!("failed to write config: {e}")
            })?;
        Ok(())
    }

    /// saves fingerprints pinned on first use
    pub(crate) async fn save_pins(&mut self) {
        if let Err(e) = self.save().await {
            log::warn!(
                "pinned certificate not saved ({e}); it is trusted until the service restarts"
            );
        }
    }

    /// saves the port the service now listens on
    pub(crate) async fn save_port(&mut self, port: u16) {
        self.config.set_port(port);
        if let Err(e) = self.save().await {
            log::warn!(
                "listening on port {port}, but the config was not saved ({e}); fix it, then run `mimouse cli save-config`"
            );
        }
    }

    pub(crate) async fn authorize_key(
        &mut self,
        description: String,
        fingerprint: String,
    ) -> Result<(), String> {
        self.change_keys(|keys| {
            keys.insert(fingerprint, description);
            Ok(())
        })
        .await
    }

    pub(crate) async fn remove_key(&mut self, fingerprint: String) -> Result<(), String> {
        self.change_keys(|keys| {
            keys.remove(&fingerprint)
                .map(|_| ())
                .ok_or_else(|| format!("no such authorized key: {fingerprint}"))
        })
        .await
    }

    async fn change_keys(
        &mut self,
        change: impl FnOnce(&mut HashMap<String, String>) -> Result<(), String>,
    ) -> Result<(), String> {
        let clients = self.running().into_iter().map(|(_, c)| c).collect();
        self.reload |=
            change_keys(&mut self.config, &self.authorized_keys, clients, change).await?;
        self.notify_keys();
        Ok(())
    }

    fn notify_keys(&self) {
        let keys = self.authorized_keys();
        self.frontend
            .broadcast(FrontendEvent::AuthorizedKeys { keys });
    }

    /// trust a paired machine and configure it as an active client pinned
    /// to its certificate (see [`paired_client`] for which client). The peer
    /// already trusts this machine, so this applies first and a failed save
    /// keeps the pairing live, reported unsaved
    pub(crate) async fn paired(&mut self, fx: &impl Sinks, peer: pair::Trusted) -> FrontendEvent {
        log::info!(
            "paired with {} ({}), fingerprint {}",
            peer.name,
            peer.position,
            peer.fingerprint
        );
        self.authorized_keys
            .write()
            .expect("lock")
            .insert(peer.fingerprint.clone(), peer.name.clone());
        self.notify_keys();
        let existing = paired_client(&self.manager.get_client_states(), &peer);
        // the address pairing used first, then previously known ones
        let known = existing
            .and_then(|h| self.manager.get_state(h))
            .map(|(config, _)| config.fix_ips)
            .unwrap_or_default();
        let mut ips = peer.ips;
        let known: Vec<IpAddr> = known.into_iter().filter(|ip| !ips.contains(ip)).collect();
        ips.extend(known);

        let mut plan = self.plan();
        let i = match existing.and_then(|h| index(&plan, h)) {
            Some(i) => i,
            None => {
                plan.push((None, unconfigured()));
                plan.len() - 1
            }
        };
        let c = &mut plan[i].1;
        c.fingerprint = Some(peer.fingerprint.clone());
        if peer.hostname.is_some() {
            c.hostname = peer.hostname;
        }
        c.port = peer.port;
        c.ips = ips.iter().copied().collect();
        c.pos = peer.position;
        plan_activate(&mut plan, i);
        let handle = self.apply(fx, plan)[i];
        self.manager.set_fix_ips(handle, ips);

        let not_saved = self.save().await.err();
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

    /// a DNS lookup progressed; a result for a host name the client no
    /// longer has is dropped
    pub(crate) fn resolved(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.manager.set_resolving(handle, false);
                match ips {
                    _ if self.manager.get_hostname(handle).as_ref() != Some(&hostname) => {
                        log::debug!("client {handle}: dropped the lookup of former name {hostname}")
                    }
                    // reconnect attempts re-resolve: a failed lookup keeps
                    // the last addresses (cleared when the host name changes)
                    Err(e) => log::warn!("could not resolve {hostname}: {e}"),
                    Ok(ips) => self.manager.set_dns_ips(handle, ips),
                }
                handle
            }
        };
        self.broadcast(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn broadcast(&self, handle: ClientHandle) {
        if let Some(client) = self.info(handle) {
            self.frontend.broadcast(FrontendEvent::State { client });
        }
    }

    /// the running clients as the config stores them
    fn running(&self) -> Vec<(ClientHandle, ConfigClient)> {
        self.manager
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

    fn plan(&self) -> Plan {
        self.running()
            .into_iter()
            .map(|(h, c)| (Some(h), c))
            .collect()
    }

    /// saves the clients as `change` leaves them, then applies that; a
    /// failed save changes nothing. Returns the handle of each client
    async fn change(
        &mut self,
        fx: &impl Sinks,
        change: impl FnOnce(&mut Plan),
    ) -> Result<Vec<ClientHandle>, String> {
        let mut plan = self.plan();
        change(&mut plan);
        let clients = plan.iter().map(|(_, c)| c.clone()).collect();
        let keys = self.authorized_keys();
        self.reload |= save(&mut self.config, clients, keys).await?;
        Ok(self.apply(fx, plan))
    }

    /// Moves the runtime to `plan`, in its order: clients missing from it
    /// are removed, new ones added, the others updated in place. Barriers,
    /// connections and hotkeys follow whether each client is active. One
    /// client is active per position: a later one wins, as activating them
    /// in order would. Returns the handle of each planned client
    fn apply(&mut self, fx: &impl Sinks, mut plan: Plan) -> Vec<ClientHandle> {
        let mut taken = HashSet::new();
        for (_, c) in plan.iter_mut().rev() {
            if c.active && !taken.insert(c.pos) {
                c.active = false;
            }
        }
        let before = self.manager.get_client_states();
        let previous = |h: ClientHandle| {
            before
                .iter()
                .find(|(b, _, _)| *b == h)
                .map(|(_, c, s)| (c, s.active))
        };
        let kept: HashSet<_> = plan.iter().filter_map(|(h, _)| *h).collect();
        for (h, _, s) in &before {
            if kept.contains(h) {
                continue;
            }
            if s.active {
                fx.destroy_barrier(*h);
                self.connector.stop(*h);
            }
            self.manager.remove_client(*h);
            fx.client_removed(*h);
            log::info!("removed client {h}");
            self.frontend.broadcast(FrontendEvent::Deleted { id: *h });
        }
        // barriers that go or move first: capture places one per position
        for (h, c) in &plan {
            let Some((h, (old, active))) = h.and_then(|h| Some((h, previous(h)?))) else {
                continue;
            };
            let was = active.then(|| Barrier::from(old));
            if was.is_some() && was != c.active.then(|| Barrier::from(c)) {
                fx.destroy_barrier(h);
            }
            if active && !c.active {
                self.manager.deactivate_client(h);
                self.connector.stop(h);
                log::info!("deactivated client {h}");
            }
        }
        let mut handles = Vec::with_capacity(plan.len());
        for (h, c) in plan {
            let barrier = c.active.then(|| Barrier::from(&c));
            let Some((h, (old, active))) = h.and_then(|h| Some((h, previous(h)?))) else {
                let h = self.manager.add_with_config(c);
                log::info!("added client {h}");
                if let Some(barrier) = barrier {
                    self.connect(fx, h, barrier);
                }
                if let Some(client) = self.info(h) {
                    self.frontend.broadcast(FrontendEvent::Created { client });
                }
                handles.push(h);
                continue;
            };
            self.update(h, old, c);
            match barrier {
                Some(barrier) if !active => {
                    self.manager.activate_client(h);
                    self.connect(fx, h, barrier);
                }
                // moved, the old one destroyed above
                Some(barrier) if barrier != Barrier::from(old) => {
                    fx.create_barrier(h, barrier);
                }
                _ => {}
            }
            if self.manager.get_state(h).map(|(c, s)| (c, s.active)) != Some((old.clone(), active))
            {
                self.broadcast(h);
            }
            handles.push(h);
        }
        self.manager.reorder(&handles);
        self.sync_hotkeys(fx);
        handles
    }

    /// the active client `handle` places its barrier and is kept connected
    fn connect(&self, fx: &impl Sinks, handle: ClientHandle, barrier: Barrier) {
        self.resolve(handle);
        fx.create_barrier(handle, barrier);
        self.connector.keep_connected(handle);
        log::info!("activated client {handle} ({})", barrier.pos);
    }

    /// the running client `handle`, configured as `old`, takes the fields
    /// of `c` (but not `active`); an open connection to an address it no
    /// longer has is closed
    fn update(&self, handle: ClientHandle, old: &ClientConfig, c: ConfigClient) {
        let mut moved = false;
        if old.fix_ips.iter().copied().collect::<HashSet<_>>() != c.ips {
            self.manager
                .set_fix_ips(handle, c.ips.into_iter().collect());
            moved = true;
        }
        if self.manager.set_hostname(handle, c.hostname) {
            self.resolve(handle);
            moved = true;
        }
        if old.port != c.port {
            self.manager.set_port(handle, c.port);
            moved = true;
        }
        // a connection made before trusted another server
        moved |= self.manager.set_fingerprint(handle, c.fingerprint);
        if moved {
            self.connector.reconnect(handle);
        }
        self.manager.set_pos(handle, c.pos);
        self.manager.set_range(handle, c.range);
        self.manager.set_enter_hook(handle, c.enter_hook);
        self.manager.set_hotkey(handle, c.hotkey);
    }

    /// hands the hotkeys of the active clients and the local one to capture
    fn sync_hotkeys(&mut self, fx: &impl Sinks) {
        let clients = self
            .manager
            .get_client_states()
            .into_iter()
            .filter(|(_, _, s)| s.active)
            .filter_map(|(h, c, _)| Some((h, c.hotkey?)))
            .collect();
        let hotkeys = Hotkeys {
            local: self.config.local_hotkey(),
            clients,
        };
        if self.hotkeys.as_ref() != Some(&hotkeys) {
            fx.set_hotkeys(hotkeys.clone());
            self.hotkeys = Some(hotkeys);
        }
    }
}

/// a new client as `add-client` and pairing start from
fn unconfigured() -> ConfigClient {
    let config = ClientConfig::default();
    ConfigClient {
        ips: HashSet::new(),
        hostname: config.hostname,
        port: config.port,
        pos: config.pos,
        active: false,
        enter_hook: config.cmd,
        range: config.range,
        hotkey: config.hotkey,
        fingerprint: config.fingerprint,
    }
}

/// where client `id` is in `plan`
fn index(plan: &[(Option<ClientHandle>, ConfigClient)], id: ClientHandle) -> Option<usize> {
    plan.iter().position(|(h, _)| *h == Some(id))
}

/// `Config::save` with the error logged and worded for the CLI
async fn save(
    config: &mut Config,
    clients: Vec<ConfigClient>,
    keys: HashMap<String, String>,
) -> Result<bool, String> {
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
    clients: Vec<ConfigClient>,
    change: impl FnOnce(&mut HashMap<String, String>) -> Result<(), String>,
) -> Result<bool, String> {
    let mut keys = authorized.read().expect("lock").clone();
    change(&mut keys)?;
    let reload = save(config, clients, keys.clone()).await?;
    *authorized.write().expect("lock") = keys;
    Ok(reload)
}

/// the planned client `i` active, as [`Clients::apply`] keeps it: another
/// active client at its position is deactivated
fn plan_activate(plan: &mut [(Option<ClientHandle>, ConfigClient)], i: usize) {
    let pos = plan[i].1.pos;
    for (j, (_, c)) in plan.iter_mut().enumerate() {
        if j == i {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::MiMouseConnection;
    use crate::ipc::{ClientState, Position};
    use std::{cell::RefCell, fs, path::PathBuf};
    use webrtc_dtls::crypto::Certificate;

    /// what the clients asked of capture and the clipboard
    #[derive(Default)]
    struct Recorded(RefCell<Vec<String>>);

    impl Recorded {
        fn take(&self) -> Vec<String> {
            self.0.take()
        }
    }

    impl Sinks for Recorded {
        fn create_barrier(&self, handle: ClientHandle, barrier: Barrier) {
            self.0
                .borrow_mut()
                .push(format!("create {handle} {}", barrier.pos));
        }

        fn destroy_barrier(&self, handle: ClientHandle) {
            self.0.borrow_mut().push(format!("destroy {handle}"));
        }

        fn set_hotkeys(&self, _: Hotkeys) {}

        fn client_removed(&self, handle: ClientHandle) {
            self.0.borrow_mut().push(format!("removed {handle}"));
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mimouse-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// the clients of the config file `dir/config.toml` holding `toml`
    fn clients(dir: &std::path::Path, toml: &str) -> Clients {
        let path = dir.join("config.toml");
        fs::write(&path, toml).unwrap();
        let config = Config::at(&path).unwrap();
        let manager = ClientManager::default();
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        let connector = MiMouseConnection::new(cert, manager.clone()).connector();
        let authorized = Arc::new(RwLock::new(config.authorized_fingerprints()));
        Clients::new(
            config,
            manager,
            connector,
            DnsRequester::detached(),
            authorized,
            Rc::default(),
        )
    }

    /// the file rewritten to `toml`, as the watcher loads it
    fn rewrite(clients: &mut Clients, dir: &std::path::Path, toml: &str) {
        let path = dir.join("config.toml");
        fs::write(&path, toml).unwrap();
        clients.config = Config::at(&path).unwrap();
    }

    fn ids(clients: &Clients) -> Vec<(ClientHandle, Option<String>)> {
        clients
            .infos()
            .into_iter()
            .map(|i| (i.id, i.config.hostname))
            .collect()
    }

    const TWO: &str = r#"
[[clients]]
hostname = "a"
position = "left"

[[clients]]
hostname = "b"
position = "right"
"#;

    /// B8: a removed client's handle is not given to the next one, and the
    /// clipboard is told the client is gone
    #[tokio::test]
    async fn removed_handle_is_not_reused() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = temp_dir("clients-reuse");
                let fx = Recorded::default();
                let mut clients = clients(&dir, TWO);
                clients.start(&fx);
                assert_eq!(
                    ids(&clients),
                    [(0, Some("a".into())), (1, Some("b".into()))]
                );
                clients.remove(&fx, 0).await.unwrap();
                assert_eq!(fx.take(), ["removed 0"]);
                let added = clients
                    .add(&fx, Some("c".into()), 4242, vec![], None)
                    .await
                    .unwrap();
                assert_eq!(added, 2);
                assert_eq!(
                    ids(&clients),
                    [(1, Some("b".into())), (2, Some("c".into()))]
                );
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    /// B8: a client still in the reloaded file keeps its id; its barrier
    /// is left alone while it stays where it was
    #[tokio::test]
    async fn reload_keeps_the_ids_of_kept_clients() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = temp_dir("clients-reload");
                let fx = Recorded::default();
                let active = TWO.replace(
                    "position = \"left\"",
                    "position = \"left\"\nactivate_on_startup = true",
                );
                let mut clients = clients(&dir, &active);
                clients.start(&fx);
                assert_eq!(fx.take(), ["create 0 left"]);

                // `b` edited and moved before `a`, `c` new, `a` given a hook
                let edited = r#"
[[clients]]
hostname = "b"
position = "right"
port = 4300

[[clients]]
hostname = "c"
position = "top"

[[clients]]
hostname = "a"
position = "left"
activate_on_startup = true
enter_hook = "true"
"#;
                rewrite(&mut clients, &dir, edited);
                clients.reload(&fx);
                assert_eq!(
                    ids(&clients),
                    [
                        (1, Some("b".into())),
                        (2, Some("c".into())),
                        (0, Some("a".into()))
                    ]
                );
                assert_eq!(fx.take(), Vec::<String>::new(), "capture touched");
                assert_eq!(clients.enter_cmd(0).as_deref(), Some("true"));

                // `a` gone: its barrier with it
                rewrite(
                    &mut clients,
                    &dir,
                    "[[clients]]\nhostname = \"b\"\nposition = \"right\"\n",
                );
                clients.reload(&fx);
                assert_eq!(fx.take(), ["removed 2", "destroy 0", "removed 0"]);
                assert_eq!(ids(&clients), [(1, Some("b".into()))]);
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    /// T2: `cli set-port <id> 0` and `cli add-client --port 0` would save an
    /// invalid file: refused, the file byte-identical, the runtime unchanged
    #[tokio::test]
    async fn change_to_an_invalid_config_is_refused() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = temp_dir("clients-invalid");
                let fx = Recorded::default();
                let mut clients = clients(&dir, TWO);
                clients.start(&fx);
                let e = clients.edit(&fx, 0, |c| c.port = 0).await.unwrap_err();
                assert!(e.contains("client a: `port` = 0"), "{e}");
                let e = clients.add(&fx, Some("c".into()), 0, vec![], None).await;
                assert!(e.is_err(), "{e:?}");
                assert_eq!(fs::read_to_string(dir.join("config.toml")).unwrap(), TWO);
                let ports: Vec<_> = clients.infos().into_iter().map(|i| i.config.port).collect();
                assert_eq!(ports, [4242, 4242]);
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    /// B8: a lookup started for the host name a client had before is
    /// dropped; one for its current name is applied
    #[tokio::test]
    async fn stale_dns_result_is_dropped() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = temp_dir("clients-dns");
                let fx = Recorded::default();
                let mut clients = clients(&dir, TWO);
                clients.start(&fx);
                clients
                    .edit(&fx, 0, |c| c.hostname = Some("renamed".into()))
                    .await
                    .unwrap();
                let stale: IpAddr = "192.168.0.9".parse().unwrap();
                clients.resolved(DnsEvent::Resolved(0, "a".into(), Ok(vec![stale])));
                assert!(!clients.manager.get_ips(0).unwrap().contains(&stale));
                let current: IpAddr = "192.168.0.10".parse().unwrap();
                clients.resolved(DnsEvent::Resolved(0, "renamed".into(), Ok(vec![current])));
                assert!(clients.manager.get_ips(0).unwrap().contains(&current));
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    /// B7: a failed save changes nothing; the same request succeeds once
    /// the file is writable again
    #[tokio::test]
    async fn removal_applies_only_after_the_save() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = temp_dir("clients-readonly");
                let fx = Recorded::default();
                let mut clients = clients(&dir, TWO);
                clients.start(&fx);
                let path = dir.join("config.toml");
                let set_readonly = |readonly| {
                    let mut permissions = fs::metadata(&path).unwrap().permissions();
                    permissions.set_readonly(readonly);
                    fs::set_permissions(&path, permissions).unwrap();
                };

                set_readonly(true);
                assert!(
                    clients
                        .remove(&fx, 0)
                        .await
                        .unwrap_err()
                        .contains("read-only")
                );
                assert_eq!(ids(&clients).len(), 2);
                assert_eq!(fx.take(), Vec::<String>::new());

                set_readonly(false);
                clients.remove(&fx, 0).await.unwrap();
                assert_eq!(ids(&clients), [(1, Some("b".into()))]);
                assert!(!fs::read_to_string(&path).unwrap().contains("\"a\""));
                fs::remove_dir_all(&dir).unwrap();
            })
            .await;
    }

    fn client(pos: Position, active: bool) -> ConfigClient {
        ConfigClient {
            pos,
            active,
            ..unconfigured()
        }
    }

    #[test]
    fn activating_deactivates_the_other_client_at_that_position() {
        let mut plan = vec![
            (Some(0), client(Position::Left, true)),
            (Some(2), client(Position::Left, false)),
            (Some(3), client(Position::Right, true)),
        ];
        plan_activate(&mut plan, 1);
        let active: Vec<_> = plan.iter().map(|(_, c)| c.active).collect();
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

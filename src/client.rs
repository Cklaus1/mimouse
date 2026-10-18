use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use input_event::scancode;
use tokio::sync::Notify;

use crate::crypto::peers::Policy;
use crate::ipc::{ClientConfig, ClientHandle, ClientState, Position};

use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Registry>>,
    /// notified when a fingerprint was pinned on first use
    pinned: Rc<Notify>,
}

#[derive(Default)]
struct Registry {
    /// in config order
    clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    /// the handle of the next client added: a handle is never reused within
    /// a run, so one held elsewhere (a DNS lookup, a `cli focus <id>`
    /// shortcut, the clipboard) never reaches another client
    next: ClientHandle,
}

impl ClientManager {
    /// `f` applied to the config and state of `handle`, if it exists
    fn with<R>(
        &self,
        handle: ClientHandle,
        f: impl FnOnce(&mut ClientConfig, &mut ClientState) -> R,
    ) -> Option<R> {
        let mut registry = self.clients.borrow_mut();
        let (_, c, s) = registry.clients.iter_mut().find(|(h, _, _)| *h == handle)?;
        Some(f(c, s))
    }

    /// `f` applied to the config and state of `handle`, if it exists
    fn read<R>(
        &self,
        handle: ClientHandle,
        f: impl FnOnce(&ClientConfig, &ClientState) -> R,
    ) -> Option<R> {
        let registry = self.clients.borrow();
        let (_, c, s) = registry.clients.iter().find(|(h, _, _)| *h == handle)?;
        Some(f(c, s))
    }

    /// appends a client configured as `config_client`
    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
            range: config_client.range,
            hotkey: config_client.hotkey,
            fingerprint: config_client.fingerprint,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        self.insert(config, state)
    }

    /// appends a new client with the default config
    pub fn add_client(&self) -> ClientHandle {
        self.insert(Default::default(), Default::default())
    }

    fn insert(&self, config: ClientConfig, state: ClientState) -> ClientHandle {
        let mut registry = self.clients.borrow_mut();
        let handle = registry.next;
        registry.next += 1;
        registry.clients.push((handle, config, state));
        handle
    }

    /// puts the clients in the order of `order` (config order); clients
    /// missing from it go last
    pub(crate) fn reorder(&self, order: &[ClientHandle]) {
        let rank = |h: &ClientHandle| order.iter().position(|o| o == h).unwrap_or(order.len());
        self.clients
            .borrow_mut()
            .clients
            .sort_by_key(|(h, _, _)| rank(h));
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        self.with(handle, |_, s| !std::mem::replace(&mut s.active, true))
            .unwrap_or(false)
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        self.with(handle, |_, s| std::mem::replace(&mut s.active, false))
            .unwrap_or(false)
    }

    /// get the client at the given position
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .clients
            .iter()
            .find(|(_, c, s)| s.active && c.pos == pos)
            .map(|(h, _, _)| *h)
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.read(handle, |c, _| c.hostname.clone()).flatten()
    }

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.read(handle, |c, _| c.pos)
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        let mut registry = self.clients.borrow_mut();
        let i = registry.clients.iter().position(|(h, _, _)| *h == client)?;
        let (_, c, s) = registry.clients.remove(i);
        Some((c, s))
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.read(handle, |c, s| (c.clone(), s.clone()))
    }

    /// where the client's TCP services (clipboard, agent) are reached: the
    /// address of its live connection first, then its other IPs
    pub(crate) fn addrs(&self, handle: ClientHandle) -> Option<Vec<SocketAddr>> {
        let (config, state) = self.get_state(handle)?;
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let Some(addr) = state.active_addr {
            addrs.push(SocketAddr::new(addr.ip(), config.port));
        }
        for ip in state.ips {
            let addr = SocketAddr::new(ip, config.port);
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        Some(addrs)
    }

    /// get the current config & state of all clients, in config order
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients.borrow().clients.clone()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.with(handle, |c, s| {
            c.fix_ips = fix_ips;
            update_ips(c, s);
        });
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        self.with(handle, |c, s| {
            s.dns_ips = dns_ips;
            update_ips(c, s);
        });
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        self.with(handle, |c, s| {
            if c.hostname == hostname {
                return false;
            }
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            update_ips(c, s);
            true
        })
        .unwrap_or(false)
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        self.with(handle, |c, s| {
            if c.port != port {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
        });
    }

    /// update the position of the client
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) {
        self.with(handle, |c, _| c.pos = pos);
    }

    /// update the part of the edge that switches to the client
    pub(crate) fn set_range(&self, handle: ClientHandle, range: Option<[u8; 2]>) {
        self.with(handle, |c, _| c.range = range);
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        self.with(handle, |c, _| c.cmd = enter_hook);
    }

    /// update the hotkey that moves control to the client
    pub(crate) fn set_hotkey(&self, handle: ClientHandle, hotkey: Option<Vec<scancode::Linux>>) {
        self.with(handle, |c, _| c.hotkey = hotkey);
    }

    /// set the certificate fingerprint outgoing connections to the client
    /// accept; true if it changed
    pub(crate) fn set_fingerprint(
        &self,
        handle: ClientHandle,
        fingerprint: Option<String>,
    ) -> bool {
        self.with(handle, |c, _| {
            let changed = c.fingerprint != fingerprint;
            c.fingerprint = fingerprint;
            changed
        })
        .unwrap_or(false)
    }

    /// the server certificates outgoing connections to the client accept:
    /// the pinned one, or any until one is pinned
    pub(crate) fn server_policy(&self, handle: ClientHandle) -> Option<Policy> {
        self.read(handle, |c, _| {
            c.fingerprint.clone().map_or(Policy::Any, Policy::Pinned)
        })
    }

    /// whether the client at `addr` presenting `fingerprint` is trusted:
    /// pins it if the client has no pin yet (trust on first use)
    pub(crate) fn pin(&self, handle: ClientHandle, addr: SocketAddr, fingerprint: &str) -> bool {
        self.with(handle, |c, _| match &c.fingerprint {
            Some(pinned) if pinned == fingerprint => true,
            Some(pinned) => {
                log::warn!(
                    "client {handle} @ {addr}: certificate {fingerprint} is not the pinned {pinned}, refused"
                );
                false
            }
            None => {
                log::info!(
                    "client {handle} @ {addr}: pinned certificate {fingerprint} on first use"
                );
                c.fingerprint = Some(fingerprint.to_owned());
                self.pinned.notify_one();
                true
            }
        })
        .unwrap_or(false)
    }

    /// waits until a connection pinned a client's fingerprint
    pub(crate) async fn pinned(&self) {
        self.pinned.notified().await
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        self.with(handle, |_, s| s.resolving = status);
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.read(handle, |c, _| c.cmd.clone()).flatten()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        self.with(handle, |_, s| s.active_addr = addr);
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        self.with(handle, |_, s| s.alive = alive);
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.read(handle, |_, s| s.active_addr).flatten()
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.read(handle, |_, s| s.alive).unwrap_or(false)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.read(handle, |c, _| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.read(handle, |_, s| s.ips.clone())
    }
}

/// the addresses connections to a client go to: the fixed ones and those
/// its host name resolved to
fn update_ips(c: &ClientConfig, s: &mut ClientState) {
    s.ips = c.fix_ips.iter().chain(s.dns_ips.iter()).copied().collect();
}

/// the capture barrier an active client places
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Barrier {
    pub(crate) pos: Position,
    /// the part of the edge it covers, in percent (`None`: all of it)
    pub(crate) range: Option<[u8; 2]>,
}

impl From<&ClientConfig> for Barrier {
    fn from(c: &ClientConfig) -> Self {
        Self {
            pos: c.pos,
            range: c.range,
        }
    }
}

impl From<&ConfigClient> for Barrier {
    fn from(c: &ConfigClient) -> Self {
        Self {
            pos: c.pos,
            range: c.range,
        }
    }
}

/// Pairs the clients of a reloaded config with the running ones, so a client
/// edited in the file keeps its handle (and, with its barrier unchanged,
/// capture is left alone: rebuilding it re-creates the portal session, which
/// asks the user again on GNOME 50). Returns, for each configured client,
/// the running client it continues (`None`: a new one).
///
/// Each running client continues at most one. Tried in this order, each
/// configured client taking the first unused match: an identical entry, the
/// same pinned certificate, the same host name, the same barrier place
/// (position and range), a shared fixed address. A client pinned to another
/// certificate is another machine and never matches.
pub(crate) fn match_clients(
    running: &[(ClientHandle, ConfigClient)],
    configured: &[ConfigClient],
) -> Vec<Option<ClientHandle>> {
    type Same = fn(&ConfigClient, &ConfigClient) -> bool;
    let tiers: [Same; 5] = [
        |a, b| a == b,
        |a, b| a.fingerprint.is_some() && a.fingerprint == b.fingerprint,
        |a, b| matches!((&a.hostname, &b.hostname), (Some(x), Some(y)) if x.eq_ignore_ascii_case(y)),
        |a, b| Barrier::from(a) == Barrier::from(b),
        |a, b| !a.ips.is_disjoint(&b.ips),
    ];
    let other_machine = |a: &ConfigClient, b: &ConfigClient| matches!((&a.fingerprint, &b.fingerprint), (Some(x), Some(y)) if x != y);
    let mut matched = vec![None; configured.len()];
    let mut used = vec![false; running.len()];
    for same in tiers {
        for (c, m) in configured.iter().zip(&mut matched) {
            if m.is_some() {
                continue;
            }
            let found = running
                .iter()
                .zip(&used)
                .position(|((_, r), used)| !used && !other_machine(r, c) && same(r, c));
            if let Some(i) = found {
                used[i] = true;
                *m = Some(running[i].0);
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(hostname: Option<&str>, pos: Position) -> ConfigClient {
        ConfigClient {
            ips: HashSet::new(),
            hostname: hostname.map(Into::into),
            port: crate::ipc::DEFAULT_PORT,
            pos,
            active: true,
            enter_hook: None,
            range: None,
            hotkey: None,
            fingerprint: None,
        }
    }

    /// B8: a removed client's handle is not given to the next one
    #[test]
    fn handles_are_never_reused() {
        let clients = ClientManager::default();
        let a = clients.add_client();
        let b = clients.add_client();
        clients.remove_client(a);
        let c = clients.add_client();
        assert!(c != a && c != b, "{c} reused ({a}, {b})");
        assert!(clients.get_state(a).is_none());
    }

    #[test]
    fn clients_are_kept_in_the_given_order() {
        let clients = ClientManager::default();
        let a = clients.add_client();
        let b = clients.add_client();
        let c = clients.add_client();
        clients.reorder(&[c, a]);
        let order: Vec<_> = clients
            .get_client_states()
            .into_iter()
            .map(|(h, _, _)| h)
            .collect();
        assert_eq!(order, [c, a, b]);
    }

    #[test]
    fn unchanged_clients_keep_their_handles_in_any_order() {
        let left = client(Some("left"), Position::Left);
        let top = client(Some("top"), Position::Top);
        let running = [(4, left.clone()), (7, top.clone())];
        let added = client(Some("new"), Position::Right);
        assert_eq!(
            match_clients(&running, &[top, added, left]),
            [Some(7), None, Some(4)]
        );
    }

    #[test]
    fn edited_clients_keep_their_handles() {
        let mut pinned = client(Some("lap"), Position::Left);
        pinned.fingerprint = Some("aa:01".into());
        let named = client(Some("desk"), Position::Right);
        let unnamed = client(None, Position::Top);
        let mut addressed = client(None, Position::Bottom);
        addressed.ips.insert("10.0.0.9".parse().unwrap());
        let running = [
            (1, pinned.clone()),
            (2, named.clone()),
            (3, unnamed),
            (5, addressed.clone()),
        ];
        // renamed and moved, but pinned to the same certificate
        let mut renamed = client(Some("lap2"), Position::Top);
        renamed.fingerprint = pinned.fingerprint.clone();
        // moved, same name
        let moved = ConfigClient {
            pos: Position::Bottom,
            ..named
        };
        // named and deactivated, same place
        let mut deactivated = client(Some("tv"), Position::Top);
        deactivated.active = false;
        // another position, same fixed address
        let readdressed = ConfigClient {
            pos: Position::Left,
            ..addressed
        };
        assert_eq!(
            match_clients(&running, &[readdressed, deactivated, moved, renamed]),
            [Some(5), Some(3), Some(2), Some(1)]
        );
    }

    #[test]
    fn another_certificate_is_another_machine() {
        let mut pinned = client(Some("ubuntu"), Position::Left);
        pinned.fingerprint = Some("aa:01".into());
        let mut other = pinned.clone();
        other.fingerprint = Some("bb:02".into());
        assert_eq!(match_clients(&[(0, pinned)], &[other]), [None]);
    }

    #[test]
    fn a_running_client_continues_one_configured_client() {
        let left = client(Some("a"), Position::Left);
        let running = [(0, left.clone())];
        assert_eq!(
            match_clients(&running, &[left.clone(), left]),
            [Some(0), None]
        );
    }
}

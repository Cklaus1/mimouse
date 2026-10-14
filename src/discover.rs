//! `mimouse cli discover`: MiMouse machines on the tailnet.
//!
//! Lists online peers from `tailscale status --json` that accept a TCP
//! connection on the MiMouse port (the TLS port, same number as the DTLS
//! input port). A LAN address is preferred over the Tailscale addresses when
//! one is known, either from a configured client or from Tailscale's direct
//! path (`CurAddr`). No mDNS: tailnet membership is the only discovery source
//! and never implies trust.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::TcpStream, time::timeout};

use crate::config::Config;
use crate::doctor::{COMMAND_TIMEOUT, CommandError, run_command};

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// operating systems that can run MiMouse (Tailscale's `OS` values)
const COMPUTER_OSES: [&str; 5] = ["linux", "windows", "macOS", "freebsd", "openbsd"];

#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error("tailscale is not installed (or not in PATH)")]
    NotInstalled,
    #[error("`tailscale status --json` failed: {0}")]
    Failed(String),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("unexpected `tailscale status --json` output: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Peer", default)]
    peer: Option<HashMap<String, Node>>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Option<Vec<IpAddr>>,
    /// `ip:port` of the direct path, empty when relayed or idle
    #[serde(rename = "CurAddr", default)]
    cur_addr: String,
}

/// an online tailnet peer that could run MiMouse
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TailnetPeer {
    /// the machine's host name
    pub name: String,
    /// MagicDNS name without the trailing dot
    pub dns_name: String,
    pub os: String,
    /// private address of the direct path, if Tailscale has one
    pub lan: Option<IpAddr>,
    pub tailscale_ips: Vec<IpAddr>,
}

/// a peer answering on the MiMouse port
#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    #[serde(flatten)]
    pub peer: TailnetPeer,
    /// the preferred address that answered
    pub address: IpAddr,
    /// `address` is a LAN address
    pub on_lan: bool,
    pub port: u16,
    /// all candidate addresses, preferred first
    pub addresses: Vec<IpAddr>,
}

/// online peers running a desktop OS, sorted by name; this machine is not
/// part of `Peer`
pub fn parse_status(json: &str) -> Result<Vec<TailnetPeer>, serde_json::Error> {
    let status: Status = serde_json::from_str(json)?;
    let mut peers = status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter(|n| n.online && COMPUTER_OSES.contains(&n.os.as_str()))
        .map(|n| TailnetPeer {
            lan: n
                .cur_addr
                .parse::<std::net::SocketAddr>()
                .ok()
                .map(|a| a.ip())
                .filter(is_lan),
            name: n.host_name,
            dns_name: n.dns_name.trim_end_matches('.').to_owned(),
            os: n.os,
            tailscale_ips: n.tailscale_ips.unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| (&a.name, &a.dns_name).cmp(&(&b.name, &b.dns_name)));
    Ok(peers)
}

/// a private address that is not Tailscale's own range
pub fn is_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() && !is_cgnat(ip),
        IpAddr::V6(ip) => is_unique_local(ip) && !is_tailscale_v6(ip),
    }
}

/// 100.64.0.0/10, which Tailscale uses for its addresses
fn is_cgnat(ip: &Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (b & 0xc0) == 64
}

fn is_unique_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fd7a:115c:a1e0::/48
fn is_tailscale_v6(ip: &Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

impl TailnetPeer {
    /// `name` is this peer's host name, MagicDNS name or its first label
    pub fn matches(&self, name: &str) -> bool {
        let host = name.trim_end_matches('.');
        self.dns_matches(name) || (!self.name.is_empty() && self.name.eq_ignore_ascii_case(host))
    }

    /// `name` is this peer's MagicDNS name or its first label: unique in a
    /// tailnet, unlike host names
    fn dns_matches(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.');
        let label = self.dns_name.split('.').next().unwrap_or_default();
        [self.dns_name.as_str(), label]
            .iter()
            .any(|n| !n.is_empty() && n.eq_ignore_ascii_case(name))
    }

    /// addresses to try, preferred first: LAN addresses known from the
    /// config, Tailscale's direct LAN path, then the Tailscale addresses
    pub fn candidates(&self, known: &[IpAddr]) -> Vec<IpAddr> {
        let mut addrs: Vec<IpAddr> = known.iter().copied().filter(is_lan).collect();
        addrs.extend(self.lan);
        addrs.extend(self.tailscale_ips.iter().copied());
        let mut seen = std::collections::HashSet::new();
        addrs.retain(|a| seen.insert(*a));
        addrs
    }
}

/// the peer called `name`: the one with that MagicDNS name, else the only
/// one with that host name; none if several share it (DNS decides then)
fn find_peer<'a>(peers: &'a [TailnetPeer], name: &str) -> Option<&'a TailnetPeer> {
    if let Some(p) = peers.iter().find(|p| p.dns_matches(name)) {
        return Some(p);
    }
    let mut by_host = peers.iter().filter(|p| p.matches(name));
    match (by_host.next(), by_host.next()) {
        (Some(p), None) => Some(p),
        _ => None,
    }
}

/// addresses of configured clients that refer to `peer`
fn known_addrs(config: &Config, peer: &TailnetPeer) -> Vec<IpAddr> {
    config
        .clients()
        .into_iter()
        .filter(|c| {
            c.hostname.as_deref().is_some_and(|h| peer.matches(h))
                || c.ips.iter().any(|ip| peer.tailscale_ips.contains(ip))
        })
        .flat_map(|c| c.ips)
        .collect()
}

/// output of `tailscale status --json`
async fn tailscale_status() -> Result<String, DiscoverError> {
    #[cfg(windows)]
    const BINARIES: &[&str] = &["tailscale", r"C:\Program Files\Tailscale\tailscale.exe"];
    #[cfg(target_os = "macos")]
    const BINARIES: &[&str] = &[
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    #[cfg(not(any(windows, target_os = "macos")))]
    const BINARIES: &[&str] = &["tailscale"];

    for bin in BINARIES {
        let output = match run_command(bin, &["status", "--json"], COMMAND_TIMEOUT).await {
            Ok(output) => output,
            Err(CommandError::Spawn { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DiscoverError::Failed(stderr.trim().to_owned()));
        }
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(DiscoverError::NotInstalled)
}

async fn answers(ip: IpAddr, port: u16) -> bool {
    matches!(
        timeout(PROBE_TIMEOUT, TcpStream::connect((ip, port))).await,
        Ok(Ok(_))
    )
}

/// a peer with its candidate addresses and which of them answered
struct Probed {
    peer: TailnetPeer,
    addresses: Vec<IpAddr>,
    answered: Vec<bool>,
}

async fn probe(config: &Config, peers: Vec<TailnetPeer>, port: u16) -> Vec<Probed> {
    join_all(peers.into_iter().map(|peer| async move {
        let addresses = peer.candidates(&known_addrs(config, &peer));
        let answered = join_all(addresses.iter().map(|&ip| answers(ip, port))).await;
        Probed {
            peer,
            addresses,
            answered,
        }
    }))
    .await
}

/// Peers that answered, at their preferred answering address. A LAN address
/// that several peers claim (WSL with mirrored networking reports the
/// Windows host's address, so do VMs behind the host) only counts for peers
/// that also answer on their own Tailscale address.
fn select(probed: Vec<Probed>, port: u16) -> Vec<Discovered> {
    let mut claims = HashMap::<IpAddr, usize>::new();
    for p in &probed {
        for ip in p.addresses.iter().filter(|ip| is_lan(ip)) {
            *claims.entry(*ip).or_default() += 1;
        }
    }
    probed
        .into_iter()
        .filter_map(|p| {
            let own = p
                .addresses
                .iter()
                .zip(&p.answered)
                .any(|(ip, ok)| *ok && p.peer.tailscale_ips.contains(ip));
            let address = p
                .addresses
                .iter()
                .zip(&p.answered)
                .filter(|(ip, ok)| **ok && (own || claims.get(*ip).is_none_or(|n| *n == 1)))
                .map(|(ip, _)| *ip)
                .next()?;
            Some(Discovered {
                on_lan: is_lan(&address),
                peer: p.peer,
                address,
                port,
                addresses: p.addresses,
            })
        })
        .collect()
}

/// online tailnet peers answering on the MiMouse port, probed concurrently
pub async fn discover(config: &Config, port: u16) -> Result<Vec<Discovered>, DiscoverError> {
    let peers = parse_status(&tailscale_status().await?)?;
    let found = select(probe(config, peers, port).await, port);
    if found.iter().all(|d| d.on_lan) {
        return Ok(found);
    }
    // an idle peer has no direct path until traffic flows; the probe just
    // sent some, so Tailscale may know LAN addresses now
    let peers = parse_status(&tailscale_status().await?)?;
    Ok(select(probe(config, peers, port).await, port))
}

/// candidate addresses of the online tailnet machine called `name`, preferred
/// first; empty without Tailscale or such a machine. LAN addresses several
/// peers claim are left out (see `select`): without probing, they may lead to
/// another machine.
pub async fn addresses(config: &Config, name: &str) -> Vec<IpAddr> {
    let Ok(status) = tailscale_status().await else {
        return vec![];
    };
    let Ok(peers) = parse_status(&status) else {
        return vec![];
    };
    let candidates: Vec<Vec<IpAddr>> = peers
        .iter()
        .map(|p| p.candidates(&known_addrs(config, p)))
        .collect();
    let mut claims = HashMap::<IpAddr, usize>::new();
    for ip in candidates.iter().flatten().filter(|ip| is_lan(ip)) {
        *claims.entry(*ip).or_default() += 1;
    }
    let Some(peer) = find_peer(&peers, name) else {
        return vec![];
    };
    let Some(mut addrs) = peers
        .iter()
        .zip(candidates)
        .find_map(|(p, c)| std::ptr::eq(p, peer).then_some(c))
    else {
        return vec![];
    };
    addrs.retain(|ip| claims.get(ip).is_none_or(|n| *n == 1));
    addrs
}

/// `mimouse cli discover`
pub async fn run(config: &Config, port: Option<u16>, json: bool) -> Result<(), DiscoverError> {
    let port = port.unwrap_or(config.port());
    let found = discover(config, port).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&found).expect("serializable peers")
        );
        return Ok(());
    }
    if found.is_empty() {
        eprintln!("no online tailnet machine answers on port {port}");
    }
    for d in found {
        let via = if d.on_lan { "lan" } else { "tailscale" };
        println!(
            "{}\t{} ({via})\t{}\t{}",
            d.peer.name, d.address, d.peer.os, d.peer.dns_name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/tailscale-status.json");

    #[test]
    fn lists_online_computers_only() {
        let peers = parse_status(FIXTURE).unwrap();
        let names: Vec<_> = peers
            .iter()
            .map(|p| (p.name.as_str(), p.os.as_str()))
            .collect();
        // offline machines (incl. the phone) and this machine are skipped
        assert_eq!(
            names,
            [
                ("evo-pc", "linux"),
                ("evo-pc", "windows"),
                ("megapc", "windows"),
                ("wsl-chris", "linux"),
            ]
        );
        let megapc = peers.iter().find(|p| p.name == "megapc").unwrap();
        assert_eq!(megapc.dns_name, "megapc.tail6765ef.ts.net");
        assert_eq!(megapc.lan, Some("192.168.50.144".parse().unwrap()));
        assert_eq!(
            megapc.tailscale_ips,
            [
                "100.81.228.80".parse::<IpAddr>().unwrap(),
                "fd7a:115c:a1e0::e539:e450".parse().unwrap()
            ]
        );
        // relayed peers have no direct path
        assert!(
            peers
                .iter()
                .filter(|p| p.name != "megapc")
                .all(|p| p.lan.is_none())
        );
    }

    #[test]
    fn skips_online_phones_and_tolerates_no_peers() {
        let json = r#"{"Self": {"HostName": "me", "OS": "linux", "Online": true},
            "Peer": {
              "a": {"HostName": "phone", "OS": "android", "Online": true, "TailscaleIPs": ["100.64.0.2"]},
              "b": {"HostName": "ipad", "OS": "iOS", "Online": true, "TailscaleIPs": ["100.64.0.3"]},
              "c": {"HostName": "mac", "OS": "macOS", "Online": true, "TailscaleIPs": ["100.64.0.4"],
                    "CurAddr": "100.70.0.1:41641"}
            }}"#;
        let peers = parse_status(json).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "mac");
        // a CGNAT CurAddr is not a LAN address
        assert_eq!(peers[0].lan, None);
        assert!(parse_status(r#"{"Peer": null}"#).unwrap().is_empty());
    }

    #[test]
    fn prefers_lan_addresses() {
        let peers = parse_status(FIXTURE).unwrap();
        let megapc = peers.iter().find(|p| p.name == "megapc").unwrap();
        let ts4: IpAddr = "100.81.228.80".parse().unwrap();
        let ts6: IpAddr = "fd7a:115c:a1e0::e539:e450".parse().unwrap();
        let cur: IpAddr = "192.168.50.144".parse().unwrap();
        let wired: IpAddr = "10.0.0.7".parse().unwrap();
        // configured LAN address first, public and tailscale addresses
        // from the config are not promoted, duplicates removed
        assert_eq!(
            megapc.candidates(&[wired, ts4, "8.8.8.8".parse().unwrap(), cur]),
            [wired, cur, ts4, ts6]
        );
        let relayed = peers.iter().find(|p| p.name == "wsl-chris").unwrap();
        assert_eq!(relayed.candidates(&[]), relayed.tailscale_ips);
    }

    #[test]
    fn matches_host_and_dns_names() {
        let peers = parse_status(FIXTURE).unwrap();
        let wsl = peers.iter().find(|p| p.name == "wsl-chris").unwrap();
        assert!(wsl.matches("WSL-CHRIS"));
        assert!(wsl.matches("megapc-wsl"));
        assert!(wsl.matches("megapc-wsl.tail6765ef.ts.net."));
        assert!(!wsl.matches("megapc"));
        assert!(!wsl.matches("tail6765ef"));
    }

    /// two machines with HostName `evo-pc`: DNS names decide, an ambiguous
    /// host name matches none
    #[test]
    fn find_peer_prefers_dns_names() {
        let mut peers = parse_status(FIXTURE).unwrap();
        let dns = |name: &str| find_peer(&peers, name).map(|p| p.dns_name.clone());
        assert_eq!(dns("evo-pc").as_deref(), Some("evo-pc.tail6765ef.ts.net"));
        assert_eq!(
            dns("EVO-PC-1").as_deref(),
            Some("evo-pc-1.tail6765ef.ts.net")
        );
        assert_eq!(
            dns("wsl-chris").as_deref(),
            Some("megapc-wsl.tail6765ef.ts.net")
        );
        for p in peers
            .iter_mut()
            .filter(|p| p.dns_name.starts_with("evo-pc."))
        {
            p.dns_name = "evo-pc-2.tail6765ef.ts.net".into();
        }
        assert!(find_peer(&peers, "evo-pc").is_none());
    }

    fn probed(peer: &TailnetPeer, answering: &[IpAddr]) -> Probed {
        let addresses = peer.candidates(&[]);
        Probed {
            peer: peer.clone(),
            answered: addresses.iter().map(|a| answering.contains(a)).collect(),
            addresses,
        }
    }

    #[test]
    fn shared_lan_address_needs_own_tailscale_answer() {
        let peers = parse_status(FIXTURE).unwrap();
        let megapc = peers.iter().find(|p| p.name == "megapc").unwrap();
        let lan = megapc.lan.unwrap();
        // WSL on megapc (mirrored networking) reports the host's LAN path
        let mut wsl = peers
            .iter()
            .find(|p| p.name == "wsl-chris")
            .unwrap()
            .clone();
        wsl.lan = Some(lan);
        let ts = megapc.tailscale_ips[0];

        let found = select(vec![probed(megapc, &[lan, ts]), probed(&wsl, &[lan])], 4242);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer.name, "megapc");
        assert_eq!(found[0].address, lan);
        assert!(found[0].on_lan);

        // an unshared LAN address counts on its own; tailscale otherwise
        let found = select(vec![probed(megapc, &[lan])], 4242);
        assert_eq!(found[0].address, lan);
        let found = select(vec![probed(megapc, &[ts]), probed(&wsl, &[])], 4242);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].address, ts);
        assert!(!found[0].on_lan);
    }
}

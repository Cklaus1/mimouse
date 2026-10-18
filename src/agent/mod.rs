//! Agent control (tasks/agent-control.md): a local agent observes and drives
//! this machine or a peer through typed requests ([`proto`]).
//!
//! The service owns one [`Agent`]: the target side ([`Target`], serving the
//! local IPC and peers over the agent channel) and the requester side
//! ([`Remote`], one connection per target client).

mod channel;
mod desk;
#[cfg(test)]
mod fake;
pub mod keys;
pub mod proto;
mod target;

use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Arc, RwLock},
};

use tokio::{sync::mpsc, task::spawn_local};
use webrtc_dtls::crypto::Certificate;

pub(crate) use channel::{Audited, Dial, Remote, RemoteEnded, is_agent};
pub(crate) use desk::ServiceDesk;
pub(crate) use target::{Desk, Policy, Target, TargetEvent};

use crate::clipboard::Incoming;

/// How long an IPC client waits for the reply to an `agent` request: the
/// service may dial a client, wait for the screen to settle and for the
/// answer, so this exceeds all of them together.
pub const IPC_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(
    channel::DIAL_TIMEOUT.as_millis() as u64
        + channel::ANSWER_TIMEOUT.as_millis() as u64
        + proto::Observe::MAX_TIMEOUT_MS as u64
        + 5_000,
);

/// what the service hears from the agent layer
#[derive(Debug)]
pub(crate) enum AgentEvent {
    /// a session on this machine started or ended
    Session(TargetEvent),
    /// a peer's request was carried out (or refused) here
    Audited(Audited),
    /// a session this machine holds on a target client ended
    RemoteEnded(RemoteEnded),
}

pub(crate) struct Agent<D: Desk> {
    target: Target<D>,
    tls: Option<Rc<channel::Tls>>,
    remote: Option<Rc<Remote>>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    sessions: mpsc::UnboundedReceiver<TargetEvent>,
    audit_tx: mpsc::UnboundedSender<Audited>,
    audit_rx: mpsc::UnboundedReceiver<Audited>,
    remote_rx: mpsc::UnboundedReceiver<RemoteEnded>,
}

impl<D: Desk> Agent<D> {
    /// the agent layer; `incoming`: agent connections from the TCP listener
    pub(crate) fn new(
        desk: D,
        policy: Policy,
        cert: &Certificate,
        authorized: Arc<RwLock<HashMap<String, String>>>,
        incoming: mpsc::UnboundedReceiver<Incoming>,
    ) -> Self {
        let (target, sessions) = Target::new(desk, policy);
        let tls = match channel::Tls::new(cert, authorized) {
            Ok(tls) => Some(Rc::new(tls)),
            Err(e) => {
                log::warn!("agent channel unavailable: {e}");
                None
            }
        };
        let (remote, remote_rx) = match &tls {
            Some(tls) => {
                let (remote, rx) = Remote::new(tls.clone());
                (Some(Rc::new(remote)), rx)
            }
            None => (None, mpsc::unbounded_channel().1),
        };
        let (audit_tx, audit_rx) = mpsc::unbounded_channel();
        Self {
            target,
            tls,
            remote,
            incoming,
            sessions,
            audit_tx,
            audit_rx,
            remote_rx,
        }
    }

    pub(crate) fn target(&self) -> &Target<D> {
        &self.target
    }

    /// the requester side; `None` when the agent channel is unavailable
    pub(crate) fn remote(&self) -> Option<Rc<Remote>> {
        self.remote.clone()
    }

    /// the next event; serves incoming agent connections meanwhile
    pub(crate) async fn event(&mut self) -> AgentEvent {
        loop {
            tokio::select! {
                Some(incoming) = self.incoming.recv() => {
                    let Some(tls) = &self.tls else { continue };
                    spawn_local(channel::serve(
                        self.target.clone(),
                        tls.clone(),
                        incoming,
                        self.audit_tx.clone(),
                    ));
                }
                Some(event) = self.sessions.recv() => return AgentEvent::Session(event),
                Some(audited) = self.audit_rx.recv() => return AgentEvent::Audited(audited),
                Some(ended) = self.remote_rx.recv() => return AgentEvent::RemoteEnded(ended),
            }
        }
    }
}

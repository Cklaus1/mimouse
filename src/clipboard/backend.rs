//! What every clipboard backend shares.
//!
//! A platform implements [`Backend::session`]: it watches the local
//! clipboard and puts contents from peers on it, and talks to the rest of
//! MiMouse only through [`Context`], which keeps the rules:
//!
//! - A local change is noticed ([`Context::noticed`]) before its contents
//!   are read; contents from peers accepted before it are never set (B21:
//!   the backend may notice a copy before the clipboard task hears of it).
//! - What was read is reported as a copy, a clear, or contents not shared
//!   (secret, or no supported format); a clear is only an owner that offers
//!   no format at all.
//! - The contents found when a session starts are not sent
//!   ([`Context::startup`]); an empty clipboard then is not a clear.
//! - Only the latest contents from peers are set ([`Context::next_set`]);
//!   they stay pending while the session waits for approval or a retry.
//! - The contents set from a peer stay [`Context::owned`] until a local
//!   change, also across sessions, for platforms whose selection ends with
//!   the session (portal).
//! - The platform never reports its own writes back.
//!
//! [`spawn`] runs sessions: [`End::Retry`] starts a new one after
//! [`RETRY_DELAY`], [`End::Final`] ends the backend, after which received
//! updates are refused until the service restarts.

// macOS has no clipboard backend yet
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::{sync::Arc, time::Duration, time::Instant};

use tokio::task::spawn_local;

use super::{BackendTx, ChangesTx, ClipboardData, LocalChange, SetRequest, SetRx, StatusTx};
use crate::ipc::{ClipboardState, ClipboardStatus};

/// how long after a failed session the next one starts
pub(super) const RETRY_DELAY: Duration = Duration::from_secs(10);

/// why a session ended
#[derive(Debug)]
pub(super) enum End {
    /// the platform may work again later: a new session starts after
    /// [`RETRY_DELAY`]
    Retry(String),
    /// it will not while the service runs (a denied permission request, a
    /// missing interface)
    // Windows never ends for good
    #[cfg_attr(windows, allow(dead_code))]
    Final(String),
}

/// a clipboard backend for one platform
pub(super) trait Backend {
    /// shares the local clipboard until the clipboard task is gone
    /// ([`Context::next_set`] returns `None`: Ok) or the platform fails
    async fn session(&mut self, cx: &mut Context) -> Result<(), End>;
}

/// a local change noticed at the time it holds, to report once its
/// contents were read
#[must_use = "noticed changes are reported"]
pub(super) struct Noticed(Instant);

/// a backend's connection to the clipboard task
pub(super) struct Context {
    changes: ChangesTx,
    status: StatusTx,
    sets: SetRx,
    owned: Option<Arc<ClipboardData>>,
}

impl Context {
    /// a permission dialog waits for the user
    // only the portal asks for permission
    #[cfg_attr(windows, allow(dead_code))]
    pub(super) fn awaiting_approval(&self) {
        self.status
            .send_replace(ClipboardStatus::new(ClipboardState::AwaitingApproval));
    }

    /// the session shares the clipboard
    pub(super) fn enabled(&self) {
        self.status
            .send_replace(ClipboardStatus::new(ClipboardState::Enabled));
    }

    /// the contents set from a peer that the local clipboard holds, until a
    /// local change
    // only the portal's selection ends with the session
    #[cfg_attr(windows, allow(dead_code))]
    pub(super) fn owned(&self) -> Option<&Arc<ClipboardData>> {
        self.owned.as_ref()
    }

    /// the local clipboard changed at `at`, before its contents are read:
    /// contents from peers accepted before are no longer set
    pub(super) fn noticed(&mut self, at: Instant) -> Noticed {
        self.sets.noticed(at);
        self.owned = None;
        Noticed(at)
    }

    /// the contents read after `noticed`; without a supported format they
    /// are not shared, which is not a clear
    pub(super) fn copied(&mut self, noticed: Noticed, data: ClipboardData) {
        if data.is_empty() {
            return self.not_shared(noticed);
        }
        self.report(LocalChange::Changed(noticed.0, Some(Arc::new(data))));
    }

    /// the owner noticed offers no format at all
    pub(super) fn cleared(&mut self, noticed: Noticed) {
        self.report(LocalChange::Changed(noticed.0, Some(Arc::default())));
    }

    /// the contents noticed are not shared (marked secret, no supported
    /// format); entering a client then sends nothing
    pub(super) fn not_shared(&mut self, noticed: Noticed) {
        self.report(LocalChange::Changed(noticed.0, None));
    }

    /// the contents found when the session started: they are not sent, and
    /// an empty clipboard was not cleared by anyone
    pub(super) fn startup(&mut self, data: ClipboardData) {
        self.owned = None;
        if !data.is_empty() {
            self.report(LocalChange::Startup(Arc::new(data)));
        }
    }

    /// the latest contents from a peer to set that were not taken yet and
    /// are newer than the latest local change noticed; owned from now on.
    /// `None` once the clipboard task is gone. Cancel safe.
    pub(super) async fn next_set(&mut self) -> Option<SetRequest> {
        let set = self.sets.recv().await?;
        self.owned = Some(set.data.clone());
        Some(set)
    }

    fn report(&self, change: LocalChange) {
        // the clipboard task is gone: `next_set` ends the session
        let _ = self.changes.send(change);
    }
}

/// runs `backend`'s sessions; the sender closes once it ended for good
pub(super) fn spawn(
    backend: impl Backend + 'static,
    changes: ChangesTx,
    status: StatusTx,
) -> BackendTx {
    let (tx, sets) = super::set_channel();
    let cx = Context {
        changes,
        status,
        sets,
        owned: None,
    };
    spawn_local(run(backend, cx));
    tx
}

async fn run(mut backend: impl Backend, mut cx: Context) {
    loop {
        let end = match backend.session(&mut cx).await {
            Ok(()) => return,
            Err(end) => end,
        };
        crate::telemetry::COUNTERS.clipboard_failures.bump();
        match end {
            End::Final(reason) => {
                log::warn!("clipboard: {reason}");
                cx.status.send_replace(ClipboardStatus::disabled(format!(
                    "{reason}; restart the service to ask again"
                )));
                return;
            }
            End::Retry(reason) => {
                log::warn!("clipboard: {reason}, retrying in {RETRY_DELAY:?}");
                cx.status.send_replace(ClipboardStatus::disabled(format!(
                    "{reason}, retrying in {RETRY_DELAY:?}"
                )));
            }
        }
        if !cx.sets.keep_for(RETRY_DELAY).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use futures::FutureExt;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::clipboard::{MIME_TEXT, set_channel};

    fn text(s: &str) -> ClipboardData {
        let mut data = ClipboardData::default();
        data.push(MIME_TEXT, s.as_bytes().to_vec());
        data
    }

    fn set(s: &str, accepted: Instant) -> SetRequest {
        SetRequest {
            data: Arc::new(text(s)),
            accepted,
        }
    }

    struct Harness {
        backend: BackendTx,
        changes: mpsc::UnboundedReceiver<LocalChange>,
        status: watch::Receiver<ClipboardStatus>,
        cx: Context,
    }

    fn harness() -> Harness {
        let (changes_tx, changes) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ClipboardStatus::new(ClipboardState::Starting));
        let (backend, sets) = set_channel();
        let cx = Context {
            changes: changes_tx,
            status: status_tx,
            sets,
            owned: None,
        };
        Harness {
            backend,
            changes,
            status,
            cx,
        }
    }

    impl Harness {
        fn reported(&mut self) -> Vec<LocalChange> {
            std::iter::from_fn(|| self.changes.try_recv().ok()).collect()
        }
    }

    /// a backend running the given session body each time
    struct Scripted<F>(F);

    impl<F: AsyncFnMut(&mut Context) -> Result<(), End>> Backend for Scripted<F> {
        async fn session(&mut self, cx: &mut Context) -> Result<(), End> {
            (self.0)(cx).await
        }
    }

    #[test]
    fn copies_clears_and_secrets_are_told_apart() {
        let mut h = harness();
        let t = Instant::now();
        let n = h.cx.noticed(t);
        h.cx.copied(n, text("a"));
        let n = h.cx.noticed(t);
        h.cx.cleared(n);
        let n = h.cx.noticed(t);
        h.cx.not_shared(n);
        // only formats no backend reads: not shared, not a clear
        let n = h.cx.noticed(t);
        h.cx.copied(n, ClipboardData::default());
        assert_eq!(
            h.reported(),
            [
                LocalChange::Changed(t, Some(Arc::new(text("a")))),
                LocalChange::Changed(t, Some(Arc::default())),
                LocalChange::Changed(t, None),
                LocalChange::Changed(t, None),
            ]
        );
    }

    #[test]
    fn startup_contents_are_not_a_change_and_empty_ones_nothing() {
        let mut h = harness();
        h.cx.startup(ClipboardData::default());
        h.cx.startup(text("old"));
        assert_eq!(h.reported(), [LocalChange::Startup(Arc::new(text("old")))]);
    }

    /// B21: the backend noticed a copy the task has not heard of yet; an
    /// update accepted before it is not set, one accepted after it is
    #[test]
    fn an_update_older_than_a_noticed_change_is_not_set() {
        let mut h = harness();
        let accepted = Instant::now();
        let copied = accepted + Duration::from_millis(1);
        let _ = h.cx.noticed(copied);

        h.backend.send_replace(Some(set("slow", accepted)));
        assert_eq!(h.cx.next_set().now_or_never(), None, "older update set");
        assert_eq!(h.cx.owned(), None);

        let newer = set("newer", copied);
        h.backend.send_replace(Some(newer.clone()));
        assert_eq!(h.cx.next_set().now_or_never(), Some(Some(newer)));
    }

    #[test]
    fn set_contents_are_owned_until_a_local_change() {
        let mut h = harness();
        let t = Instant::now();
        h.backend.send_replace(Some(set("remote", t)));
        h.cx.next_set().now_or_never().flatten().expect("set");
        assert_eq!(h.cx.owned(), Some(&Arc::new(text("remote"))));
        let _ = h.cx.noticed(t + Duration::from_millis(1));
        assert_eq!(h.cx.owned(), None);

        h.backend
            .send_replace(Some(set("remote", t + Duration::from_millis(2))));
        h.cx.next_set().now_or_never().flatten().expect("set");
        h.cx.startup(text("local"));
        assert_eq!(h.cx.owned(), None);
    }

    /// a failed session is retried after `RETRY_DELAY`; what peers sent
    /// meanwhile is set by the next session, only the latest, and the
    /// contents set before are still owned (the portal announces them again)
    #[tokio::test(start_paused = true)]
    async fn a_retried_session_gets_the_latest_contents_received_meanwhile() {
        let h = harness();
        let started = Rc::new(RefCell::new(Vec::new()));
        let log = started.clone();
        let backend = Scripted(async move |cx: &mut Context| {
            log.borrow_mut()
                .push((tokio::time::Instant::now(), cx.owned().cloned()));
            let set = cx.next_set().await;
            if log.borrow().len() == 1 {
                return Err(End::Retry("session closed".into()));
            }
            log.borrow_mut()
                .push((tokio::time::Instant::now(), set.map(|s| s.data)));
            while cx.next_set().await.is_some() {}
            Ok(())
        });
        let t0 = tokio::time::Instant::now();
        let t = Instant::now();
        let Harness {
            backend: tx,
            status,
            cx,
            ..
        } = h;
        let peers = async {
            tx.send_replace(Some(set("first", t)));
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                *status.borrow(),
                ClipboardStatus::disabled(format!("session closed, retrying in {RETRY_DELAY:?}"))
            );
            tx.send_replace(Some(set("second", t)));
            tx.send_replace(Some(set("third", t)));
            tokio::time::sleep(RETRY_DELAY).await;
            drop(tx);
        };
        tokio::join!(run(backend, cx), peers);

        let started = started.borrow();
        assert_eq!(started.len(), 3, "{started:?}");
        assert_eq!(started[0], (t0, None));
        assert_eq!(
            started[1],
            (t0 + RETRY_DELAY, Some(Arc::new(text("first"))))
        );
        assert_eq!(started[2].1, Some(Arc::new(text("third"))));
    }

    /// a final end closes the set channel: the clipboard task refuses
    /// received updates from then on
    #[tokio::test(start_paused = true)]
    async fn a_final_end_closes_the_backend() {
        let h = harness();
        let sessions = Rc::new(RefCell::new(0));
        let count = sessions.clone();
        let backend = Scripted(async move |_: &mut Context| {
            *count.borrow_mut() += 1;
            Err(End::Final("permission request failed: cancelled".into()))
        });
        run(backend, h.cx).await;
        assert_eq!(*sessions.borrow(), 1);
        assert_eq!(
            *h.status.borrow(),
            ClipboardStatus::disabled(
                "permission request failed: cancelled; restart the service to ask again"
            )
        );
        assert!(h.backend.closed().now_or_never().is_some());
    }

    /// the clipboard task going away during the retry wait ends the
    /// backend without another session
    #[tokio::test(start_paused = true)]
    async fn the_task_gone_during_a_retry_ends_the_backend() {
        let h = harness();
        let sessions = Rc::new(RefCell::new(0));
        let count = sessions.clone();
        let backend = Scripted(async move |_: &mut Context| {
            *count.borrow_mut() += 1;
            Err(End::Retry("timed out".into()))
        });
        drop(h.backend);
        run(backend, h.cx).await;
        assert_eq!(*sessions.borrow(), 1);
    }
}

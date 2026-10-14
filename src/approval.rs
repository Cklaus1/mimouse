//! Capture and emulation backends flag a pending permission request (the
//! desktop portal dialog) on a watch channel; this turns the flag into what
//! the user sees: an `awaiting_approval` status and repeated log warnings.

use std::time::Duration;

use tokio::{
    sync::watch,
    time::{Instant, interval_at, sleep},
};

use crate::ipc::Status;

/// the status shown to the user: a pending approval hides enabled /
/// disabled, since a re-created portal session waits on the same dialog
pub(crate) fn reported_status(status: Status, awaiting: bool) -> Status {
    match awaiting {
        true => Status::AwaitingApproval,
        false => status,
    }
}

/// how long a request may be pending before it counts as waiting for the
/// user: a portal restoring a saved grant answers within this, a dialog not
pub(crate) const GRACE: Duration = Duration::from_secs(2);
/// how often the log reminds the user while a request is waiting
pub(crate) const WARN_EVERY: Duration = Duration::from_secs(30);

/// Calls `report(true)` once a request has been pending for `grace`, warns
/// every `warn_every` while it stays pending and calls `report(false)` when
/// it is answered or the backend went away. Returns when the sender is
/// dropped.
pub(crate) async fn forward(
    mut pending: watch::Receiver<bool>,
    what: &str,
    grace: Duration,
    warn_every: Duration,
    mut report: impl FnMut(bool),
) {
    loop {
        if pending.wait_for(|p| *p).await.is_err() {
            return;
        }
        tokio::select! {
            answered = pending.wait_for(|p| !*p) => match answered {
                Ok(_) => continue,
                Err(_) => return,
            },
            _ = sleep(grace) => {}
        }
        report(true);
        log::warn!("{what} is waiting for permission: approve the system dialog");
        let mut warn = interval_at(Instant::now() + warn_every, warn_every);
        let answered = loop {
            tokio::select! {
                answered = pending.wait_for(|p| !*p) => break answered.is_ok(),
                _ = warn.tick() => {
                    log::warn!("{what} is still waiting for permission: approve the system dialog");
                }
            }
        };
        report(false);
        if !answered {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, future::Future, rc::Rc};

    const GRACE: Duration = Duration::from_millis(30);
    const LONG: Duration = Duration::from_secs(3600);

    /// runs `forward` against a backend that sets the flag as `script` does
    async fn reports<F: Future<Output = ()>>(
        script: impl FnOnce(watch::Sender<bool>) -> F,
    ) -> Vec<bool> {
        let (tx, rx) = watch::channel(false);
        let seen = Rc::new(RefCell::new(vec![]));
        let record = seen.clone();
        tokio::join!(
            forward(rx, "test", GRACE, LONG, move |a| record
                .borrow_mut()
                .push(a)),
            script(tx),
        );
        seen.take()
    }

    #[tokio::test(start_paused = true)]
    async fn restored_grant_is_never_reported() {
        let seen = reports(|tx| async move {
            for _ in 0..3 {
                tx.send_replace(true);
                sleep(Duration::from_millis(1)).await;
                tx.send_replace(false);
            }
        })
        .await;
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn dialog_is_reported_until_answered_then_again() {
        let seen = reports(|tx| async move {
            for _ in 0..2 {
                tx.send_replace(true);
                sleep(GRACE * 3).await;
                tx.send_replace(false);
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(seen, [true, false, true, false]);
    }

    #[tokio::test(start_paused = true)]
    async fn backend_gone_while_waiting_clears_the_status() {
        let seen = reports(|tx| async move {
            tx.send_replace(true);
            sleep(GRACE * 3).await;
            drop(tx);
        })
        .await;
        assert_eq!(seen, [true, false]);
    }
}

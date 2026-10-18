//! Heuristic: polls the session idle time (GNOME's Mutter idle monitor,
//! else `org.freedesktop.ScreenSaver` where it answers in milliseconds) and
//! reports input made more than `SLACK` after the last injection.
//!
//! Limit: the idle time only tells when the latest input happened, not
//! whose it was. While the agent injects more often than every `SLACK`, a
//! person's input hides among the injections and is noticed only once the
//! agent pauses for longer than `SLACK` and the person keeps going.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use tokio::sync::{oneshot, watch};
use zbus::Connection;

use super::{
    PhysicalError,
    idle::{IdleDetector, POLL, reports_ms},
};

pub(super) const METHOD: &str = "idle";

/// how long `watch` waits for the session bus to answer the probe
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// the gap between the two readings that tell `ScreenSaver`'s units
const CALIBRATION_GAP: Duration = Duration::from_millis(100);
/// readings until `ScreenSaver` is given up on (input in between spoils one)
const CALIBRATION_TRIES: u32 = 3;

const _: () = assert!(
    CALIBRATION_GAP.as_millis() * CALIBRATION_TRIES as u128 * 2 < PROBE_TIMEOUT.as_millis()
);

pub(super) struct Watcher {
    injected: Arc<Mutex<Option<Instant>>>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn start(events: watch::Sender<()>) -> Result<Self, PhysicalError> {
        let injected = Arc::new(Mutex::new(None));
        let (stop, stopped) = oneshot::channel();
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("physical-input".into())
            .spawn({
                let injected = injected.clone();
                move || run(events, injected, ready_tx, stopped)
            })
            .map_err(|e| PhysicalError::Failed(format!("thread: {e}")))?;
        let mut watcher = Watcher {
            injected,
            stop: Some(stop),
            thread: Some(thread),
        };
        match ready.recv_timeout(PROBE_TIMEOUT) {
            Ok(Ok(())) => Ok(watcher),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // drop stops the probe; the thread is not left behind
                watcher.stop.take();
                Err(PhysicalError::Failed(format!(
                    "the session idle time did not arrive within {PROBE_TIMEOUT:?}"
                )))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(PhysicalError::Failed("watcher thread ended".into()))
            }
        }
    }

    pub(super) fn injected(&self, at: Instant) {
        let mut last = self.injected.lock().expect("poisoned");
        *last = (*last).max(Some(at));
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    events: watch::Sender<()>,
    injected: Arc<Mutex<Option<Instant>>>,
    ready: mpsc::SyncSender<Result<(), PhysicalError>>,
    stopped: oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(PhysicalError::Failed(format!("runtime: {e}"))));
            return;
        }
    };
    runtime.block_on(async move {
        tokio::select! {
            _ = stopped => {}
            _ = watch_idle(events, injected, ready) => {}
        }
    });
}

async fn watch_idle(
    events: watch::Sender<()>,
    injected: Arc<Mutex<Option<Instant>>>,
    ready: mpsc::SyncSender<Result<(), PhysicalError>>,
) {
    let source = match IdleSource::probe().await {
        Ok(source) => source,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    log::info!("watching for physical input: {}", source.name());
    let _ = ready.send(Ok(()));

    let mut detector = IdleDetector::default();
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let idle = match source.idle().await {
            Ok(idle) => idle,
            Err(e) => {
                // ends the watch: `next` returns None
                log::warn!("physical input: {} failed: {e}", source.name());
                return;
            }
        };
        if let Some(at) = *injected.lock().expect("poisoned") {
            detector.injected(at);
        }
        if detector.sample(now, idle) {
            log::debug!("physical input (idle {idle:?})");
            events.send_replace(());
        }
    }
}

enum Kind {
    Mutter,
    ScreenSaver,
}

struct IdleSource {
    conn: Connection,
    kind: Kind,
}

impl IdleSource {
    async fn probe() -> Result<Self, PhysicalError> {
        let conn = Connection::session()
            .await
            .map_err(|e| PhysicalError::Unsupported(format!("no session bus: {e}")))?;
        let mutter = IdleSource {
            conn,
            kind: Kind::Mutter,
        };
        let mutter_err = match mutter.idle().await {
            Ok(_) => return Ok(mutter),
            Err(e) => e,
        };
        let screensaver = IdleSource {
            conn: mutter.conn,
            kind: Kind::ScreenSaver,
        };
        let mut seconds = false;
        for _ in 0..CALIBRATION_TRIES {
            let start = Instant::now();
            let first = screensaver.screensaver_idle().await.map_err(|e| {
                PhysicalError::Unsupported(format!(
                    "no session idle time: Mutter: {mutter_err}; ScreenSaver: {e}"
                ))
            })?;
            tokio::time::sleep(CALIBRATION_GAP).await;
            let second = screensaver
                .screensaver_idle()
                .await
                .map_err(|e| PhysicalError::Failed(format!("ScreenSaver: {e}")))?;
            if reports_ms(first, second, start.elapsed()) {
                return Ok(screensaver);
            }
            seconds = second.abs_diff(first) <= 1;
        }
        Err(PhysicalError::Unsupported(if seconds {
            format!("Mutter: {mutter_err}; ScreenSaver idle time is in seconds, too coarse")
        } else {
            format!("Mutter: {mutter_err}; ScreenSaver idle time has unknown units")
        }))
    }

    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Mutter => "org.gnome.Mutter.IdleMonitor",
            Kind::ScreenSaver => "org.freedesktop.ScreenSaver",
        }
    }

    async fn idle(&self) -> zbus::Result<Duration> {
        match self.kind {
            Kind::Mutter => {
                let ms: u64 = self
                    .conn
                    .call_method(
                        Some("org.gnome.Mutter.IdleMonitor"),
                        "/org/gnome/Mutter/IdleMonitor/Core",
                        Some("org.gnome.Mutter.IdleMonitor"),
                        "GetIdletime",
                        &(),
                    )
                    .await?
                    .body()
                    .deserialize()?;
                Ok(Duration::from_millis(ms))
            }
            Kind::ScreenSaver => Ok(Duration::from_millis(self.screensaver_idle().await?.into())),
        }
    }

    async fn screensaver_idle(&self) -> zbus::Result<u32> {
        self.conn
            .call_method(
                Some("org.freedesktop.ScreenSaver"),
                "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"),
                "GetSessionIdleTime",
                &(),
            )
            .await?
            .body()
            .deserialize()
    }
}

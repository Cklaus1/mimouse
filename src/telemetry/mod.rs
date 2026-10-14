//! Opt-in telemetry: off unless `telemetry = true` in config.toml (`mimouse
//! cli telemetry on`). Counters are atomics bumped where events happen; a
//! task on the service's LocalSet drains them once a minute into the
//! current UTC day, persists the day and the queue of completed days and
//! crash records (`spawn_blocking`, at most once a minute and on shutdown)
//! and uploads the queue once a day, plus at a start with a crash record
//! waiting. The upload body is `telemetry-collector/schema.json`;
//! `mimouse cli telemetry show` prints the same bytes. Turning telemetry off
//! deletes the install id and the queue and asks the collector to delete
//! the install's rows.

mod counters;
mod crash;
mod http;
mod schema;
mod store;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, path::Path, rc::Rc, sync::atomic::Ordering::Relaxed, time::Duration};

use jiff::{Timestamp, tz::TimeZone};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, spawn_blocking, spawn_local},
    time::{MissedTickBehavior, interval},
};

use crate::{
    config::Config,
    ipc::{FrontendRequest, TelemetryCrash, TelemetryShow, TelemetryStatus},
};

pub(crate) use counters::COUNTERS;
use counters::Counters;
pub(crate) use http::Endpoint;
use http::{Https, Request, Transport};
use schema::{Batch, Crash, Identity, SCHEMA_VERSION};
pub(crate) use store::Dirs;
use store::State;

/// how often counters are drained into the day, the day is persisted and
/// a due upload starts
const TICK: Duration = Duration::from_secs(60);

/// the endpoint the release workflow builds in (tag builds only)
const BUILT_IN_ENDPOINT: Option<&str> = option_env!("MIMOUSE_TELEMETRY_URL");

/// commands that run without the service, counted when it is up
const LOCAL_COMMANDS: [&str; 2] = ["doctor", "discover"];

/// what is collected, as `mimouse cli telemetry on` lists it
pub(crate) const COLLECTED: &str = "\
Sent once a day (completed UTC days, at most 7) with a random install id:
  - MiMouse version and commit, OS family and version, Linux desktop and session type
  - capture and emulation backend in use and how it was chosen
  - counts: reconnects, ping timeouts, watchdog key releases, capture and emulation
    disables, permission dialog waits (by duration), clipboard backend failures
  - counts: switches by edge, hotkey and `cli focus`; configured peers; clipboard and
    file transfers (by size); IPC requests by name, without arguments
  - crashes: file:line, a fixed panic message (never a formatted one), backends
Never sent: input, clipboard or file contents and names, host names, addresses,
fingerprints, user names, paths, hook commands or other config values.";

const NO_ENDPOINT: &str = "this build has no telemetry endpoint: set `telemetry_endpoint = \"https://...\"` in config.toml";

/// the endpoint in effect: `telemetry_endpoint` from the config, else the
/// one built in; https only
pub(crate) fn endpoint(configured: Option<&str>) -> Result<Endpoint, String> {
    endpoint_from(configured, BUILT_IN_ENDPOINT)
}

fn endpoint_from(configured: Option<&str>, built_in: Option<&str>) -> Result<Endpoint, String> {
    let set = |url: &&str| !url.is_empty();
    match configured.filter(set).or(built_in.filter(set)) {
        Some(url) => Endpoint::parse(url),
        None => Err(NO_ENDPOINT.to_owned()),
    }
}

/// records panics of the service in the cache dir (see `crash`)
pub(crate) fn install_panic_hook(config_dir: &Path) {
    crash::install(Dirs::new(config_dir).cache, &COUNTERS);
}

/// crash records of earlier runs written while telemetry was off: never
/// uploaded, only shown (`doctor`, `cli telemetry status|show`)
pub(crate) fn local_crashes(dirs: &Dirs) -> Vec<TelemetryCrash> {
    store::local_crashes(dirs).iter().map(summary).collect()
}

fn summary(crash: &Crash) -> TelemetryCrash {
    TelemetryCrash {
        location: crash.location.clone(),
        message: crash.message.clone(),
    }
}

/// what the config asks for
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub endpoint: Result<Endpoint, String>,
}

impl Settings {
    pub(crate) fn of(config: &Config) -> Self {
        Self {
            enabled: config.telemetry(),
            endpoint: endpoint(config.telemetry_endpoint()),
        }
    }
}

/// state shared by the service (requests) and the telemetry task
struct Shared {
    settings: Settings,
    /// the install id is loaded and counting is on
    active: bool,
    install_id: Option<String>,
    state: State,
    local_crashes: Vec<Crash>,
    identity: Identity,
    /// `state` changed since it was persisted
    dirty: bool,
    /// upload at the next tick, though one was attempted today (a start
    /// with a crash record waiting)
    upload_now: bool,
    /// why telemetry could not be turned on
    error: Option<String>,
}

impl Shared {
    /// the next upload's body, if anything is queued
    fn batch(&self) -> Option<Vec<u8>> {
        let install_id = self.install_id.as_deref()?;
        if !self.active || self.state.is_empty() {
            return None;
        }
        let batch = Batch {
            schema_version: SCHEMA_VERSION,
            install_id,
            identity: &self.identity,
            days: &self.state.days,
            crashes: &self.state.crashes,
        };
        let mut body = serde_json::to_vec_pretty(&batch).expect("serializable batch");
        body.push(b'\n');
        Some(body)
    }
}

enum Command {
    Configure(Settings, Option<oneshot::Sender<()>>),
    Shutdown(oneshot::Sender<()>),
}

type Clock = Rc<dyn Fn() -> Timestamp>;

/// the service's handle on telemetry
pub(crate) struct Telemetry {
    shared: Rc<RefCell<Shared>>,
    commands: mpsc::UnboundedSender<Command>,
    task: JoinHandle<()>,
}

impl Telemetry {
    pub(crate) fn new(config: &Config) -> Self {
        Self::start(
            Settings::of(config),
            Dirs::new(config.config_dir()),
            Rc::new(Timestamp::now),
            Rc::new(Https::default()),
            &COUNTERS,
        )
    }

    /// telemetry as configured by `config`, keeping its files in `dirs`
    #[cfg(test)]
    pub(crate) fn for_test(config: &Config, dirs: Dirs) -> Self {
        let clock = Rc::new(Timestamp::now);
        Self::start(
            Settings::of(config),
            dirs,
            clock,
            Rc::new(Https::default()),
            &COUNTERS,
        )
    }

    fn start(
        settings: Settings,
        dirs: Dirs,
        clock: Clock,
        transport: Rc<dyn Transport>,
        counters: &'static Counters,
    ) -> Self {
        let shared = Rc::new(RefCell::new(Shared {
            settings: settings.clone(),
            active: false,
            install_id: None,
            state: State::default(),
            local_crashes: vec![],
            identity: Identity::default(),
            dirty: false,
            upload_now: false,
            error: None,
        }));
        let (commands, rx) = mpsc::unbounded_channel();
        let (results_tx, results) = mpsc::unbounded_channel();
        let task = Task {
            shared: shared.clone(),
            dirs,
            clock,
            transport,
            counters,
            upload: None,
            generation: 0,
            results_tx,
        };
        let task = spawn_local(task.run(settings, rx, results));
        Self {
            shared,
            commands,
            task,
        }
    }

    /// applies changed settings (a reloaded config) in the background
    pub(crate) fn configure(&self, settings: Settings) {
        let _ = self.commands.send(Command::Configure(settings, None));
    }

    /// applies `settings`, returning once the install id exists (on) or
    /// is deleted (off)
    pub(crate) async fn apply(&self, settings: Settings) -> Result<(), String> {
        let (done, applied) = oneshot::channel();
        let _ = self
            .commands
            .send(Command::Configure(settings.clone(), Some(done)));
        let _ = applied.await;
        let shared = self.shared.borrow();
        match (settings.enabled, shared.active) {
            (true, false) => Err(shared
                .error
                .clone()
                .unwrap_or_else(|| "telemetry could not be turned on".into())),
            _ => Ok(()),
        }
    }

    /// counts an IPC request by name; `count_command` stands for the
    /// command it names
    pub(crate) fn count_request(&self, request: &FrontendRequest) {
        let mut shared = self.shared.borrow_mut();
        if !shared.active {
            return;
        }
        let name = match request {
            FrontendRequest::CountCommand { command } if LOCAL_COMMANDS.contains(&&**command) => {
                command.clone()
            }
            FrontendRequest::CountCommand { .. } => return,
            request => match serde_json::to_value(request) {
                Ok(serde_json::Value::Object(mut o)) => match o.remove("action") {
                    Some(serde_json::Value::String(name)) => name,
                    _ => return,
                },
                _ => return,
            },
        };
        shared.state.today.count_command(&name);
        shared.dirty = true;
    }

    pub(crate) fn status(&self) -> TelemetryStatus {
        let shared = self.shared.borrow();
        let endpoint = &shared.settings.endpoint;
        TelemetryStatus {
            enabled: shared.settings.enabled,
            install_id: shared.install_id.clone(),
            endpoint: endpoint.as_ref().ok().map(ToString::to_string),
            endpoint_error: endpoint.as_ref().err().cloned(),
            error: shared.error.clone(),
            queued_days: shared.state.days.len(),
            queued_crashes: shared.state.crashes.len(),
            counting_since: Some(shared.state.today.date.clone()).filter(|d| !d.is_empty()),
            last_attempt: shared.state.last_attempt.clone(),
            last_error: shared.state.last_error.clone(),
            local_crashes: shared.local_crashes.iter().map(summary).collect(),
        }
    }

    /// the exact body of the next upload
    pub(crate) fn show(&self) -> TelemetryShow {
        let shared = self.shared.borrow();
        TelemetryShow {
            enabled: shared.settings.enabled,
            batch: shared
                .batch()
                .map(|b| String::from_utf8(b).expect("JSON is UTF-8")),
            counting_since: Some(shared.state.today.date.clone()).filter(|d| !d.is_empty()),
            local_crashes: shared.local_crashes.iter().map(summary).collect(),
        }
    }

    /// persists the counters and drops a running upload
    pub(crate) async fn shutdown(&self) {
        let (done, finished) = oneshot::channel();
        if self.commands.send(Command::Shutdown(done)).is_ok() {
            let _ = finished.await;
        }
        self.task.abort();
    }
}

/// an upload in flight, aborted when dropped
struct Upload {
    task: JoinHandle<()>,
    /// dates of the days sent
    dates: Vec<String>,
    crashes: Vec<Crash>,
}

impl Drop for Upload {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Task {
    shared: Rc<RefCell<Shared>>,
    dirs: Dirs,
    clock: Clock,
    transport: Rc<dyn Transport>,
    counters: &'static Counters,
    upload: Option<Upload>,
    /// tells the result of the upload in flight from an aborted one's
    generation: u64,
    results_tx: mpsc::UnboundedSender<(u64, Result<u16, String>)>,
}

impl Task {
    async fn run(
        mut self,
        settings: Settings,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut results: mpsc::UnboundedReceiver<(u64, Result<u16, String>)>,
    ) {
        self.configure(settings, true).await;
        let mut tick = interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Configure(settings, done)) => {
                        self.configure(settings, false).await;
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                    Some(Command::Shutdown(done)) => {
                        self.upload = None;
                        self.count();
                        self.persist().await;
                        let _ = done.send(());
                        return;
                    }
                    None => return,
                },
                _ = tick.tick() => {
                    self.count();
                    self.start_upload();
                    self.persist().await;
                }
                Some((generation, result)) = results.recv() => {
                    if generation == self.generation {
                        if let Some(upload) = self.upload.take() {
                            self.uploaded(upload, result);
                        }
                    }
                }
            }
        }
    }

    fn today(&self) -> String {
        (self.clock)().to_zoned(TimeZone::UTC).date().to_string()
    }

    /// drains the counters into the current day, then moves to a new day
    /// if it changed: counts since the last tick go to the day that ended
    fn count(&mut self) {
        let date = self.today();
        let mut shared = self.shared.borrow_mut();
        if !shared.active {
            self.counters.discard();
            return;
        }
        let before = shared.state.today.clone();
        self.counters.drain_into(&mut shared.state.today);
        let rolled = shared.state.roll(&date);
        if rolled || shared.state.today != before {
            shared.dirty = true;
        }
    }

    /// turns telemetry on (install id, persisted state, crash records of
    /// earlier runs) or off (all of it deleted, the collector asked to
    /// delete the install's rows). At the start, `off` also purges what a
    /// stopped service left behind, and so does turning off a setting that
    /// was on but never active (no endpoint)
    async fn configure(&mut self, settings: Settings, start: bool) {
        let (active, was_enabled) = {
            let mut shared = self.shared.borrow_mut();
            let was_enabled = shared.settings.enabled;
            shared.settings = settings.clone();
            if !settings.enabled {
                shared.error = None;
            }
            (shared.active, was_enabled)
        };
        match (settings.enabled, active) {
            (true, false) => match &settings.endpoint {
                Ok(_) => self.turn_on(start).await,
                Err(e) => {
                    log::warn!("telemetry stays off: {e}");
                    self.shared.borrow_mut().error = Some(e.clone());
                }
            },
            (false, true) => self.turn_off(&settings).await,
            (false, false) if start || was_enabled => self.turn_off(&settings).await,
            _ => {}
        }
    }

    async fn turn_on(&mut self, start: bool) {
        let dirs = self.dirs.clone();
        let loaded = spawn_blocking(move || {
            let id = store::install_id(&dirs)?;
            let state = store::load_state(&dirs);
            let crashes = store::uploadable_crashes(&dirs);
            let local = store::local_crashes(&dirs);
            Ok::<_, std::io::Error>((id, state, crashes, local, Identity::detect()))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        let (id, mut state, crashes, local, identity) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                log::warn!("telemetry: could not create the install id: {e}");
                self.shared.borrow_mut().error =
                    Some(format!("could not create the install id: {e}"));
                return;
            }
        };
        state.roll(&self.today());
        let (paths, crashes): (Vec<_>, Vec<_>) = crashes.into_iter().unzip();
        state.queue_crashes(crashes);
        {
            let mut shared = self.shared.borrow_mut();
            shared.upload_now = start && !state.crashes.is_empty();
            shared.install_id = Some(id);
            shared.identity = identity;
            shared.state = state;
            shared.local_crashes = local;
            shared.active = true;
            shared.dirty = true;
            shared.error = None;
        }
        // counts from while it was off are not sent
        self.counters.discard();
        self.counters.enabled.store(true, Relaxed);
        log::info!("telemetry is on");
        // queued: the records are in `telemetry.json` now
        if self.persist().await && !paths.is_empty() {
            let _ = spawn_blocking(move || {
                for path in paths {
                    let _ = std::fs::remove_file(path);
                }
            })
            .await;
        }
    }

    async fn turn_off(&mut self, settings: &Settings) {
        self.counters.enabled.store(false, Relaxed);
        self.upload = None;
        {
            let mut shared = self.shared.borrow_mut();
            shared.active = false;
            shared.install_id = None;
            shared.state = State::default();
            shared.dirty = false;
            shared.upload_now = false;
            shared.error = None;
        }
        let dirs = self.dirs.clone();
        let purged = spawn_blocking(move || {
            let id = store::existing_install_id(&dirs);
            let purged = store::purge(&dirs);
            (id, purged, store::local_crashes(&dirs))
        })
        .await;
        let Ok((id, purged, local)) = purged else {
            return;
        };
        self.shared.borrow_mut().local_crashes = local;
        let Some(id) = id else {
            return;
        };
        match purged {
            Ok(()) => log::info!("telemetry is off: install id and queue deleted"),
            Err(e) => log::warn!("telemetry is off, but not everything was deleted: {e}"),
        }
        // best effort: the collector deletes the install's rows
        let endpoint = match &settings.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                log::warn!("telemetry: server-side rows not deleted: {e}");
                return;
            }
        };
        let request = Request {
            method: "DELETE",
            route: format!("/v1/installs/{id}"),
            body: None,
        };
        let sent = self.transport.send(endpoint, request);
        spawn_local(async move {
            match sent.await {
                Ok(status) if (200..300).contains(&status) => {
                    log::info!("telemetry: the collector deleted this install's rows")
                }
                Ok(status) => log::warn!("telemetry: deleting server-side rows: HTTP {status}"),
                Err(e) => log::warn!("telemetry: deleting server-side rows: {e}"),
            }
        });
    }

    /// starts the upload if one is due: once a day, or now after a start
    /// with a crash record waiting
    fn start_upload(&mut self) {
        if self.upload.is_some() {
            return;
        }
        let date = self.today();
        let mut shared = self.shared.borrow_mut();
        let Ok(endpoint) = shared.settings.endpoint.clone() else {
            return;
        };
        let due = shared.upload_now || shared.state.last_attempt.as_deref() != Some(&*date);
        let Some(body) = shared.batch().filter(|_| due) else {
            return;
        };
        shared.upload_now = false;
        shared.state.last_attempt = Some(date);
        shared.dirty = true;
        let dates = shared.state.days.iter().map(|d| d.date.clone()).collect();
        let crashes = shared.state.crashes.clone();
        drop(shared);
        self.generation += 1;
        let generation = self.generation;
        let results = self.results_tx.clone();
        let request = Request {
            method: "POST",
            route: "/v1/batch".into(),
            body: Some(body),
        };
        let sent = self.transport.send(endpoint, request);
        let task = spawn_local(async move {
            let _ = results.send((generation, sent.await));
        });
        self.upload = Some(Upload {
            task,
            dates,
            crashes,
        });
    }

    /// a 204 removes what was sent; anything else keeps the queue for the
    /// next day
    fn uploaded(&mut self, upload: Upload, result: Result<u16, String>) {
        let mut shared = self.shared.borrow_mut();
        shared.dirty = true;
        let error = match result {
            Ok(204) => {
                let state = &mut shared.state;
                state.days.retain(|d| !upload.dates.contains(&d.date));
                for sent in &upload.crashes {
                    if let Some(i) = state.crashes.iter().position(|c| c == sent) {
                        state.crashes.remove(i);
                    }
                }
                state.last_error = None;
                log::info!("telemetry: uploaded");
                return;
            }
            Ok(status) => format!("the collector answered HTTP {status}"),
            Err(e) => e,
        };
        log::warn!("telemetry: upload failed ({error}), retrying tomorrow");
        shared.state.last_error = Some(error);
    }

    /// writes the state if it changed; whether it is on disk
    async fn persist(&mut self) -> bool {
        let json = {
            let mut shared = self.shared.borrow_mut();
            if !shared.active || !shared.dirty {
                return shared.active;
            }
            shared.dirty = false;
            serde_json::to_vec(&shared.state).expect("serializable state")
        };
        let dirs = self.dirs.clone();
        let saved = spawn_blocking(move || store::save_state(&dirs, &json))
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r);
        if let Err(e) = saved {
            log::warn!("telemetry: {e}");
            self.shared.borrow_mut().dirty = true;
            return false;
        }
        true
    }
}

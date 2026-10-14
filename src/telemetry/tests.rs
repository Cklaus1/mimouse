//! Telemetry as the service runs it: a task on a LocalSet, a clock that
//! follows tokio's (paused) time, a transport recording every request.

use std::{
    cell::RefCell,
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
    rc::Rc,
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use futures::future::LocalBoxFuture;
use input_event::{Event, KeyboardEvent};
use jiff::Timestamp;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::LocalSet,
    time::{Instant, sleep, timeout},
};

use super::{
    counters::Counters,
    crash::{self, FORMATTED},
    http::{Https, Request, Transport},
    schema::{Backend, Day, MAX_COMMANDS, MAX_COUNT, MAX_CRASHES, MAX_DAYS, MAX_PEERS, Selection},
    store::{CrashRecord, State, crash_file_name},
    *,
};
use crate::{
    clipboard::ClipboardData,
    ipc::{BackendInUse, Position},
};

const ENDPOINT: &str = "https://collector.example/telemetry";
/// 12:00 UTC: a day ends 12 h after the start
const START: &str = "2026-10-13T12:00:00Z";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const HALF_DAY: Duration = Duration::from_secs(12 * 60 * 60);

/// what a request got: an immediate status, or one the test sends later
enum Answer {
    Status(u16),
    Held(oneshot::Receiver<u16>),
}

/// records every request; answers from a script, then fails
#[derive(Clone, Default)]
struct Recorder {
    sent: Rc<RefCell<Vec<(Endpoint, Request)>>>,
    answers: Rc<RefCell<VecDeque<Answer>>>,
}

impl Transport for Recorder {
    fn send(
        &self,
        endpoint: Endpoint,
        request: Request,
    ) -> LocalBoxFuture<'static, Result<u16, String>> {
        self.sent.borrow_mut().push((endpoint, request));
        let answer = self.answers.borrow_mut().pop_front();
        Box::pin(async move {
            match answer {
                Some(Answer::Status(status)) => Ok(status),
                Some(Answer::Held(status)) => status.await.map_err(|_| "dropped".to_owned()),
                None => Err("connect: connection refused".to_owned()),
            }
        })
    }
}

impl Recorder {
    fn answer(&self, status: u16) {
        self.answers.borrow_mut().push_back(Answer::Status(status));
    }

    fn requests(&self) -> Vec<Request> {
        self.sent.borrow().iter().map(|(_, r)| r.clone()).collect()
    }

    /// the parsed bodies of the uploads
    fn batches(&self) -> Vec<serde_json::Value> {
        self.requests()
            .iter()
            .filter(|r| r.route == "/v1/batch")
            .map(|r| serde_json::from_slice(r.body.as_deref().expect("a body")).expect("JSON"))
            .collect()
    }
}

/// a scratch directory, removed when dropped
struct Fixture {
    root: PathBuf,
    dirs: Dirs,
    counters: &'static Counters,
    recorder: Recorder,
}

/// a `Fixture` named `$name` with counters of its own
macro_rules! fixture {
    ($name:literal) => {{
        static COUNTERS: Counters = Counters::new();
        Fixture::new($name, &COUNTERS)
    }};
}

impl Fixture {
    /// with `counters` of its own, so tests running in parallel do not
    /// count each other's events (see `fixture!`)
    fn new(name: &str, counters: &'static Counters) -> Self {
        let root =
            std::env::temp_dir().join(format!("mimouse-telemetry-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self {
            dirs: Dirs {
                cache: root.join("cache"),
                config: root.join("config"),
            },
            root,
            counters,
            recorder: Recorder::default(),
        }
    }

    /// telemetry over the recorder, its clock `START` plus tokio's time
    fn start(&self, settings: Settings) -> Telemetry {
        self.start_with(settings, Rc::new(self.recorder.clone()), clock(START))
    }

    fn start_with(
        &self,
        settings: Settings,
        transport: Rc<dyn Transport>,
        clock: Clock,
    ) -> Telemetry {
        Telemetry::start(settings, self.dirs.clone(), clock, transport, self.counters)
    }

    fn id_file(&self) -> PathBuf {
        self.dirs.config.join("telemetry-id")
    }

    fn state_file(&self) -> PathBuf {
        self.dirs.cache.join("telemetry.json")
    }

    fn crash_files(&self) -> Vec<PathBuf> {
        match fs::read_dir(&self.dirs.cache) {
            Ok(entries) => entries
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("crash-")
                })
                .collect(),
            Err(_) => vec![],
        }
    }

    fn write_crash(&self, unique: &str, upload: bool, location: &str) {
        let record = CrashRecord {
            upload,
            crash: Crash {
                location: location.into(),
                message: "channel closed".into(),
                capture_backend: None,
                emulation_backend: None,
            },
        };
        fs::create_dir_all(&self.dirs.cache).unwrap();
        let path = self.dirs.cache.join(crash_file_name(unique));
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `start` plus the time passed since the call
fn clock(start: &str) -> Clock {
    let start: Timestamp = start.parse().unwrap();
    let origin = Instant::now();
    Rc::new(move || start.checked_add(Instant::now() - origin).unwrap())
}

fn on() -> Settings {
    Settings {
        enabled: true,
        endpoint: Endpoint::parse(ENDPOINT),
    }
}

fn off() -> Settings {
    Settings {
        enabled: false,
        endpoint: Endpoint::parse(ENDPOINT),
    }
}

/// lets the telemetry task catch up (blocking file IO holds the paused
/// clock)
async fn settle() {
    sleep(Duration::from_millis(1)).await;
}

fn dates(batch: &serde_json::Value) -> Vec<&str> {
    batch["days"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["date"].as_str().unwrap())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn nothing_is_contacted_while_off_even_with_crash_records() {
    let fx = fixture!("off");
    // one written while it was off, one while it was on (turned off in the
    // config while the service was stopped)
    fx.write_crash("0001", false, "src/capture.rs:1");
    fx.write_crash("0002", true, "src/capture.rs:2");
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(off());
            settle().await;
            for _ in 0..3 {
                fx.counters.reconnects.bump();
                telemetry.count_request(&FrontendRequest::List);
                sleep(DAY).await;
            }
            telemetry.shutdown().await;
            assert!(fx.recorder.requests().is_empty());
            assert!(!fx.id_file().exists());
            assert!(!fx.state_file().exists());
            let status = telemetry.status();
            assert!(!status.enabled && status.install_id.is_none());
            // the record written while on was queued, and the queue is gone
            assert_eq!(fx.crash_files().len(), 1);
            let shown: Vec<_> = status.local_crashes.iter().map(|c| &*c.location).collect();
            assert_eq!(shown, ["src/capture.rs:1"]);
            assert_eq!(telemetry.show().batch, None);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn turning_on_without_an_endpoint_fails_and_says_why() {
    let fx = fixture!("no-endpoint");
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(off());
            let settings = Settings {
                enabled: true,
                endpoint: endpoint_from(None, Some("")),
            };
            let e = telemetry.apply(settings).await.unwrap_err();
            assert!(e.contains("no telemetry endpoint"), "{e}");
            assert!(e.contains("telemetry_endpoint"), "{e}");
            assert!(!fx.id_file().exists());
            assert_eq!(telemetry.status().install_id, None);
        })
        .await;
}

#[test]
fn configured_endpoint_overrides_the_built_in_one() {
    let url = |configured, built_in| endpoint_from(configured, built_in).map(|e| e.to_string());
    assert_eq!(
        url(
            Some("https://mine.example/"),
            Some("https://built-in.example")
        ),
        Ok("https://mine.example".to_owned())
    );
    assert_eq!(
        url(None, Some("https://built-in.example")),
        Ok("https://built-in.example".to_owned())
    );
    // an empty `MIMOUSE_TELEMETRY_URL` is no endpoint
    assert!(url(None, Some("")).is_err());
    assert!(url(None, None).is_err());
    // refused when used, whatever validated the config
    assert!(
        url(
            Some("http://mine.example"),
            Some("https://built-in.example")
        )
        .unwrap_err()
        .contains("https://")
    );
}

#[tokio::test(start_paused = true)]
async fn failed_upload_keeps_the_queue_and_retries_the_next_day() {
    let fx = fixture!("retry");
    fx.recorder.answer(500);
    fx.recorder.answer(204);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            for _ in 0..3 {
                fx.counters.reconnects.bump();
            }
            // the first day ends: its upload fails
            sleep(HALF_DAY + TICK).await;
            let batches = fx.recorder.batches();
            assert_eq!(batches.len(), 1);
            assert_eq!(dates(&batches[0]), ["2026-10-13"]);
            assert_eq!(batches[0]["days"][0]["reliability"]["reconnects"], 3);
            let status = telemetry.status();
            assert_eq!(status.queued_days, 1);
            assert!(status.last_error.unwrap().contains("500"));
            // not again that day
            sleep(HALF_DAY).await;
            assert_eq!(fx.recorder.requests().len(), 1);
            // the next day sends both days, and a 204 clears them
            sleep(HALF_DAY).await;
            let batches = fx.recorder.batches();
            assert_eq!(batches.len(), 2);
            assert_eq!(dates(&batches[1]), ["2026-10-13", "2026-10-14"]);
            let status = telemetry.status();
            assert_eq!((status.queued_days, status.last_error), (0, None));
            assert_eq!(telemetry.show().batch, None);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn queue_never_holds_more_than_seven_days() {
    let fx = fixture!("cap");
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            sleep(HALF_DAY + 10 * DAY + TICK).await;
            let batches = fx.recorder.batches();
            // one attempt a day, every one failing
            assert_eq!(batches.len(), 11);
            assert!(batches.iter().all(|b| dates(b).len() <= MAX_DAYS));
            assert_eq!(
                dates(batches.last().unwrap()),
                [
                    "2026-10-17",
                    "2026-10-18",
                    "2026-10-19",
                    "2026-10-20",
                    "2026-10-21",
                    "2026-10-22",
                    "2026-10-23"
                ]
            );
            assert_eq!(telemetry.status().queued_days, MAX_DAYS);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_204_clears_only_what_was_sent() {
    let fx = fixture!("in-flight");
    let (answer, held) = oneshot::channel();
    fx.recorder
        .answers
        .borrow_mut()
        .push_back(Answer::Held(held));
    fx.recorder.answer(204);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            sleep(HALF_DAY + TICK).await;
            // another day ends while the upload of the first is in flight
            sleep(DAY).await;
            assert_eq!(fx.recorder.requests().len(), 1);
            assert_eq!(telemetry.status().queued_days, 2);
            answer.send(204).unwrap();
            settle().await;
            assert_eq!(telemetry.status().queued_days, 1);
            // no attempt yet today: the rest goes at the next tick
            sleep(TICK).await;
            let batches = fx.recorder.batches();
            assert_eq!(batches.len(), 2);
            assert_eq!(dates(&batches[1]), ["2026-10-14"]);
            assert_eq!(telemetry.status().queued_days, 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn counts_and_install_id_survive_a_restart() {
    let fx = fixture!("restart");
    fx.recorder.answer(204);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            let id = telemetry.status().install_id.unwrap();
            fx.counters.ping_timeouts.bump();
            telemetry.count_request(&FrontendRequest::List);
            // stopped before the minute is over
            telemetry.shutdown().await;

            let telemetry = fx.start(on());
            settle().await;
            assert_eq!(telemetry.status().install_id, Some(id.clone()));
            sleep(HALF_DAY + TICK).await;
            let batches = fx.recorder.batches();
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0]["install_id"], id);
            let day = &batches[0]["days"][0];
            assert_eq!(day["reliability"]["ping_timeouts"], 1);
            assert_eq!(day["usage"]["cli_commands"]["list"], 1);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn turning_off_deletes_everything_and_asks_the_collector_to_forget() {
    let fx = fixture!("turn-off");
    fx.recorder.answer(500);
    fx.recorder.answer(204);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            fx.counters.reconnects.bump();
            sleep(HALF_DAY + TICK).await;
            let id = telemetry.status().install_id.unwrap();
            assert_eq!(telemetry.status().queued_days, 1);

            telemetry.apply(off()).await.unwrap();
            settle().await;
            let last = fx.recorder.sent.borrow().last().cloned().unwrap();
            assert_eq!(last.0.to_string(), ENDPOINT);
            assert_eq!(last.1.method, "DELETE");
            assert_eq!(last.1.route, format!("/v1/installs/{id}"));
            assert_eq!(last.1.body, None);
            assert!(!fx.id_file().exists());
            assert!(!fx.state_file().exists());
            let status = telemetry.status();
            assert_eq!((status.install_id, status.queued_days), (None, 0));
            assert_eq!(telemetry.show().batch, None);

            // nothing is counted or sent while off
            fx.counters.reconnects.bump();
            sleep(2 * DAY).await;
            assert_eq!(fx.recorder.requests().len(), 2);

            // on again: a new install, nothing from before
            telemetry.apply(on()).await.unwrap();
            let new_id = telemetry.status().install_id.unwrap();
            assert_ne!(new_id, id);
            assert_eq!(telemetry.status().queued_days, 0);
        })
        .await;
}

const CRASH_DIR: &str = "MIMOUSE_TEST_CRASH_DIR";
const CRASH_ON: &str = "MIMOUSE_TEST_CRASH_ON";
const SECRET_HOST: &str = "secret-host.example";

/// the child half of `crash_in_child`: records two panics in `CRASH_DIR`,
/// a static one, then one carrying an input event and a host name.
/// Without `CRASH_DIR` (a normal test run) it does nothing
#[test]
fn crash_child() {
    let Some(dir) = std::env::var_os(CRASH_DIR) else {
        return;
    };
    static COUNTERS: Counters = Counters::new();
    let counters = &COUNTERS;
    counters
        .enabled
        .store(std::env::var(CRASH_ON).as_deref() == Ok("1"), Relaxed);
    counters.set_capture(Some(&BackendInUse {
        name: "input-capture-portal".into(),
        auto: true,
    }));
    counters.set_emulation(Some(&BackendInUse {
        name: "libei".into(),
        auto: false,
    }));
    crash::install(PathBuf::from(dir), counters);
    let _ = std::panic::catch_unwind(|| panic!("channel closed"));
    let event = Event::Keyboard(KeyboardEvent::Key {
        time: 987_654,
        key: 30,
        state: 1,
    });
    panic!("unexpected {event:?} from {SECRET_HOST} at 192.168.77.13");
}

/// panics in a separate test process, with telemetry `on` or not, writing
/// crash records to `dir`
fn crash_in_child(dir: &Path, on: bool) {
    let status = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry::tests::crash_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_DIR, dir)
        .env(CRASH_ON, if on { "1" } else { "0" })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the child panics");
}

#[tokio::test(start_paused = true)]
async fn a_crash_while_on_is_uploaded_at_the_next_start() {
    let fx = fixture!("crash-on");
    crash_in_child(&fx.dirs.cache, true);
    assert_eq!(fx.crash_files().len(), 2);
    fx.recorder.answer(204);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            // at the start, not a day later
            let batches = fx.recorder.batches();
            assert_eq!(batches.len(), 1);
            let crashes = batches[0]["crashes"].as_array().unwrap();
            let messages: Vec<_> = crashes
                .iter()
                .map(|c| c["message"].as_str().unwrap())
                .collect();
            assert_eq!(messages, ["channel closed", FORMATTED]);
            for crash in crashes {
                let location = crash["location"].as_str().unwrap();
                assert!(
                    location.starts_with("src/telemetry/tests.rs:"),
                    "{location}"
                );
                assert_eq!(crash["capture_backend"], "input-capture-portal");
                assert_eq!(crash["emulation_backend"], "libei");
            }
            // queued, then sent: the records are gone
            assert!(fx.crash_files().is_empty());
            assert_eq!(telemetry.status().queued_crashes, 0);
            assert!(telemetry.status().local_crashes.is_empty());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_crash_while_off_is_only_shown() {
    let fx = fixture!("crash-off");
    crash_in_child(&fx.dirs.cache, false);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(off());
            settle().await;
            let shown = |status: TelemetryStatus| {
                status
                    .local_crashes
                    .into_iter()
                    .map(|c| c.message)
                    .collect::<Vec<_>>()
            };
            assert_eq!(shown(telemetry.status()), ["channel closed", FORMATTED]);
            assert_eq!(telemetry.show().local_crashes.len(), 2);
            // turning it on later does not send them either
            telemetry.apply(on()).await.unwrap();
            sleep(HALF_DAY + 2 * DAY).await;
            assert!(
                fx.recorder
                    .batches()
                    .iter()
                    .all(|b| b["crashes"] == serde_json::json!([]))
            );
            assert_eq!(shown(telemetry.status()), ["channel closed", FORMATTED]);
            assert_eq!(fx.crash_files().len(), 2);
        })
        .await;
}

/// the next upload, with telemetry configured by a config full of host
/// names, addresses, fingerprints and paths, and a runtime that used them
#[tokio::test(start_paused = true)]
async fn no_host_names_addresses_fingerprints_or_paths_are_sent() {
    let fx = fixture!("pii");
    let fingerprint = "a1:b2:c3:d4:e5:f6:07:18:29:3a:4b:5c:6d:7e:8f:90:a1:b2:c3:d4:e5:f6:07:18:29:3a:4b:5c:6d:7e:8f:90";
    let hook = "/home/alice/bin/secret-hook.sh";
    let config_path = fx.dirs.config.join("config.toml");
    fs::create_dir_all(&fx.dirs.config).unwrap();
    fs::write(
        &config_path,
        format!(
            r#"telemetry = true
telemetry_endpoint = "{ENDPOINT}"
cert_path = "/home/alice/.config/mimouse/secret.pem"

[authorized_fingerprints]
"{fingerprint}" = "alice-laptop"

[[clients]]
hostname = "{SECRET_HOST}"
ips = ["192.168.77.13", "fd00::77"]
port = 4243
position = "left"
enter_hook = "{hook}"
fingerprint = "{fingerprint}"
"#
        ),
    )
    .unwrap();
    let config = Config::at(&config_path).unwrap();
    let settings = Settings::of(&config);
    assert!(settings.enabled && settings.endpoint.is_ok());
    // a crash carrying an input event and a host name
    crash_in_child(&fx.dirs.cache, true);
    fx.recorder.answer(500);
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(settings);
            settle().await;
            let ips = vec![
                "192.168.77.13".parse().unwrap(),
                "fd00::77".parse().unwrap(),
            ];
            let requests = [
                FrontendRequest::AddClient {
                    hostname: Some(SECRET_HOST.into()),
                    port: 4243,
                    ips: ips.clone(),
                    enter_hook: Some(hook.into()),
                },
                FrontendRequest::SetHost {
                    id: 0,
                    hostname: Some(SECRET_HOST.into()),
                },
                FrontendRequest::AuthorizeKey {
                    description: "alice-laptop".into(),
                    fingerprint: fingerprint.into(),
                },
                FrontendRequest::Pair {
                    host: SECRET_HOST.into(),
                    ips,
                    port: 4243,
                    position: Position::Left,
                },
                FrontendRequest::CountCommand {
                    command: SECRET_HOST.into(),
                },
                FrontendRequest::CountCommand {
                    command: "doctor".into(),
                },
            ];
            for request in &requests {
                telemetry.count_request(request);
            }
            fx.counters.set_peers(config.clients().len());
            let mut clipboard = ClipboardData::default();
            clipboard.push(
                "text/plain",
                format!("{SECRET_HOST} {fingerprint}").into_bytes(),
            );
            fx.counters.transfer(&clipboard);
            fx.counters.set_capture(Some(&BackendInUse {
                name: "input-capture-portal".into(),
                auto: false,
            }));
            sleep(HALF_DAY + TICK).await;

            let batch = telemetry.show().batch.expect("a day and crashes queued");
            // what was used is counted
            let parsed: serde_json::Value = serde_json::from_str(&batch).unwrap();
            let day = &parsed["days"][0];
            assert_eq!(
                day["usage"]["cli_commands"],
                serde_json::json!({
                    "add_client": 1, "set_host": 1, "authorize_key": 1, "pair": 1, "doctor": 1
                })
            );
            assert_eq!(day["usage"]["peers"], 1);
            assert_eq!(day["usage"]["clipboard_transfers"]["lt_1k"], 1);
            assert_eq!(parsed["crashes"].as_array().unwrap().len(), 2);
            // the start sent the crashes alone, the day's end everything:
            // nothing identifying in either
            let requests = fx.recorder.requests();
            assert_eq!(requests.len(), 2);
            let sent: Vec<String> = requests
                .iter()
                .map(|r| String::from_utf8(r.body.clone().unwrap()).unwrap())
                .collect();
            assert_eq!(sent[1], batch, "the upload sends exactly what show prints");
            let lower = sent.concat().to_lowercase();
            let event = Event::Keyboard(KeyboardEvent::Key {
                time: 987_654,
                key: 30,
                state: 1,
            });
            for secret in [
                "secret",
                "alice",
                "192.168.77.13",
                "fd00",
                "a1:b2",
                "a1b2",
                "4243",
                "987654",
                "home/",
                ".pem",
                ".sh",
                "keyboard",
                &format!("{event:?}").to_lowercase(),
                &fx.root.to_string_lossy().to_lowercase(),
            ] {
                assert!(!lower.contains(secret), "{secret:?} in {batch}");
            }
        })
        .await;
}

/// runs the collector's validator (`telemetry-collector/src/validate.js`,
/// interpreting `schema.json`) on `batch`
fn collector_accepts(batch: &[u8]) -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("telemetry-collector");
    // one file per call: tests validate in parallel
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Relaxed);
    let file =
        std::env::temp_dir().join(format!("mimouse-batch-{}-{call}.json", std::process::id()));
    fs::write(&file, batch).unwrap();
    const SCRIPT: &str = r#"
        import { readFileSync } from "node:fs";
        import { pathToFileURL } from "node:url";
        const [validator, schema, batch] = process.argv.slice(1);
        const { compileSchema } = await import(pathToFileURL(validator).href);
        const error = compileSchema(JSON.parse(readFileSync(schema, "utf8")))(
            JSON.parse(readFileSync(batch, "utf8")));
        if (error) { console.log(error); process.exitCode = 1; }
    "#;
    let output = Process::new("node")
        .args(["--input-type=module", "-e", SCRIPT])
        .arg(dir.join("src/validate.js"))
        .arg(dir.join("schema.json"))
        .arg(&file)
        .output()
        .map_err(|e| format!("node (v18 or later) runs the collector's validator: {e}"));
    let _ = fs::remove_file(&file);
    let output = output?;
    match output.status.success() {
        true => Ok(()),
        false => Err(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

#[test]
fn collector_validator_rejects_a_broken_batch() {
    let example = include_str!("../../telemetry-collector/example-batch.json");
    assert_eq!(collector_accepts(example.as_bytes()), Ok(()));
    let broken = example.replace(
        "\"schema_version\": 1",
        "\"schema_version\": 1, \"host\": \"x\"",
    );
    assert_ne!(broken, example);
    assert!(collector_accepts(broken.as_bytes()).is_err());
}

/// the largest batch the client sends: 7 days of saturated counters, 64
/// commands a day, 8 crashes with the longest locations and messages, and
/// the nullable fields null
#[tokio::test(start_paused = true)]
async fn the_collector_accepts_what_is_uploaded() {
    let fx = fixture!("schema");
    let full_day = |date: &str| {
        let mut day = Day::new(date.into());
        day.capture = Backend {
            backend: Some("input-capture-portal".into()),
            selection: Some(Selection::Auto),
        };
        let r = &mut day.reliability;
        r.reconnects = MAX_COUNT;
        r.approval_waits.ge_60s = MAX_COUNT;
        day.usage.peers = MAX_PEERS;
        day.usage.file_transfers.ge_64m = MAX_COUNT;
        for i in 0..MAX_COMMANDS + 5 {
            day.count_command(&format!("command_{i}_{}", "x".repeat(20)));
        }
        day
    };
    let state = State {
        today: Day::new("2026-10-13".into()),
        days: (6..=12)
            .map(|d| full_day(&format!("2026-10-{d:02}")))
            .collect(),
        crashes: (0..MAX_CRASHES)
            .map(|i| Crash {
                location: format!(
                    "{}.rs:999999",
                    crash::relative_file(&format!("src/{}{i}.rs", "a".repeat(300)))
                        .trim_end_matches(".rs")
                ),
                message: "é".repeat(256),
                capture_backend: None,
                emulation_backend: Some("libei".into()),
            })
            .collect(),
        last_attempt: None,
        last_error: None,
    };
    // no attempt yet today: the queue goes at the start
    store::save_state(&fx.dirs, &serde_json::to_vec(&state).unwrap()).unwrap();
    LocalSet::new()
        .run_until(async {
            let telemetry = fx.start(on());
            settle().await;
            let requests = fx.recorder.requests();
            assert_eq!(requests.len(), 1);
            let body = requests[0].body.clone().unwrap();
            assert_eq!(
                telemetry.show().batch.as_deref().map(str::as_bytes),
                Some(&*body)
            );
            let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(dates(&batch).len(), MAX_DAYS);
            assert_eq!(batch["crashes"].as_array().unwrap().len(), MAX_CRASHES);
            assert_eq!(
                batch["days"][0]["usage"]["cli_commands"]
                    .as_object()
                    .unwrap()
                    .len(),
                MAX_COMMANDS
            );
            assert_eq!(collector_accepts(&body), Ok(()));
        })
        .await;
}

/// a TLS server for `localhost`; `requests` gets each request's head and
/// body, and the response waits for the status sent on the paired channel
async fn recording_server(
    listener: TcpListener,
    cert: rcgen::CertifiedKey,
    requests: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>, oneshot::Sender<u16>)>,
) {
    use rustls_pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let (tcp, _) = listener.accept().await.unwrap();
        let Ok(mut tls) = acceptor.accept(tcp).await else {
            continue;
        };
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        let end = loop {
            let n = tls.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "request cut short");
            received.extend_from_slice(&buf[..n]);
            if let Some(i) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8(received[..end].to_vec()).unwrap();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        while received.len() < end + length {
            let n = tls.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "body cut short");
            received.extend_from_slice(&buf[..n]);
        }
        let (status_tx, status) = oneshot::channel();
        requests
            .send((head, received[end..].to_vec(), status_tx))
            .unwrap();
        let status = status.await.unwrap();
        let response = format!("HTTP/1.1 {status} Whatever\r\nConnection: close\r\n\r\n");
        tls.write_all(response.as_bytes()).await.unwrap();
        let _ = tls.shutdown().await;
    }
}

/// `cli telemetry show` against a real upload: HTTPS to a local server
/// trusted through the test-only root seam
#[tokio::test(flavor = "current_thread")]
async fn show_is_the_exact_body_the_upload_sends() {
    let fx = fixture!("https");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let https = Https {
        roots: Some(Arc::new(roots)),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let settings = Settings {
        enabled: true,
        endpoint: Endpoint::parse(&format!("https://localhost:{port}/base/")),
    };
    // a completed day waits
    let today = Timestamp::now().to_zoned(TimeZone::UTC).date().to_string();
    let mut day = Day::new("2026-01-01".into());
    day.reliability.reconnects = 2;
    day.count_command("list");
    let state = State {
        today: Day::new(today),
        days: vec![day],
        ..State::default()
    };
    store::save_state(&fx.dirs, &serde_json::to_vec(&state).unwrap()).unwrap();
    LocalSet::new()
        .run_until(async {
            let (requests_tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
            tokio::task::spawn_local(recording_server(listener, cert, requests_tx));
            let telemetry = fx.start_with(settings, Rc::new(https), Rc::new(Timestamp::now));
            let (head, body, respond) = timeout(Duration::from_secs(20), requests.recv())
                .await
                .expect("the upload arrives")
                .unwrap();
            let shown = telemetry.show().batch.expect("the queued day");
            assert_eq!(shown.as_bytes(), &*body);
            assert!(
                head.starts_with("POST /base/v1/batch HTTP/1.1\r\n"),
                "{head}"
            );
            assert!(
                head.contains(&format!("\r\nHost: localhost:{port}\r\n")),
                "{head}"
            );
            assert!(
                head.contains("\r\nContent-Type: application/json\r\n"),
                "{head}"
            );
            let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(dates(&batch), ["2026-01-01"]);
            assert_eq!(batch["days"][0]["usage"]["cli_commands"]["list"], 1);

            respond.send(204).unwrap();
            let cleared = async {
                while telemetry.status().queued_days != 0 {
                    sleep(Duration::from_millis(10)).await;
                }
            };
            timeout(Duration::from_secs(10), cleared)
                .await
                .expect("a 204 clears the queue");
            assert_eq!(telemetry.show().batch, None);
            assert_eq!(telemetry.status().last_error, None);
            telemetry.shutdown().await;
        })
        .await;
}

# MiMouse agent notes

Software KVM: one Rust binary (`mimouse`) runs the service; `mimouse cli` controls it over IPC. No GUI.

## Layout

- `src/`: service, config, DTLS listener/connection, CLI, IPC (`ipc/`), wire format (`proto.rs`), clipboard (`clipboard/`).
  - `config.rs`: `Config::save` is a three-way merge (base snapshot, disk, runtime); never write the config any other way. Writes go through `replace_file` (temp file + rename, symlinks resolved, mode kept) inside `spawn_blocking`; each good load or save refreshes `config.toml.last-good`. Cross-key rules live only in `ConfigToml::validate`; load, reload, the last-good fallback, `check_file` and `write_merged` (disk, merged, and memory when no file exists) all call it. A request that has effects before its save (e.g. `ChangePort` rebinding the listener) checks first through a `Config` helper that calls validate (`Config::check_port`), not a copy of the rule.
  - `clients.rs`: owns the client lifecycle (see Invariants). `client.rs`: `ClientManager`, `match_clients`, `server_policy`/`pin`.
  - `connect.rs`: `Connector` (shared, Clone) owns outgoing connections; the service calls `keep_connected`/`stop` on activate/deactivate/remove. A connect attempt belongs to its client's keeper (`keep_connected`); `stop` cancels in-flight attempts, and an attempt registers its connection only if the keeper's token is not cancelled, checked under the `conns` lock with no await in between. `send` starts an attempt only for a kept client. `MiMouseConnection::recv` yields `ConnEvent::{Received, Closed}`; capture releases on `Closed` of the active client.
  - `crypto/peers.rs`: the one certificate-trust module. `Policy::{Authorized, Pinned, Any}` and the DTLS/TLS client and server configs. Listener, outgoing connections, clipboard, pairing and doctor all verify peers through it; never build a verifier elsewhere. Outgoing connections use `ClientManager::server_policy` and pin on first use with `ClientManager::pin`.
  - `clipboard/`: `wire.rs` (frame v1 = formats, v2 = formats + files; send v1 when no files; v1/v2 bytes must stay unchanged), `files.rs` (copied-file refs, received-file store under the cache dir). `backend.rs`: every platform backend (`portal.rs`, `windows.rs`) implements `Backend::session` and reports and receives only through `Context`, which keeps the backend rules (module doc; its tests are the clipboard contract suite); `backend::spawn` runs sessions, `End::Retry` after `RETRY_DELAY`, `End::Final` ends the backend. A clear is reported only when the owner offers no format at all (`Context::cleared`, see `Offer`); secret-marked copies and copies without a supported format are reported as not shared, never read or sent. A clear is a v1 frame with zero items. The backend set channel is a latest-wins `watch` (`SetRx`); contents received during a retry wait stay in it for the next session; the backend ending = `SetRx` dropped (`BackendTx::closed`). A `Snapshot` is encoded once and shared by all client senders. `wire::read` streams file payloads to a `FileSink` (`files::Receiving`) in `wire::CHUNK` pieces; transfers being received are never pruned. Inbound admission: `MAX_PEER_TRANSFERS` (1) per peer fingerprint, `MAX_TRANSFERS` overall, both waiting rather than refused.
  - `inhibit.rs` (idle/lock inhibit), `notify.rs` (switch and approval notices), `approval.rs` (portal approval waits).
  - `pair.rs` (pairing), `discover.rs` (`cli discover`, `tailscale status --json` only, never mDNS), `doctor.rs` (`cli doctor`; the ufw/firewalld/netsh/socketfilterfw parsers are pure functions with tests). External commands (doctor/discover/pair) go through `doctor::run_command` (timeout + `kill_on_drop`, `COMMAND_TIMEOUT`); never call `Command::new(...).output()` directly.
  - `telemetry/`: off by default. Counters are relaxed atomics in `telemetry::COUNTERS`, bumped with a single line where events happen (any thread); collection and upload never block the LocalSet or the input path (file IO via `spawn_blocking`, the upload in its own task). Never send host names, addresses, fingerprints, paths, config values or formatted panic messages.
- `telemetry-collector/`: `schema.json` is the telemetry contract; the Worker validates against it at runtime and rejects unknown fields. Adding a field means bumping `schema_version` (`telemetry/schema.rs`), the README field list and the client's no-PII test (`telemetry::tests::no_host_names_addresses_fingerprints_or_paths_are_sent`) together.
- `input-capture/`: OS events → `Stream<CaptureEvent>` (libei, layer-shell, windows, macos). `chord.rs`: `ChordMatcher` (exact chord only; supersets and auto-repeat never fire) for the release bind and hotkeys; `feed` returns `Fed::{Pass, Fired, Swallowed}`, and `swallow(key)` drops a fired chord's final key, auto-repeat and key-up included, until it goes up. It is the one state machine for `src/capture.rs` and the Windows/macOS local hotkeys. `InputCapture::with_backend` injects a backend (tests).
  - `Capture::focus` starts capture without a crossing; backends that can't (libei, layer-shell, x11) return `FocusByPointerPush` and `src/capture.rs` asks emulation to push the local pointer through the barrier. Edge ranges (`EdgeRange`) are applied by each backend (barrier/surface length or activation check).
  - `InputCapture::create` takes `enter_only`: when a normal (Default) client shares the position, enter-only ids (inbound connections) do not shape the barrier and get no Input events, only Begin and End. An enter-only id alone at a position keeps the whole edge.
  - `InputCapture` alone owns pending-queue hygiene: `release()` drops queued Input for every id. Windows/macOS OS callbacks send through `event_queue` (unbounded, never blocks or panics; over the limit drops presses/motion/crossings but always queues releases, Modifiers and End). libei batches queued client updates (`apply_queued`) and restarts the session only when the client list changes (mutter AddBarrier needs INIT state).
- `input-emulation/`: replays events via the `Emulation` trait (libei, wlroots, remote-desktop portal, x11, windows, macos). `Emulation::enter_at` places the pointer for `ProtoEvent::EnterAt` via `geometry::entry_point` (pure, tested); return `None` only when the backend can't place absolutely. macOS key logic is in `macos_logic.rs` and the scroll sign in `input-event/src/macos_scroll.rs`; both are platform-independent and unit-tested, so change them rather than the CoreGraphics glue.
  - `InputEmulation` passes every peer event through `normalize.rs` (`Peer::plan`) before a backend sees it: out-of-protocol values dropped, held keys/buttons tracked per handle and released on `destroy`/`terminate`, motion/scroll remainders kept per handle in the backend's `Emulation::steps`. `keys.rs`: evdev-keyed key-repeat planner shared by Windows and macOS. `contract.rs` (tests): the contract suite every CI-runnable backend passes, fed with wire traces from `tests/fixtures/emulation/*.trace`; add a backend to `SUBJECTS` and a trace to `TRACES`.
  - wlroots keeps one seat `wl_keyboard` (`State::keyboard`) and uploads the keymap to its virtual keyboards only when the keymap's contents change. wlroots broadcasts the keymap to every client whenever any keyboard sets one, a virtual keyboard's upload included, so re-uploading an unchanged keymap loops and floods the connection; `NO_KEYMAP` events are ignored (uploading one ends the connection). Under backpressure wlroots discards motion and presses but never a key-up or button-up: a release waits for the compositor to read (`drain`, at most `RELEASE_TIMEOUT`), so Ok from `consume` means it was sent.
- `input-event/`: shared scancodes and event types; extend here, don't duplicate translations.
- `vendor/webrtc-util`: 0.11.0 with webrtc-rs/webrtc#812 (Windows `WSAECONNRESET` fix) and listener limits: `ListenConfig::{max_conns, conn_buffer_size, idle_timeout}`, and `UdpConn::close` forgets its remote. Its tests are outside the workspace.
- `packaging/`: Scoop, winget (schema 1.10.0) and Homebrew manifests holding the Cargo.toml version and `@SHA256_*@` placeholders; `packaging/render-manifests.py` fills the hashes and validates against the official schemas in the release workflow. Bump the version here with Cargo.toml (the release fails otherwise). `.deb`/`.rpm` metadata is in Cargo.toml `[package.metadata.deb]` / `[package.metadata.generate-rpm]`; the rpm requires list is the binary's NEEDED sonames (`readelf -d`), so update it when a new system library gets linked.

## Invariants

- A peer is either sending or receiving, never both; this prevents feedback loops and indirect control through a third machine.
- Input: UDP/DTLS. Clipboard: TCP/TLS on the same port, same certificate, same `authorized_fingerprints`.
- IPC (`ipc/`): newline-delimited JSON; requests carry a `seq` and get exactly one `Reply`. An undecodable request gets an error `Reply` when its `seq` can be read; otherwise the connection is closed. Each connection has a bounded outbound queue (`QUEUE_LEN`) drained by its own writer task; `AsyncFrontendListener::send` never waits and drops a connection whose queue is full (the frontend reconnects and gets the sync events). The CLI waits at most `REPLY_TIMEOUT` for a reply (`pair` and `watch` are exempt). Windows uses a per-user named pipe with an owner-only DACL, never a TCP port. State-changing requests are audited (`audit.rs`, written on its own thread; the log keeps recording when rotation fails, appending to the current `audit.jsonl`); never log typed text or clipboard contents. Never do file IO on the service's LocalSet.
- New `FrontendEvent` fields must be `Option` with `#[serde(default)]` so older/newer CLI-service pairs still decode.
- Client lifecycle (active <=> capture barrier <=> kept connection <=> armed hotkey) lives only in `src/clients.rs` (`Clients::apply`); every client change is planned, saved, then applied (`Clients::change`, `change_keys`), so a failed save changes nothing; pairing applies first and reports `not_saved`. Client handles increase monotonically within a run (never reused); reloads keep matched clients' handles (`client::match_clients`). Peers controlling this machine are tracked in one map `Service::incoming` keyed by address. A config watcher error is logged and ignored (release is `panic = "abort"`).
- Restart-only config keys (see the config table) read `Config`'s `started` snapshot, never the reloaded `config_toml`; add a new restart-only key to both `Config::report` and `Config::awaiting_restart`.
- Pairing: TLS on the MiMouse TCP port with ALPN `mimouse-pair/1`, accepted from unknown fingerprints only while `pair --accept` is waiting. The code commits to the initiator's nonce before the acceptor's is known; trust is written only after both users confirm. The TCP listener runs even with clipboard sharing off.
- Revocation: the listener (`listen.rs`, per datagram) and clipboard receive (`transport.rs`, every `REVOCATION_CHECK`, and again before the ACK) re-check each open connection's stored fingerprint with `crypto::peers::Policy::check` against the shared `authorized_keys` map. Code that changes the keys must mutate that map in place (never swap the Arc), or open connections won't see the revocation. The TCP listener holds at most `MAX_CONNECTIONS` live connections, pairing included (`pair::Incoming` carries the permit); accept errors back off like `ipc::listen`. Unauthorized-attempt notices: one per `UNAUTHORIZED_INTERVAL` globally, with a count of the ones held back.
- Wire-format changes break compatibility with Lan Mouse peers. `ProtoEvent::EnterAt` is an `Enter` datagram plus two trailing bytes that Lan Mouse ignores; decode by datagram length (`ProtoEvent::decode`), never from the whole receive buffer. `ProtoEvent::decode` rejects button states and scroll axes other than 0/1, so backends can convert them without failing.
- Capture and emulation track pressed keys and release them on disconnect (`terminate()`). Every capture end (release bind, hotkey, compositor/portal deactivation, error, send failure, client removed) goes through `leave_client` (via `release_capture` unless the backend already ended capture), which sends key-ups. `ICaptureEvent::ClientEntered` is emitted only after the first event reached the client, and `Released` only for an announced entry; `Service::control_switched` is the single switch hook (notifier + idle inhibit), so a new release path that bypasses `leave_client` leaves keys stuck and the inhibitor held.
- Emulation recovery (`src/emulation.rs` `recovery`): the backend is recreated only on `InputEmulationError::is_transient` errors (EndOfStream, portal not on the bus, NoAvailableBackend, wlroots compositor loss, roundtrip timeout and a stalled compositor (`EmulationError::Stalled`), X11 display not open), with `connect::Backoff`; after `cancelled_by_user()` it waits for `enable-emulation` so a denied dialog is not shown again. Auto selection stops at a backend whose desktop runs but is not ready (`EmulationCreationError::not_ready`: a wlroots seat without a keyboard) and returns its transient error, so it is retried rather than replaced by X11 on Xwayland. Dummy is never auto-selected and never logs key codes.
- Timing relations are `const _: () = assert!(..)` next to the constants (`emulation.rs`, `connect.rs`, `pair.rs`); values are in the Tunables tables. DTLS listener: the UDP idle timeout must stay above the handshake and peer idle timeouts, because closing a UDP conn under a live DTLS conn makes the webrtc-dtls reader task spin.
- Clipboard: (1) `transport::receive` hands an update to the clipboard task before ACKing. The task drops its receiver only when the backend's set channel closes (backend ended), so updates are ACKed while the portal dialog is pending and refused afterwards. (2) Backends report `LocalChange::Changed(at, ..)` with the time the change was noticed; inbound updates accepted (TCP accept `Instant`) before the last local change are dropped, because frames carry no timestamp. The task checks this, and each backend checks it again against the change it noticed itself (`Context::noticed` records it in `SetRx` and returns the `Noticed` token every report needs), which the task may not have heard of yet. (3) Startup contents (`Context::startup` → `LocalChange::Startup`) seed `current` and are never sent; a portal session restart announcing the copy already held is not a startup. (4) Portal sessions run inside `closing()`, which closes the session on every exit path; only a denied dialog, a missing grant and a frontend without the RemoteDesktop portal (`PortalNotFound`) end the backend (`End::Final`), every other portal error (GNOME's `Session creation inhibited` included) retries. (5) IPC event `clipboard_status` {status, reason?} is part of the sync events after `capture`. (6) Clipboard TOFU pins a server's fingerprint only after that server ACKed the update.
- Windows: injected mouse moves tagged `input_capture::IDLE_KEEPALIVE_TAG` must pass the LL hook. Logoff and shutdown reach the service only as WM_QUERYENDSESSION/WM_ENDSESSION on the hidden top-level window `MiMouseEndSession` (`service::end_session`), because a process that loads user32 never gets CTRL_LOGOFF/CTRL_SHUTDOWN; WM_ENDSESSION blocks until `Shutdown::done`, at most `WAIT`.
- Received file names are base names validated by `files::valid_name`; never join an unvalidated peer name onto a path.
- Telemetry payload is exactly `telemetry-collector/schema.json`; the `telemetry/schema.rs` limits equal the schema's.
- `mimouse.pem` stays owner-only (0600 / protected DACL, `crypto.rs`). Config write-backs edit `config.toml` in place with `toml_edit`; never re-serialize the whole file.
- macOS: only `macos_permissions` shows permission prompts (once, at service start); backends check silently.

## Tunables

Constants stay compiled `const`: they sit on the input path or are read once, so there is no runtime lookup. Scale: `log` = tuned by factors (times, sizes), `linear` = small counts. "Range" is recorded only where a reason exists; otherwise "unknown".

### Constants

| Constant | file:line | Value | Scale | Range | Trades off |
|---|---|---|---|---|---|
| `PING_INTERVAL` | src/connect.rs:493 | 500 ms | log | < `WATCHDOG_TIMEOUT`/2 (assert src/emulation.rs:230); Lan Mouse pings every 500 ms | dead-peer detection latency vs. ping traffic |
| `MAX_MISSED_PINGS` | src/connect.rs:494 | 8 (= 4 s) | linear | unknown; 4 s rides out loss bursts (lan-mouse#307) | loss tolerance vs. time until a dead peer is released |
| `DEFAULT_CONNECTION_TIMEOUT` | src/connect.rs:44 | 5 s | log | < `HANDSHAKE_TIMEOUT` (comment src/listen.rs:45) | slow links vs. reconnect start |
| `Backoff::MIN` / `MAX` | src/connect.rs:216-217 | 1 s → 30 s, doubling | log | MIN <= MAX (assert src/connect.rs:230) | reconnect / emulation-retry speed vs. retry load |
| `WATCHDOG_TIMEOUT` | src/emulation.rs:227 | 3 s | log | > 2 × `PING_INTERVAL` (assert src/emulation.rs:230) | keys held after a peer vanishes vs. releasing on short loss |
| `WATCHDOG_TICK` | src/emulation.rs:229 | 500 ms | log | < `WATCHDOG_TIMEOUT` (assert src/emulation.rs:234) | release precision (max hold = timeout + tick) vs. wakeups |
| `REJECTION_REPEAT_WINDOW` | src/emulation.rs:261 | 2 s | log | unknown | log noise vs. reporting repeated refusals |
| `MAX_RECENT_REJECTIONS` | src/emulation.rs:264 | 64 | log | unknown | memory vs. dedup of refused fingerprints |
| `ENTER_REPEAT_WINDOW` | src/emulation.rs:610 | 1 s | log | unknown | merging `Enter` repeats into one crossing vs. missing a real re-entry |
| `HANDSHAKE_TIMEOUT` (DTLS) | src/listen.rs:46 | 10 s | log | > client `DEFAULT_CONNECTION_TIMEOUT` 5 s; < `UDP_IDLE_TIMEOUT` | stalled handshake slots vs. slow peers |
| `PEER_IDLE_TIMEOUT` | src/listen.rs:48 | 10 s | log | > peer ping interval 500 ms; < `UDP_IDLE_TIMEOUT` | resources of silent peers vs. dropping live ones |
| `UDP_IDLE_TIMEOUT` | src/listen.rs:52 | 30 s | log | > both DTLS timeouts (DTLS reader spin) | memory for idle remotes |
| `FOCUS_TIMEOUT` | src/capture.rs:765 | 1 s | log | < `REPLY_TIMEOUT` (comment src/cli.rs:54) | `cli focus` failure latency vs. slow capture start |
| `CtrlDoubleTap::MAX_HOLD` | src/capture.rs:869 | 300 ms | log | unknown | tap detection vs. ordinary Ctrl use |
| `CtrlDoubleTap::MAX_GAP` | src/capture.rs:871 | 400 ms | log | unknown | same |
| send-failure log debounce `DUR` | src/capture.rs:555 | 500 ms | log | unknown | log noise |
| `REPLY_TIMEOUT` | src/cli.rs:56 | 5 s | log | > `FOCUS_TIMEOUT` | CLI hang vs. slow service |
| `COUNT_TIMEOUT` | src/cli.rs:266 | 500 ms | log | unknown | `doctor`/`discover` start delay vs. counting the command |
| `GRACE` | src/approval.rs:25 | 2 s | log | unknown; a portal restoring a saved grant answers within it | false "waiting for approval" reports vs. late ones |
| `WARN_EVERY` | src/approval.rs:27 | 30 s | log | unknown | reminders vs. log noise |
| `RELEASE_DELAY` | src/inhibit.rs:15 | 5 s | log | unknown | inhibitor churn vs. lock delay after control returns |
| `KEEPALIVE` (Windows) | src/inhibit.rs:351 | 30 s | log | unknown; must be below the OS idle timeout | wakeups vs. idle timer reset |
| `UNAUTHORIZED_INTERVAL` | src/notify.rs:15 | 60 s | log | unknown | notice spam vs. awareness |
| `APPROVAL_INTERVAL` | src/notify.rs:17 | 300 s | log | unknown | same, approval waits |
| `EXPIRE_MS` (Linux notice) | src/notify.rs:224 | 3000 ms | log | unknown | notice visibility |
| `MAX_QUEUED` (macOS notices) | src/notify.rs:442 | 4 | linear | unknown | dropped notices vs. backlog |
| `MAX_WINDOW` | src/pair.rs:47 | 3600 s | log | >= `CONFIRM_TIMEOUT` (assert src/pair.rs:56); `pair --accept --timeout` (default 120, src/cli.rs:99) must be 1..=it | exposure of the accept window |
| `MAX_FRAME` / `MAX_NAME` / `MAX_REASON` | src/pair.rs:48-50 | 4096 / 253 / 512 B | log | unknown | pairing message limits |
| `CONNECT_TIMEOUT` (pair) | src/pair.rs:51 | 3 s | log | unknown | |
| `FRAME_TIMEOUT` | src/pair.rs:52 | 10 s | log | < `CONFIRM_TIMEOUT` (assert src/pair.rs:55) | |
| `CONFIRM_TIMEOUT` | src/pair.rs:54 | 120 s | log | <= `MAX_WINDOW` (assert src/pair.rs:56) | time to compare codes vs. open session |
| `PROBE_TIMEOUT` (discover) | src/discover.rs:25 | 1.5 s | log | unknown | |
| `IPC_TIMEOUT` / `PROBE_TIMEOUT` (doctor) | src/doctor.rs:37-38 | 2 s / 3 s | log | unknown | |
| `COMMAND_TIMEOUT` | src/doctor.rs:41 | 10 s | log | unknown; `netsh` with hundreds of rules takes seconds | hung tools vs. slow firewall listings |
| `QUEUE_LEN` (IPC) | src/ipc/listen.rs:30 | 64 events | log | unknown | memory vs. disconnecting slow frontends |
| IPC/TCP accept backoff | src/ipc/listen.rs:234, src/clipboard/transport.rs:381 | 10 ms doubling to 1 s (literals) | log | unknown | spin at fd limit vs. accept latency |
| `MAX_SIZE` / `KEEP` (audit) | src/audit.rs:17-18 | 10 MiB / 3 rotations | log / linear | unknown | disk use vs. history |
| `MAX_CLIPBOARD_SIZE` | src/clipboard/mod.rs:70 | 32 MiB (all formats) | log | unknown | large images vs. memory (`MAX_TRANSFERS` × it) |
| `MAX_NAME_LEN` | src/clipboard/files.rs:19 | 255 B | — | = Linux `NAME_MAX` | |
| `KEEP_TRANSFERS` | src/clipboard/files.rs:21 | 3 | linear | unknown | disk use vs. pasting older transfers |
| `CONNECT_TIMEOUT` / `HANDSHAKE_TIMEOUT` (clipboard) | src/clipboard/transport.rs:50-51 | 3 s / 5 s | log | unknown | |
| `TRANSFER_TIMEOUT` + `MIN_TRANSFER_RATE` | src/clipboard/transport.rs:52,54 | 60 s + 1 s per MiB | log | unknown | slow links vs. stuck transfers |
| `MAX_CONNECTIONS` | src/clipboard/transport.rs:58 | 16 | log | unknown | DoS resistance vs. many peers |
| `MAX_TRANSFERS` / `MAX_PEER_TRANSFERS` | src/clipboard/transport.rs:62,65 | 4 / 1 | linear | unknown; memory bound 4 × 32 MiB | parallelism vs. memory |
| `REVOCATION_CHECK` | src/clipboard/transport.rs:67 | 500 ms | log | unknown | revocation latency vs. wakeups |
| `CHUNK` | src/clipboard/wire.rs:40 | 256 KiB | log | unknown | memory vs. syscalls |
| `MAX_ITEMS` / `MAX_MIME_LEN` / `MAX_FILES` | src/clipboard/wire.rs:36-38 | 255 / 255 / 65535 | — | fixed by wire field widths (u8/u8/u16) | |
| `RETRY_DELAY` (clipboard backends) | src/clipboard/backend.rs:37 | 10 s | log | unknown | recovery after a closed session or refused portal vs. retry load |
| `IO_TIMEOUT` (portal clipboard) | src/clipboard/portal.rs:61 | 5 s | log | unknown | |
| `RESTORE_WINDOW` | src/clipboard/portal.rs:68 | 300 ms | log | unknown; mutter restores within the same emission or 10 ms | dropping secret restores vs. clear latency |
| `STARTUP_WINDOW` | src/clipboard/portal.rs:73 | 1 s | log | unknown; mutter announces within the same second (GNOME 50 smoke) | |
| `OPEN_ATTEMPTS` × `OPEN_RETRY_DELAY` (Windows) | src/clipboard/windows.rs:38-39 | 20 × 25 ms | linear / log | unknown | |
| `READ_ATTEMPTS` × `READ_RETRY_DELAY` (Windows) | src/clipboard/windows.rs:43-44 | 10 × 50 ms | linear / log | unknown; covers OS error 1418 while the owner renders | |
| `SETTLE_DELAY` × `SETTLE_ROUNDS` (Windows) | src/clipboard/windows.rs:48-49 | 100 ms × 10 | log / linear | unknown | breaking multi-step writers vs. latency |
| `TICK` (telemetry) | src/telemetry/mod.rs:44 | 60 s | log | unknown | persist/upload granularity |
| `TIMEOUT` / `MAX_STATUS_LINE` (telemetry HTTP) | src/telemetry/http.rs:18,20 | 10 s / 1024 B | log | unknown | |
| `MAX_DAYS` / `MAX_CRASHES` / `MAX_COUNT` / `MAX_PEERS` / `MAX_COMMANDS` / `MAX_MESSAGE` | src/telemetry/schema.rs:10-19 | 7 / 8 / 1e9 / 10000 / 64 / 256 chars | — | = `telemetry-collector/schema.json` | |
| event queue limit (Windows / macOS) | input-capture/src/windows.rs:61, input-capture/src/macos.rs:833 | 10 / 32 events (literals) | log | unknown | dropped motion vs. latency |
| `EDGE_INSET` | input-emulation/src/geometry.rs:40 | 1 px | linear | >= 1 (local capture sits on the outermost pixel) | |
| pointer push `STEPS` × `STEP` | input-emulation/src/lib.rs:225,227 | 3 × 16 px | linear | `STEP` > `EDGE_INSET` | |
| `DEFAULT_REPEAT_DELAY` / `DEFAULT_REPEAT_INTERVAL` | input-emulation/src/windows.rs:34-35, input-emulation/src/macos.rs:28-29 | 500 ms / 32 ms | log | unknown | key repeat feel |
| `DOUBLE_CLICK_INTERVAL` (macOS) | input-emulation/src/macos.rs:30 | 500 ms | log | unknown | |
| `REFUSED_WARN_INTERVAL` (Windows) | input-emulation/src/windows.rs:181 | 10 s | log | unknown | log noise |
| `STARTUP_TIMEOUT` (wlroots) | input-emulation/src/wlroots.rs:49 | 5 s | log | unknown | |
| `RELEASE_TIMEOUT` (wlroots) | input-emulation/src/wlroots.rs:55 | 1 s | log | unknown; bounds shutdown, which awaits the emulation task | a slow compositor's dropped release vs. a stalled backend restart |
| `WAIT` (Windows end session) | src/service.rs:978 | 2 s | log | unknown; Windows may end the process once WM_ENDSESSION returns | cleanup (key-ups to the peer) at logoff/shutdown vs. holding up the session end |
| `MAX_CLICKS` (X11 scroll) | input-emulation/src/x11.rs:22 | 16 per event | log | unknown | hostile values vs. fast scrolling |

### `config.toml` keys

Struct `ConfigToml` / `TomlClient` (src/config.rs), example in `config.toml`. Legal ranges come from `ConfigToml::validate` (src/config.rs:97). Reload class from `Config::report` (src/config.rs:754) / `Config::awaiting_restart` (src/config.rs:858); live keys are applied in `Service::handle_config_change` (src/service.rs:538) or `Clients::reload`.

| Key | file:line | Default | Range | Reload |
|---|---|---|---|---|
| `capture_backend` | src/config.rs:53 | auto | backend names | restart (flag wins) |
| `emulation_backend` | src/config.rs:54 | auto | backend names | restart (flag wins) |
| `port` | src/config.rs:55 | 4242 (`DEFAULT_PORT` src/ipc/mod.rs:66) | 1..=65535 | restart; `cli change-port` is live |
| `release_bind` | src/config.rs:56 | Ctrl+Shift+Super+Alt (src/config.rs:510) | no client `hotkey` equal | live |
| `release_double_tap_ctrl` | src/config.rs:57 | true | bool | live |
| `cert_path` | src/config.rs:58 | `<config dir>/mimouse.pem` | path | restart (flag wins) |
| `clients` | src/config.rs:59 | none | see client keys | live |
| `authorized_fingerprints` | src/config.rs:60 | none | | live |
| `clipboard` | src/config.rs:61 | false | bool | restart |
| `max_file_size` | src/config.rs:62 | 100 MiB (src/config.rs:514) | >= 1 byte | restart |
| `notify_switch` | src/config.rs:63 | true | bool | live |
| `idle_inhibit` | src/config.rs:64 | true | bool | live |
| `local_hotkey` | src/config.rs:65 | none | no client `hotkey` equal | live |
| `telemetry` | src/config.rs:66 | false | bool | live |
| `telemetry_endpoint` | src/config.rs:67 | built-in (release builds only) | empty or `https://` URL | live |
| client `hostname` | src/config.rs:72 | none | | live |
| client `host_name` | src/config.rs:73 | none | only names the client in validation errors | live |
| client `ips` | src/config.rs:74 | none | | live |
| client `port` | src/config.rs:75 | 4242 | 1..=65535 | live |
| client `position` | src/config.rs:76 | `left` | left/right/top/bottom | live |
| client `activate_on_startup` | src/config.rs:77 | false | bool | live |
| client `enter_hook` | src/config.rs:78 | none | shell command | live |
| client `range` | src/config.rs:79 | whole edge | start < end <= 100 (`EdgeRange::new`) | live |
| client `hotkey` | src/config.rs:80 | none | unique among clients, `local_hotkey`, `release_bind` (chords compare as key sets) | live |
| client `fingerprint` | src/config.rs:81 | none (pinned on first use) | | live |

## Persistent state

Config dir: Linux/macOS `$XDG_CONFIG_HOME/mimouse/` else `~/.config/mimouse/`; Windows `%LOCALAPPDATA%\mimouse\` (src/config.rs:34). Cache dir (`files::cache_dir`, src/clipboard/files.rs:180): Linux `$XDG_CACHE_HOME` else `~/.cache`; Windows `%LOCALAPPDATA%`; macOS `~/Library/Caches`; MiMouse uses its `mimouse/` subdir. On Windows both resolve to `%LOCALAPPDATA%\mimouse\`. In-memory state is not listed here.

| File | Path | Writer | Created / rewritten / deleted | Permissions | Read by |
|---|---|---|---|---|---|
| `config.toml` | config dir | `config.rs` | created with defaults on first `Config` load (service, CLI); rewritten by `Config::save` (`replace_file`); never deleted | creation umask/default ACL; rewrites keep the mode | service (load + watcher), CLI, `doctor` |
| `config.toml.last-good` | next to `config.toml` | `config.rs` `write_last_good` | after every valid load or save, if the text changed; never deleted | owner-only (`create_private`) | startup fallback when `config.toml` is invalid; `doctor`; restore command |
| `mimouse.pem` | `cert_path` (default config dir) | `crypto.rs` | generated if missing at service start; never rewritten or deleted; loose permissions repaired at start | 0600 / protected DACL | service (DTLS/TLS identity), `doctor` |
| `audit.jsonl`, `audit.{1,2,3}.jsonl` | config dir | `audit.rs` (own thread) | appended per state-changing IPC request; rotated at 10 MiB, `audit.3.jsonl` overwritten | 0600 on Unix, default ACL on Windows | humans |
| `input-capture.token` | `$XDG_CACHE_HOME`/`~/.cache` + `/mimouse/` (Linux) | `input-capture/src/libei.rs` | written when the InputCapture portal returns a restore token; never deleted | umask | libei capture at session start |
| `remote-desktop.token` | same dir (Linux) | `input-emulation/src/libei.rs` | written when the RemoteDesktop portal returns a restore token; never deleted | umask | libei emulation at session start |
| `clipboard.token` | `<cache dir>/mimouse/` (Linux) | `src/clipboard/portal.rs` | written when the clipboard portal session returns a restore token; never deleted | umask | portal clipboard at session start |
| received files `files/<id>/` | `<cache dir>/mimouse/files/` | `clipboard/files.rs` `FileStore` | one dir per inbound transfer (id = ms timestamp); a dropped unfinished transfer removes its dir; after each stored transfer all but the newest `KEEP_TRANSFERS` complete ones are removed | dirs 0700 on Unix | local clipboard (file URIs offered to the OS) |
| `telemetry-id` | config dir | `telemetry/store.rs` | created (UUID v4) when telemetry turns on; deleted by `telemetry off` | owner-only | telemetry upload, `cli telemetry status` |
| `telemetry.json` | `<cache dir>/mimouse/` (config dir if no cache dir) | `telemetry/store.rs` | rewritten every `TICK` when changed and at shutdown while on; deleted by `telemetry off` | owner-only | telemetry at start/turn-on, `cli telemetry show/status` |
| `crash-<ms>-<pid>-<seq>.json` | same dir as `telemetry.json` | `telemetry/crash.rs` panic hook | one per panic, telemetry on or off; uploadable ones are moved into `telemetry.json` and deleted when telemetry turns on, and deleted by `telemetry off`; local ones beyond the newest 8 are deleted when listed | 0600 on Unix | telemetry, `doctor`, `cli telemetry show` |
| IPC socket `mimouse-socket.sock` (runtime) | Linux `$XDG_RUNTIME_DIR/`, macOS `~/Library/Caches/`; Windows named pipe | `ipc/listen.rs` | bound at service start, removed on drop | | CLI |

## Conventions

- Backends are cargo features in the root `Cargo.toml`; gate modules with the cfgs emitted by `build.rs`.
  `build.rs` reads the host, so cross-checking needs `--no-default-features`.
- `build.rs` embeds a VERSIONINFO resource (winresource) for Windows targets; keep it, a resource-less exe was quarantined by Defender.
- Tokio single-threaded runtime (`LocalSet`), `futures` streams, `async_trait`; no blocking in async code.
- Errors via `thiserror`. Comments only for non-obvious invariants.
- Timing tests: pure state machines take `now` (`MissedPings`, `Backoff`, `CtrlDoubleTap`, `Debounce`). Task-level timing uses `#[tokio::test(start_paused = true)]` (ping_pong over webrtc_util `conn_pipe`, capture focus timeout via `TestBackend`). Tests doing DTLS on loopback sockets stay on real time. Tests that need a listener use `MiMouseListener::on_free_port` (never pick a port by binding :0 and dropping it).
- Service-level tests build a `Service` with `Service::assemble` on Dummy backends in a temp dir (`service::tests::service`), using `AsyncFrontendListener::detached` and `Telemetry::for_test`; they never touch the user's IPC socket, cache dir or certificate.
- Warnings are errors (CI `-D warnings`); an `#[allow]` needs a one-line reason.
- Update README.md when changing user-visible behavior or platform support; user-visible changes get a CHANGELOG line under the unreleased version.

## Commands

```sh
cargo fmt && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo clippy --release --target x86_64-pc-windows-gnu --no-default-features -- -D warnings
cargo deny check licenses bans sources   # license allow-list in deny.toml; a new license needs a GPL-3.0 compatibility check
cd telemetry-collector && npm ci && npm test   # collector tests
MIMOUSE_LOG_LEVEL=debug cargo run
```

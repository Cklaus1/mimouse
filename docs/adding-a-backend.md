# Adding a backend

MiMouse talks to each desktop through three kinds of backend: input capture (sending), input emulation (receiving) and the clipboard. Backends are compiled into the one `mimouse` binary; there are no runtime plugins, because:

- a capture backend sees every keystroke, so loading third-party code at runtime would be a keylogger slot;
- macOS accessibility permissions and Windows Defender's trust attach to one binary, and a plugin would have to be trusted separately;
- the input-path invariants in `AGENTS.md` (keys released on every exit, one control direction per peer) are kept in-process, by shared layers every backend runs under;
- Rust has no stable ABI.

A backend implements one trait, runs under a shared layer that keeps the cross-platform rules, and passes that layer's contract suite. Read `AGENTS.md` first (layout, invariants, conventions) and `CONTRIBUTING.md` (gates).

| Kind | Code | Trait | Shared layer | Contract suite | Existing backends |
|---|---|---|---|---|---|
| Capture | `input-capture/src/<name>.rs` | `Capture` (`input-capture/src/lib.rs`) | `InputCapture` | `input-capture/src/contract.rs` | libei, layer_shell, x11, windows, macos, dummy |
| Emulation | `input-emulation/src/<name>.rs` | `Emulation` (`input-emulation/src/lib.rs`) | `InputEmulation` + `normalize.rs` | `input-emulation/src/contract.rs` | wlroots, libei, xdg_desktop_portal, x11, windows, macos, dummy |
| Clipboard | `src/clipboard/<name>.rs` | `backend::Backend` (`src/clipboard/backend.rs`) | `backend::Context` + `backend::spawn` | tests in `src/clipboard/backend.rs` | portal (Linux), windows |

## Steps for every kind

1. **Gate the code.** Windows and macOS backends use `#[cfg(windows)]` / `#[cfg(target_os = "macos")]` and need no feature. A Linux/BSD backend is a cargo feature:
   - in the crate's `Cargo.toml`, a feature that enables its optional dependencies (`x11 = ["dep:x11"]`);
   - in the crate's `build.rs`, emit `cargo::rustc-check-cfg=cfg(<name>)` and, when `unix && !macos && feature`, `cargo::rustc-cfg=<name>`; gate the module with `#[cfg(<name>)]`;
   - in the root `Cargo.toml`, a feature forwarding to it (`x11_capture = ["input-capture/x11"]`), added to `default` if it ships, and the matching cfg in the root `build.rs`.

   `build.rs` reads the host, so cross-checks use `--no-default-features`.
2. **Register it.** In the crate's `lib.rs`: a `Backend` variant and its `Display` name, an arm in `create_backend`, and a place in `AUTO_BACKENDS`. The order there is the preference order; Dummy is never auto-selected. In `src/config.rs`: a variant of `CaptureBackend` / `EmulationBackend` whose serde name is the name users type in `capture_backend` / `emulation_backend` and `--capture-backend` / `--emulation-backend`, and its arm in the `From` conversion. (Clipboard backends have no selection; see below.)
3. **Telemetry.** The backend names are a closed list: `CAPTURE_BACKENDS` / `EMULATION_BACKENDS` in `src/telemetry/counters.rs` and the matching `enum` in `telemetry-collector/schema.json`. The collector rejects unknown values, so change both together, following the schema rules in `AGENTS.md` (`schema_version`, README field list).
4. **Classify errors.** Errors are `thiserror` enums in the crate's `error.rs`. The service decides what to do from them:
   - Emulation: `is_transient` means the backend is recreated with backoff (the portal is not on the bus yet, the compositor went away). `not_ready` means the desktop runs but cannot be used yet, and auto selection stops there instead of falling through to another backend. `cancelled_by_user` means the user denied a dialog, and the service waits for `cli enable-emulation`.
   - Capture: `cancelled_by_user` stops auto selection, so a denied dialog is not shown again.
   - Map each OS error deliberately and add a test next to the existing ones in `error.rs`.
5. **Permission dialogs.** If the backend waits for a dialog, hold the `permission_pending` sender (`watch::Sender<bool>`) at `true` while it is open (see `PermissionPending` in `input-emulation/src/lib.rs` and `Pending` in `input-capture/src/libei.rs`). `cli list` and the approval notice read it. On macOS, only `macos_permissions` shows prompts; backends check silently.
6. **External processes and files.** External commands go through `doctor::run_command`. No blocking calls on the service's `LocalSet`: use `spawn_blocking` or a thread of your own, as the Windows hooks do.
7. **Packaging.** If the binary now links a new system library, add its soname (from `readelf -d`) to `[package.metadata.generate-rpm.requires]` in `Cargo.toml`. The deb's `depends = "$auto"` picks it up by itself. A new crate dependency must pass `cargo deny check licenses bans sources`.
8. **Docs.** Add a row to the table under "Platform support" in `README.md`, including what you tested and when. Add a line to `CHANGELOG.md` under the unreleased version, and add the backend's constants to the Tunables table in `AGENTS.md`.
9. **Verify.** Run the gates in `CONTRIBUTING.md`, including the cross-target clippy lines. Then smoke the real desktop: start `MIMOUSE_LOG_LEVEL=debug cargo run`, check `mimouse cli doctor`, and cross into and out of a peer with keys held. Say in the pull request what you ran on real hardware; cross-compiling only shows that the code type-checks.

## Capture

`Capture` is a `Stream` of `(Position, CaptureEvent)` plus control methods (doc comments in `input-capture/src/lib.rs`):

- `create(pos, range)` puts a barrier at an edge, covering `range` of it (`EdgeRange`, applied by the backend: barrier or surface length, or an activation check). It replaces the barrier already at `pos`. `destroy(pos)` removes it.
- When the cursor crosses a barrier, yield `Begin`, then `Input` events. `release()` gives the pointer back. Pressed keys are tracked and released by the layers above; you never synthesize key-ups.
- `focus(pos, range)` starts capture without a crossing and reports `Begin`. A backend that cannot grab input by itself returns `CaptureError::FocusByPointerPush`, and the service pushes the local pointer through the barrier with input emulation.
- `set_hotkeys` is only for backends that see local keys while input is local (Windows, macOS). Feed keys to `chord::ChordMatcher` (`feed` → `Fed::{Pass, Fired, Swallowed}`, `swallow`); never write a second chord state machine.
- Events produced in OS callbacks go through `event_queue`. It never blocks or panics; over its limit it drops motion and presses but keeps releases, `Modifiers` and `End`.
- `terminate()` tears everything down.

`InputCapture` alone owns the pending queue and the client ids. It drops queued input on `release()` and on a destroyed id, and it handles enter-only ids (inbound peers that get only `Begin`/`End`). Don't reimplement these in a backend.

Contract: `input-capture/src/contract.rs` lists the rules every backend keeps under `InputCapture` (module doc) and checks them on each `SUBJECTS` entry. The rules cover:

- which ids get a crossing, input and `End`;
- barrier ranges, and a destroyed id getting nothing more;
- `release()` dropping queued input;
- focus, including `FocusByPointerPush`;
- hotkeys following the barriers;
- a flooded OS queue keeping releases and `End`;
- errors arriving after the events queued before them;
- held keys handed over once.

A `Subject` declares whether `focus` starts capture by itself (`focuses`) and whether the backend can report where no barrier is (`reports_anywhere`). It also gives a `start` function that returns the backend and a `Driver`, which makes it report events. Add your backend there if it can run in CI without a real desktop, as layer-shell does against a fake compositor (`layer_shell::tests::on_fake_compositor`). Otherwise add a `fake(...)` subject that reproduces its queue limit and focus behaviour with `testing::Fake`, as Windows, macOS and libei have. Put its decisions in pure, platform-independent functions (like `input-capture/src/geometry.rs`) and test those.

## Emulation

`Emulation` (`input-emulation/src/lib.rs`):

- `consume(event, handle)` replays one event. `InputEmulation` already passed it through `normalize.rs`, which drops out-of-protocol values, tracks held keys and buttons per handle and releases them on `destroy`/`terminate`, and keeps motion and scroll remainders. Return `Ok` only when the event was delivered. Under backpressure, motion may be dropped but releases never (see the wlroots notes in `AGENTS.md`).
- `steps()` declares the smallest motion and scroll step the backend can emit (default `Steps::EXACT`); the layer above accumulates anything finer.
- `enter_at(handle, edge, fraction)` places the pointer at `geometry::entry_point` and returns the point. Return `None` only if the backend cannot place the pointer absolutely.
- `create`, `destroy`, `terminate` manage per-peer handles.
- If the OS does not repeat injected keys, use the `keys.rs` repeat planner (shared by Windows and macOS).

Contract: `input-emulation/src/contract.rs` replays the wire traces in `tests/fixtures/emulation/*.trace` against every `SUBJECTS` entry. It checks that everything held is released, that nothing arrives after `terminate`, that malformed datagrams never reach a backend, and where the pointer may be placed. Add the backend to `SUBJECTS` if it can run in CI, as wlroots does against a fake compositor (`wlroots::tests::on_fake_compositor`). Otherwise keep the platform logic in pure modules (`macos_logic.rs`, `input-event/src/macos_scroll.rs`) and test those. A new kind of wire behaviour gets a new trace in `TRACES`.

## Clipboard

A clipboard backend implements one method:

```rust
impl Backend for MyPlatform {
    async fn session(&mut self, cx: &mut Context) -> Result<(), End> {
        // connect to the platform, ask for access (cx.awaiting_approval()),
        // put back what peers set before if the selection died with the
        // last session (cx.owned()), then:
        cx.enabled();
        loop {
            tokio::select! {
                change = platform_changes.next() => {
                    // a change that is not our own write
                    let noticed = cx.noticed(Instant::now());
                    match read_formats().await {
                        Read::Data(data) => cx.copied(noticed, data),
                        Read::Cleared => cx.cleared(noticed),
                        Read::Secret => cx.not_shared(noticed),
                    }
                }
                set = cx.next_set() => {
                    let Some(set) = set else { return Ok(()) }; // service stopping
                    write_formats(&set.data).await;
                }
            }
        }
    }
}
```

Then add a `spawn` that calls `backend::spawn(MyPlatform, changes, status)`, and add its arm in `spawn_backend` (`src/clipboard/mod.rs`) under the platform's cfg.

`Context` (`src/clipboard/backend.rs`) is the backend's only way to report and receive, and it keeps the rules:

- Call `cx.noticed(at)` as soon as a change is noticed, before reading it. Contents from peers accepted before that moment are no longer set, because the user's copy is newer. If the platform can notice a change on another thread while a write is pending, check `SetRequest::older_than` under the same lock as the write (as `windows.rs` does with `Own`).
- Report what you read as `copied` (no supported format counts as not shared), `cleared` (the owner offers no format at all), or `not_shared` (marked secret). Secret markers are classified by pure functions on `Offer` (`from_mime_types`, `from_windows_formats`). Add one for your platform, with tests, and never read a secret copy.
- Report the contents present when the session starts with `cx.startup(data)`; they are not sent.
- Never report your own writes. Recognize them by what the platform gives you: the portal's `session_is_owner`, the Windows clipboard sequence number, or on macOS the pasteboard `changeCount` after writing.
- `cx.next_set()` yields only the latest contents from peers, also after a slow approval or a retry. `cx.owned()` holds them until a local change, for re-announcing after a session restart or serving lazy transfers (portal `SelectionTransfer`).
- Return `End::Retry(reason)` when the platform may work again later (the session closed, a timeout, the system refusing for now): a new session starts after `RETRY_DELAY`. Return `End::Final(reason)` only when it will not work while the service runs (a denied dialog, a missing interface): received updates are refused until restart. The status shown by `cli list` follows from these.
- Use the canonical formats (`MIME_TEXT`, `MIME_HTML`, `MIME_PNG`, files as `FileRef`s), and convert to and from the platform's formats in the backend. Stay within `MAX_CLIPBOARD_SIZE`.
- Run blocking OS calls on `spawn_blocking` or a thread.

Contract: the tests in `src/clipboard/backend.rs` check `Context` and the retry loop that every backend runs under: copies vs. clears vs. secrets, startup contents, older updates dropped, latest wins, owned contents across sessions, retry vs. final. The platform pieces you add get their own pure tests, as with `Offer::from_windows_formats`, `own_seq_after_write` and the portal's `OwnerChanges`.

macOS has no clipboard backend yet. A starting point: `NSPasteboard` has no change notification, so poll `changeCount` in the session loop (on `spawn_blocking`), call `cx.noticed` when it moves and was not ours, and recognize our own writes by the `changeCount` after writing.

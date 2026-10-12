# MiMouse agent notes

Software KVM: one Rust binary (`mimouse`) runs the service; `mimouse cli` controls it over IPC. No GUI.

## Layout

- `src/`: service, config, DTLS listener/connection, CLI, IPC (`ipc/`), wire format (`proto.rs`), clipboard (`clipboard/`).
  - `connect.rs`: `Connector` (shared, Clone) owns outgoing connections; the service calls `keep_connected`/`stop` on activate/deactivate/remove. `MiMouseConnection::recv` yields `ConnEvent::{Received, Closed}`; capture releases on `Closed` of the active client.
  - `clipboard/`: `wire.rs` (frame v1 = formats, v2 = formats + files; send v1 when no files), `files.rs` (copied-file refs, received-file store under the cache dir).
  - `inhibit.rs` (idle/lock inhibit), `notify.rs` (switch and approval notices), `approval.rs` (portal approval waits).
- `input-capture/`: OS events → `Stream<CaptureEvent>` (libei, layer-shell, windows, macos).
- `input-emulation/`: replays events via the `Emulation` trait (libei, wlroots, remote-desktop portal, x11, windows, macos). `Emulation::enter_at` places the pointer for `ProtoEvent::EnterAt` via `geometry::entry_point` (pure, tested); return `None` only when the backend can't place absolutely. macOS key logic is in `macos_logic.rs` and the scroll sign in `input-event/src/macos_scroll.rs`; both are platform-independent and unit-tested, so change them rather than the CoreGraphics glue.
- `input-event/`: shared scancodes and event types; extend here, don't duplicate translations.
- `vendor/webrtc-util`: 0.11.0 with webrtc-rs/webrtc#812 (Windows `WSAECONNRESET` fix). Drop once a release includes it.
- `packaging/`: Scoop, winget (schema 1.10.0) and Homebrew manifests holding the Cargo.toml version and `@SHA256_*@` placeholders; `packaging/render-manifests.py` fills the hashes and validates against the official schemas in the release workflow. Bump the version here with Cargo.toml (the release fails otherwise). `.deb`/`.rpm` metadata is in Cargo.toml `[package.metadata.deb]` / `[package.metadata.generate-rpm]`; the rpm requires list is the binary's NEEDED sonames (`readelf -d`), so update it when a new system library gets linked.

## Invariants

- A peer is either sending or receiving, never both; this prevents feedback loops and indirect control through a third machine.
- Input: UDP/DTLS. Clipboard: TCP/TLS on the same port, same certificate, same `authorized_fingerprints`.
- IPC (`ipc/`): newline-delimited JSON; requests carry a `seq` and get exactly one `Reply`. Windows uses a per-user named pipe with an owner-only DACL, never a TCP port. State-changing requests are audited (`audit.rs`); never log typed text or clipboard contents.
- Wire-format changes break compatibility with Lan Mouse peers. `ProtoEvent::EnterAt` is an `Enter` datagram plus two trailing bytes that Lan Mouse ignores; decode by datagram length (`ProtoEvent::decode`), never from the whole receive buffer.
- Capture and emulation track pressed keys and release them on disconnect (`terminate()`). Every capture end (release bind, compositor/portal deactivation, error, send failure) goes through `release_capture`, which sends key-ups and emits `ICaptureEvent::Released`; `Service::control_switched` is the single switch hook (notifier + idle inhibit), so a new release path that bypasses it leaves keys stuck and the inhibitor held.
- Timing: PING_INTERVAL 500 ms and MAX_MISSED_PINGS 8 (`connect.rs`), WATCHDOG_TIMEOUT 3 s and WATCHDOG_TICK 500 ms (`emulation.rs`), reconnect backoff 1-30 s.
- Windows: injected mouse moves tagged `input_capture::IDLE_KEEPALIVE_TAG` must pass the LL hook.
- Received file names are base names validated by `files::valid_name`; never join an unvalidated peer name onto a path.
- `mimouse.pem` stays owner-only (0600 / protected DACL, `crypto.rs`). Config write-backs edit `config.toml` in place with `toml_edit`; never re-serialize the whole file.
- macOS: only `macos_permissions` shows permission prompts (once, at service start); backends check silently.

## Conventions

- Backends are cargo features in the root `Cargo.toml`; gate modules with the cfgs emitted by `build.rs`.
  `build.rs` reads the host, so cross-checking needs `--no-default-features`.
- Tokio single-threaded runtime (`LocalSet`), `futures` streams, `async_trait`; no blocking in async code.
- Errors via `thiserror`. Comments only for non-obvious invariants.
- Update README.md when changing user-visible behavior or platform support.

## Commands

```sh
cargo fmt && cargo clippy --workspace --all-targets && cargo test --workspace
cargo clippy --release --target x86_64-pc-windows-gnu --no-default-features
MIMOUSE_LOG_LEVEL=debug cargo run
```

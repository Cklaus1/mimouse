# MiMouse agent notes

Software KVM: one Rust binary (`mimouse`) runs the service; `mimouse cli` controls it over IPC. No GUI.

## Layout

- `src/`: service, config, DTLS listener/connection, CLI, IPC (`ipc/`), wire format (`proto.rs`), clipboard (`clipboard/`).
- `input-capture/`: OS events → `Stream<CaptureEvent>` (libei, layer-shell, windows, macos).
- `input-emulation/`: replays events via the `Emulation` trait (libei, wlroots, remote-desktop portal, x11, windows, macos).
- `input-event/`: shared scancodes and event types; extend here, don't duplicate translations.
- `vendor/webrtc-util`: 0.11.0 with webrtc-rs/webrtc#812 (Windows `WSAECONNRESET` fix). Drop once a release includes it.

## Invariants

- A peer is either sending or receiving, never both; this prevents feedback loops and indirect control through a third machine.
- Input: UDP/DTLS. Clipboard: TCP/TLS on the same port, same certificate, same `authorized_fingerprints`.
- IPC (`ipc/`): newline-delimited JSON; requests carry a `seq` and get exactly one `Reply`. Windows uses a per-user named pipe with an owner-only DACL, never a TCP port. State-changing requests are audited (`audit.rs`); never log typed text or clipboard contents.
- Wire-format changes break compatibility with Lan Mouse peers.
- Capture and emulation track pressed keys and release them on disconnect (`terminate()`).
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

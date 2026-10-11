# MiMouse

[![CI](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml) [![Release](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml)

Share one mouse and keyboard between Linux, Windows and macOS over the LAN (software KVM).
Fork of [Lan Mouse](https://github.com/feschber/lan-mouse) by Ferdinand Schober (GPL-3.0-or-later); the input protocol is unchanged, so it interoperates with Lan Mouse 0.11.

Changes from upstream:
- Clipboard sharing: text, HTML, images (Linux via xdg-desktop-portal, Windows).
- Windows UDP listener no longer dies on `WSAECONNRESET` ([lan-mouse#262](https://github.com/feschber/lan-mouse/issues/262)).
- No GUI: a single background binary, configured by `config.toml` and `mimouse cli`.

Traffic is encrypted with DTLS (input) and TLS (clipboard); peers authenticate by certificate fingerprint.

## Build

```sh
sudo apt install libx11-dev libxtst-dev   # Debian/Ubuntu
sudo dnf install libX11-devel libXtst-devel   # Fedora
sudo pacman -S libx11 libxtst   # Arch
cargo build --release   # -> target/release/mimouse
```

Windows and macOS need no extra libraries. Windows also cross-compiles from Linux:
`cargo build --release --target x86_64-pc-windows-gnu`.
Release binaries: [releases](https://github.com/Cklaus1/mimouse/releases).

## Setup

1. Run `mimouse` on each machine. The log shows `certificate fingerprint: aa:bb:...`.
2. Write `config.toml` on each machine (see [config.toml](config.toml)): the peer under `[[clients]]` with its position, and the peer's fingerprint under `[authorized_fingerprints]`.
   - Linux/macOS: `~/.config/mimouse/config.toml`
   - Windows: `%LOCALAPPDATA%\mimouse\config.toml`
3. Open the port (default 4242): UDP for input, plus TCP if `clipboard = true`. firewalld: `firewall/mimouse.xml`.
4. Restart `mimouse`.

An unauthorized peer shows up in the log with the command to approve it (`mimouse cli authorize-key <name> <fingerprint>`).

Default release bind (return control to the local machine): Ctrl+Shift+Super+Alt.

## Run at login

- **Linux:** install `service/mimouse.service` to `~/.config/systemd/user/` (fix `ExecStart` if the binary isn't `/usr/bin/mimouse`), then `systemctl --user enable --now mimouse`.
- **Windows:** scheduled task at logon running `mimouse.exe daemon`, "run with highest privileges" (needed to control elevated windows).
- **macOS:** on first start, grant Accessibility and Input Monitoring when prompted.

## CLI

Talks to the running service; `mimouse cli help` lists commands:
`list`, `watch`, `add-client`, `remove-client`, `activate`, `deactivate`, `set-host`, `set-port`, `set-position`, `set-ips`, `set-enter-hook`, `change-port`, `authorize-key`, `remove-authorized-key`, `enable-capture`, `enable-emulation`, `save-config`.

- Commands wait for the service, exit non-zero on failure (e.g. unknown client id), and save `config.toml` on success.
- `--json`: one JSON value per result. `watch --json` prints the current state, then streams events (clients, connections, clipboard transfers without contents, unauthorized attempts) as JSON lines; schema: `FrontendEvent` in `src/ipc/mod.rs`.
- Control channel: Unix socket in `$XDG_RUNTIME_DIR` (Linux) or `~/Library/Caches` (macOS); on Windows the named pipe `\\.\pipe\mimouse-<user SID>`, accessible only to the current user, local only. The CLI refuses a pipe owned by anyone else.
- Audit log: every state-changing request is appended to `audit.jsonl` next to `config.toml` (time, requesting pid/user/exe, action, details, result); rotated at 10 MiB, 3 old files kept. `list` and `watch` are not logged.

Logging: `MIMOUSE_LOG_LEVEL=debug mimouse`.

## Platform support

Sending needs a capture backend; receiving needs an emulation backend. Chosen automatically; override with `--capture-backend` / `--emulation-backend`.

| Desktop | Capture | Emulation |
|---|---|---|
| GNOME >= 45 | libei | libei, remote-desktop portal |
| KDE Plasma >= 6.1 | libei, layer-shell | libei, remote-desktop portal |
| wlroots (Sway >= 1.8, Hyprland, Wayfire) | layer-shell | wlroots |
| X11 | none | x11 |
| Windows | windows | windows |
| macOS | macos | macos |

Backends are cargo features (`Cargo.toml`), e.g. `cargo build --no-default-features --features layer_shell_capture,wlroots_emulation`.
Clipboard: Linux (portal Clipboard interface, e.g. GNOME) and Windows; not macOS.

## Known issues

- X11 can only receive.
- wlroots without libei on the receiving side ignores modifiers unless the sender uses layer-shell.
- Wayfire needs the `shortcuts-inhibit` plugin.
- Windows hides the cursor while receiving if no physical mouse is attached.
- The first crossing right after start can be dropped while the peer connects; cross again.
- GNOME can stop accepting emulated input ([lan-mouse#193](https://github.com/feschber/lan-mouse/issues/193)); run `mimouse cli enable-emulation`.

## Development

```sh
cargo fmt && cargo clippy --workspace --all-targets && cargo test --workspace
git config core.hooksPath .githooks   # optional pre-commit: fmt, clippy, test
```

See [AGENTS.md](AGENTS.md) for architecture.

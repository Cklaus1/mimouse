# MiMouse

[![CI](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml) [![Release](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml)

Share one mouse and keyboard between Linux, Windows and macOS over the LAN (software KVM).
Fork of [Lan Mouse](https://github.com/feschber/lan-mouse) by Ferdinand Schober (GPL-3.0-or-later); the input protocol is unchanged, so it interoperates with Lan Mouse 0.11.

Changes from upstream:
- Clipboard sharing: text, HTML, images and copied files (Linux via xdg-desktop-portal, Windows).
- The cursor appears on the other machine at the height where it left (MiMouse peers).
- Connects at startup and reconnects with backoff; control comes back if the controlled machine disappears.
- Notifications on each switch of control; the controlling machine doesn't idle or lock.
- Return control with a Ctrl double-tap.
- macOS: modifier shortcuts and scroll direction fixed ([lan-mouse#450](https://github.com/feschber/lan-mouse/issues/450), [#481](https://github.com/feschber/lan-mouse/issues/481)).
- Windows UDP listener no longer dies on `WSAECONNRESET` ([lan-mouse#262](https://github.com/feschber/lan-mouse/issues/262)).
- No GUI: a single background binary, configured by `config.toml` and `mimouse cli`.

Traffic is encrypted with DTLS (input) and TLS (clipboard); peers authenticate by certificate fingerprint.
The certificate key (`mimouse.pem`) is readable by its owner only (0600 on Linux/macOS, a protected owner-only DACL on Windows); looser permissions are repaired at startup with a warning.

## Build

```sh
sudo apt install libx11-dev libxtst-dev   # Debian/Ubuntu
sudo dnf install libX11-devel libXtst-devel   # Fedora
sudo pacman -S libx11 libxtst   # Arch
cargo build --release   # -> target/release/mimouse
```

Windows and macOS need no extra libraries. Windows also cross-compiles from Linux:
`cargo build --release --target x86_64-pc-windows-gnu`.
Release binaries and packages: [releases](https://github.com/Cklaus1/mimouse/releases) (`.deb`, `.rpm`, Windows `.exe`, macOS). Package manifests (Scoop, winget, Homebrew) are under `packaging/`; each tagged release attaches them with filled-in hashes.
Packages from source: `cargo build --release && cargo deb --no-build && cargo generate-rpm`.

## Setup

1. Run `mimouse` on each machine (or install the package / service, see [Run at login](#run-at-login)).
2. Open the port (default 4242): UDP for input, TCP for clipboard and pairing (TCP is needed even with `clipboard = false`). firewalld: `firewall/mimouse.xml`.
3. Pair the machines (below), or write `config.toml` by hand on each machine (see [config.toml](config.toml)): the peer under `[[clients]]` with its position, and the peer's fingerprint (`certificate fingerprint: aa:bb:...` in its log) under `[authorized_fingerprints]`.
   - Linux/macOS: `~/.config/mimouse/config.toml`
   - Windows: `%LOCALAPPDATA%\mimouse\config.toml`

### Pairing

On the machine to be added run `mimouse cli pair --accept` (waits 120 s, `--timeout` to change). On this machine run `mimouse cli pair <host> --position <left|right|top|bottom>`. `<host>` can be a host name, an address, or a Tailscale machine name; `mimouse cli discover` lists tailnet machines that answer on the MiMouse port (it reads `tailscale status`; no mDNS). Both terminals show a 6-digit code; confirm only if both codes match. Without a terminal, use `mimouse cli pair --confirm <code>` or `--cancel`. Each side then trusts the other's certificate and adds it as a client on the matching side. Being on the same tailnet never implies trust.

### Troubleshooting

`mimouse cli doctor` checks the service, config, key file permissions, firewall and every client (reachable over DTLS/TLS, trusted in both directions) and prints a fix for each failure. It exits non-zero when any check fails; `--json` gives machine-readable output.

An unauthorized peer shows up in the log with the command to approve it (`mimouse cli authorize-key <name> <fingerprint>`).

`save-config` and other write-backs edit `config.toml` in place: comments, key order and unknown keys are kept, and only changed values are rewritten. `[[clients]]` entries are matched by hostname + position.

## Switching

- **Return control** to the local machine: double-tap Ctrl (either side, nothing pressed in between), or hold Ctrl+Shift+Super+Alt. Change with `release_double_tap_ctrl = false` and `release_bind = [...]` in `config.toml`. Held keys and modifiers are released on the other machine whenever control ends, however it ends.
- **Cursor position:** the receiving machine places its pointer at the same relative position along the entered edge, on the display at that height, 1 px inside the edge. Works with the Windows, macOS, GNOME (libei), wlroots and X11 emulation backends; the remote-desktop portal fallback can't place the pointer, so it stays where it was. Lan Mouse peers keep the old behaviour.
- **Connections:** MiMouse connects to every active client at startup and on activation, so the first crossing goes straight through. A dropped connection is retried after 1 s, doubling to at most 30 s. If the controlled machine disappears, control comes back within about 4 s and its held keys are released; packet-loss bursts under 4 s don't drop the connection. The receiving side releases held keys after 3 s without any datagram.
- **Notifications:** a short notice on each switch of control ("Controlling megapc" / "Back on gpumaster"; Linux desktop notification, Windows tray balloon, macOS Notification Center). Portal approval waits and connection attempts from unknown certificates are always announced. `notify_switch = false` turns off the switch notices.
- **Idle:** while this machine controls another, it doesn't idle or lock (`idle_inhibit = false` turns this off); released 5 s after control returns.
- **GNOME approval:** capture and emulation need a one-time approval in a GNOME dialog. `cli list` shows `awaiting_approval` and the log warns every 30 s until someone approves; capture then starts without a restart. With xdg-desktop-portal-gnome >= 51 (InputCapture v2) the grant is remembered in `~/.cache/mimouse/input-capture.token`; GNOME 50 asks at every service start. Config reloads that change only hostnames, ips, ports or hooks don't recreate the capture session.

## Clipboard

Set `clipboard = true` on both machines. Text, HTML and images go to the machine the cursor enters. Copied files (not folders) are sent too: Windows Explorer / `Set-Clipboard -Path`, GNOME Files, or anything offering `text/uri-list` / `x-special/gnome-copied-files`, up to `max_file_size` bytes in total (default 100 MiB); larger selections are not sent and a warning is logged. The receiver stores files in `~/.cache/mimouse/files/<transfer>/` (Windows: `%LOCALAPPDATA%\mimouse\files\<transfer>\`), keeps the newest 3 transfers, and puts them on its clipboard as files, so pasting in a file manager copies them. File transfer needs MiMouse on both sides; text and images still reach older MiMouse peers.

## Run at login

- **Linux:** the `.deb`/`.rpm` installs `/usr/bin/mimouse`, the user unit `/usr/lib/systemd/user/mimouse.service` and the firewalld service `mimouse`; run `systemctl --user enable --now mimouse` and `sudo firewall-cmd --permanent --add-service=mimouse && sudo firewall-cmd --reload`. From source: install `service/mimouse.service` to `~/.config/systemd/user/` (fix `ExecStart` if the binary isn't `/usr/bin/mimouse`).
- **Windows:** scheduled task at logon running `mimouse.exe daemon`, "run with highest privileges" (needed to control elevated windows).
- **macOS:** on first start, grant Accessibility and Input Monitoring when prompted.

## CLI

Talks to the running service; `mimouse cli help` lists commands:
`list`, `watch`, `discover`, `doctor`, `pair`, `add-client`, `remove-client`, `activate`, `deactivate`, `set-host`, `set-port`, `set-position`, `set-ips`, `set-enter-hook`, `change-port`, `authorize-key`, `remove-authorized-key`, `enable-capture`, `enable-emulation`, `save-config`.

- Commands wait for the service, exit non-zero on failure (e.g. unknown client id), and save `config.toml` on success.
- `--json`: one JSON value per result. `list --json` includes `capture` and `emulation`: `enabled`, `disabled` or `awaiting_approval` (a portal dialog is waiting). `watch --json` prints the current state, then streams events (clients, connections, clipboard transfers without contents, unauthorized attempts) as JSON lines; schema: `FrontendEvent` in `src/ipc/mod.rs`.
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
- GNOME can stop accepting emulated input ([lan-mouse#193](https://github.com/feschber/lan-mouse/issues/193)); run `mimouse cli enable-emulation`.

## Development

```sh
cargo fmt && cargo clippy --workspace --all-targets && cargo test --workspace
git config core.hooksPath .githooks   # optional pre-commit: fmt, clippy, test
```

See [AGENTS.md](AGENTS.md) for architecture.

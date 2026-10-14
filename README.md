# MiMouse

[![CI](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/rust.yml) [![Release](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml/badge.svg)](https://github.com/Cklaus1/mimouse/actions/workflows/release.yml)

MiMouse shares one mouse and keyboard between Linux, Windows and macOS computers on a local network (a software KVM switch). Move the pointer across a screen edge and keyboard and mouse input goes to the machine on that side; the clipboard can follow. One binary runs the background service (`mimouse daemon`, or `mimouse` with no arguments) and the command line interface that controls it (`mimouse cli ...`); there is no GUI.
MiMouse is originally forked from [Lan Mouse](https://github.com/feschber/lan-mouse) by Ferdinand Schober (GPL-3.0-or-later) and is developed independently of it.

Differences from Lan Mouse:
- Clipboard sharing: text, HTML, images and copied files (Linux via xdg-desktop-portal, Windows).
- The cursor appears on the other machine at the height where it left (MiMouse peers).
- Connects at startup and reconnects with backoff; control comes back if the controlled machine disappears.
- Notifications on each switch of control; the controlling machine doesn't idle or lock.
- Return control with a Ctrl double-tap.
- macOS: modifier shortcuts and scroll direction fixed ([lan-mouse#450](https://github.com/feschber/lan-mouse/issues/450), [#481](https://github.com/feschber/lan-mouse/issues/481)).
- Windows UDP listener no longer dies on `WSAECONNRESET` ([lan-mouse#262](https://github.com/feschber/lan-mouse/issues/262)).
- No GUI: a single background binary, configured by `config.toml` and `mimouse cli`.
- Pairing with a confirmation code, certificate pinning, `mimouse cli doctor`, opt-in [telemetry](#telemetry).

Compatibility: the input protocol interoperates with Lan Mouse 0.11. MiMouse-only features (clipboard, cursor position on entry, pairing) need MiMouse on both sides. Later Lan Mouse versions are not tracked.

Traffic is encrypted with DTLS (input) and TLS (clipboard); peers authenticate by certificate fingerprint.
The certificate key (`mimouse.pem`) is readable by its owner only (0600 on Linux/macOS, a protected owner-only DACL on Windows); looser permissions are repaired at startup with a warning.


## Install

Downloads are on the [releases](https://github.com/Cklaus1/mimouse/releases) page: `.deb` and `.rpm` packages (x86_64 and arm64), bare binaries `mimouse-linux-x86_64`, `mimouse-linux-arm64`, `mimouse-windows-x86_64.exe`, `mimouse-macos-arm64` and `mimouse-macos-intel`. Package manifests (Scoop, winget, Homebrew) are under `packaging/`; each tagged release attaches them with filled-in hashes.

### Linux packages

```sh
sudo apt install ./mimouse_*.deb      # Debian/Ubuntu
sudo dnf install ./mimouse-*.rpm      # Fedora/openSUSE
systemctl --user enable --now mimouse
```

The package installs `/usr/bin/mimouse`, the systemd user unit `/usr/lib/systemd/user/mimouse.service`, the firewalld service `mimouse` and an example config in `/usr/share/doc/mimouse/config.toml.example`. It recommends `xdg-desktop-portal`, which GNOME and KDE capture, emulation and clipboard use.

### Windows

Download `mimouse-windows-x86_64.exe`, rename it to `mimouse.exe` and put it in a folder of your choice. Windows builds are unsigned:
- SmartScreen may block the first start; choose "More info", then "Run anyway".
- Microsoft Defender has flagged unsigned builds with a machine-learning heuristic (`Trojan:Win32/Bearfoos.B!ml`) in testing. If it quarantines `mimouse.exe`, report a false positive at https://www.microsoft.com/wdsi/filesubmission. Don't turn Defender off.

To start it at logon, create a scheduled task at logon running `mimouse.exe daemon` with "run with highest privileges" (needed to control elevated windows).

### macOS (experimental)

Download `mimouse-macos-arm64` (Apple silicon) or `mimouse-macos-intel`, then remove the Gatekeeper quarantine and make it executable:

```sh
xattr -d com.apple.quarantine mimouse-macos-arm64
chmod +x mimouse-macos-arm64
```

On first start, grant Accessibility and Input Monitoring when prompted. macOS is not covered by the release smoke tests (see [Platform support](#platform-support)).

### From source

```sh
sudo apt install libx11-dev libxtst-dev       # Debian/Ubuntu
sudo dnf install libX11-devel libXtst-devel   # Fedora
sudo pacman -S libx11 libxtst                 # Arch
cargo build --release   # -> target/release/mimouse
```

Windows and macOS need no extra libraries. Windows also cross-compiles from Linux: `cargo build --release --target x86_64-pc-windows-gnu`. Packages from source: `cargo build --release && cargo deb --no-build && cargo generate-rpm`.

### Run at login

- **Linux:** `systemctl --user enable --now mimouse` (packages). From source: install `service/mimouse.service` to `~/.config/systemd/user/` (fix `ExecStart` if the binary isn't `/usr/bin/mimouse`).
- **Windows:** the scheduled task above.
- **macOS:** `brew services start mimouse` with the Homebrew formula, or start `mimouse` yourself.

The service stops cleanly on SIGTERM and SIGHUP (e.g. `systemctl --user stop/restart`, a closed terminal) and, on Windows, when its console is closed or at logoff or shutdown: control goes back to the controlled machine and held keys are released.

## Firewall

MiMouse uses one port, default 4242: UDP for input, TCP for clipboard and pairing (TCP is needed even with `clipboard = false`). Open both on every machine. `mimouse cli doctor` checks the firewall and prints the matching command.

- **ufw:** `sudo ufw allow 4242/udp && sudo ufw allow 4242/tcp`
- **firewalld:** packages install the service `mimouse`: `sudo firewall-cmd --permanent --add-service=mimouse && sudo firewall-cmd --reload`. From source, copy `firewall/mimouse.xml` to `/etc/firewalld/services/` first, or open the ports: `sudo firewall-cmd --permanent --add-port=4242/udp --add-port=4242/tcp && sudo firewall-cmd --reload`.
- **Windows:** allow `mimouse.exe` if Windows Defender Firewall asks, or in an admin shell: `netsh advfirewall firewall add rule name=MiMouse dir=in action=allow protocol=UDP localport=4242` and the same with `protocol=TCP`.
- **macOS:** System Settings > Network > Firewall > Options: allow incoming connections for mimouse. `doctor` reads the state with `socketfilterfw --getglobalstate`, but can't read per-app rules.

## First pairing

1. Install MiMouse on both machines and start the service ([Install](#install)).
2. Open port 4242 UDP and TCP on both ([Firewall](#firewall)).
3. On the machine to be added (here `laptop`), run `mimouse cli pair --accept`.
4. On the other machine, run `mimouse cli pair laptop --position right`, where `--position` (`left`, `right`, `top`, `bottom`) is where `laptop` sits relative to this machine.
5. Both terminals show a 6-digit code. Confirm on both only if the codes match. Then move the pointer across that edge; if it doesn't cross, run `mimouse cli doctor` on both machines.

### Pairing details

On the machine to be added run `mimouse cli pair --accept` (waits 120 s, `--timeout` to change). On this machine run `mimouse cli pair <host> --position <left|right|top|bottom>`. `<host>` can be a host name, an address, or a Tailscale machine name; `mimouse cli discover` lists tailnet machines that answer on the MiMouse port (it reads `tailscale status`; no mDNS). Both terminals show a 6-digit code; confirm only if both codes match. Without a terminal, use `mimouse cli pair --confirm <code>` or `--cancel`. Each side then trusts the other's certificate and adds it as a client on the matching side. Being on the same tailnet never implies trust.

Pairing matches an existing client by the peer's certificate first. A client with the same name or address is reused only if it is not pinned to another certificate, so two machines named alike stay separate. If pairing succeeds but the config can't be saved, `cli pair` says "paired, not saved" and names the fix (`mimouse cli save-config`).

### Writing config.toml by hand

Instead of pairing, put on each machine the peer under `[[clients]]` with its position, and the peer's fingerprint (`certificate fingerprint: aa:bb:...` in its log) under `[authorized_fingerprints]`; see the example [config.toml](config.toml). Optionally pin the peer's certificate in its `[[clients]]` entry with `fingerprint = "aa:bb:..."`; without it the first certificate the peer presents is pinned and saved automatically (trust on first use).

## Daily use

### Switching

- **Return control** to the local machine: double-tap Ctrl (either side, nothing pressed in between), or hold Ctrl+Shift+Super+Alt. Change with `release_double_tap_ctrl = false` and `release_bind = [...]` in `config.toml`. Held keys and modifiers are released on the other machine whenever control ends, however it ends.
- **Machine hotkeys:** give a client `hotkey = [...]` (an exact chord of key names) and this machine `local_hotkey = [...]`. While you control a remote machine, a client's hotkey moves control straight to it; `local_hotkey`, like the release bind, brings it back. Only the exact chord counts (holding an extra key such as Shift does not trigger it), and the chord's last key is not passed on. On Windows and macOS the hotkeys also work while input is local: control moves as if the pointer had crossed in at the middle of the client's edge range.
- **`mimouse cli focus <id>`** does the same from the command line on every backend (`--json` works). It replies once control has moved, or exits non-zero with `no such client: N`, `client N is not active`, `input capture is waiting for permission` (GNOME dialog not approved yet), or the reason. GNOME and other Wayland sessions don't let MiMouse see local keys, so bind `mimouse cli focus <id>` as a custom shortcut (GNOME Settings > Keyboard > Custom Shortcuts). There, focus starts capture by pushing the local pointer through the client's barrier with MiMouse's own input emulation; this works on GNOME 50 (mutter activates the barrier for the emulated motion). If a compositor ignores barriers for emulated motion, `focus` fails after about a second with `focus is not available on this desktop session ... move the pointer across the edge`.
- **Client ids** (`mimouse cli list`, `cli focus <id>`) stay the same while the service runs: editing or reloading the config keeps a client's id, and a removed client's id is never given to another one. Ids follow config order and may change after a restart.
- **Edge ranges:** `range = [start, end]` on a client limits the part of the shared edge that switches to it, in percent of the desktop's extent along that side (start < end <= 100); crossings elsewhere stay local. Supported by libei (shorter barriers), layer-shell (shorter surface), Windows and macOS capture. An invalid range is a config error naming the client. Several clients may share a position, overlapping ranges included; activating one deactivates the others there.
- **Cursor position:** the receiving machine places its pointer at the same relative position along the entered edge, on the display at that height, 1 px inside the edge. Works with the Windows, macOS, GNOME (libei), wlroots and X11 emulation backends; the remote-desktop portal fallback can't place the pointer, so it stays where it was. Lan Mouse peers keep the old behaviour.
- **Connections:** MiMouse connects to every active client at startup and on activation, so the first crossing goes straight through. A dropped connection is retried after 1 s, doubling to at most 30 s. If the controlled machine disappears, control comes back within about 4 s and its held keys are released; packet-loss bursts under 4 s don't drop the connection. The receiving side releases held keys after 3 s without any datagram, and releases every key and mouse button a peer held when it disconnects or emulation stops.
- **Notifications:** a short notice on each switch of control ("Controlling megapc" / "Back on gpumaster"; Linux desktop notification, Windows tray balloon, macOS Notification Center). Portal approval waits and connection attempts from unknown certificates are always announced. `notify_switch = false` turns off the switch notices.
- **Idle:** while this machine controls another, it doesn't idle or lock (`idle_inhibit = false` turns this off); released 5 s after control returns.
- **GNOME approval:** capture and emulation need a one-time approval in a GNOME dialog. `cli list` shows `awaiting_approval` and the log warns every 30 s until someone approves; capture then starts without a restart. With xdg-desktop-portal-gnome >= 51 (InputCapture v2) the grant is remembered in `~/.cache/mimouse/input-capture.token`; GNOME 50 asks at every service start. Config reloads that change only hostnames, ips, ports or hooks don't recreate the capture session.
- **Emulation backend selection:** if no backend is configured and none works, input emulation stays disabled; MiMouse never falls back to the dummy backend, which doesn't replay input. The service retries every 1 to 30 s while the cause is temporary (desktop portal not up yet at login, EIS/portal connection lost, no backend available yet, a wlroots compositor not ready within a few seconds). If you deny the permission dialog, the service stops retrying until you run `mimouse cli enable-emulation`. On Windows, input sent while the lock screen, a UAC prompt or a more privileged window is active is dropped, and a warning is logged at most every 10 s.

### Clipboard

Set `clipboard = true` on both machines. Text, HTML and images are sent to all active clients at once; copied files only to the client the cursor enters. Copied files (not folders) come from Windows Explorer / `Set-Clipboard -Path`, GNOME Files, or anything offering `text/uri-list` / `x-special/gnome-copied-files`, up to `max_file_size` bytes in total (default 100 MiB); larger selections are not sent and a warning is logged. The receiver stores files in `~/.cache/mimouse/files/<transfer>/` (Windows: `%LOCALAPPDATA%\mimouse\files\<transfer>\`), keeps the newest 3 transfers, and puts them on its clipboard as files, so pasting in a file manager copies them. File transfer needs MiMouse on both sides; text and images still reach older MiMouse peers.

- Copies marked secret by a password manager (KeePassXC and others: `x-kde-passwordManagerHint` on Linux, `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory=0` on Windows) are never sent.
- Clearing the clipboard clears it on the peers too; copying something MiMouse cannot transfer leaves the peers' clipboards unchanged.
- Copied files are streamed: neither side holds a whole file in memory. A receiver reads at most 4 clipboard updates at once and one at a time per peer; further updates wait.
- A copy that arrives after you copied something locally does not overwrite it, if the transfer started before your copy (e.g. a slow file transfer).
- What the clipboard holds when the service starts (Windows) or when clipboard access is granted (GNOME) is not sent; copying it again sends it.
- `mimouse cli list` (and `--json`, field `clipboard`) and `mimouse cli doctor` show the clipboard sharing state: off, starting, awaiting approval (GNOME's remote desktop / clipboard dialog is open; copies received meanwhile are applied once it is approved), enabled, or disabled with the reason (dialog denied: restart the service to ask again). While clipboard sharing is disabled because its backend ended, received copies are refused, so the sending machine logs `peer did not acknowledge the update`.

## Config

`config.toml` is described in the example [config.toml](config.toml). Linux/macOS: `~/.config/mimouse/config.toml`; Windows: `%LOCALAPPDATA%\mimouse\config.toml`; `--config <path>` picks another file.

**Precedence:** a command line flag (`--port`, `--capture-backend`, `--emulation-backend`, `--cert-path`) beats `config.toml`, which beats the built-in default. Every other key comes from the file, else the default. `MIMOUSE_LOG_LEVEL` only sets logging.

**Reload:** the service watches the file. Edits to clients, `release_bind`, `release_double_tap_ctrl`, `hotkey` and `local_hotkey`, `notify_switch`, `idle_inhibit`, `authorized_fingerprints`, `telemetry` and `telemetry_endpoint` apply at once. `clipboard`, `max_file_size`, `capture_backend`, `emulation_backend`, `cert_path` and the file's `port` take effect at the next start; `mimouse cli change-port <port>` is live and saves the new port to config.toml (a `--port` flag still overrides it at startup). When an edit changes a restart-only key, the service logs one warning per reload naming the keys that wait for a restart.

`mimouse cli config [--json]` prints each key's value in effect, where it comes from (`flag`, `file`, `default`, or `cli` for a port set by `change-port`) and the keys changed in the file that wait for a restart. `mimouse cli list` and `mimouse cli doctor` show the capture and emulation backend in use and whether it was configured or picked automatically, e.g. `capture: enabled (input-capture-portal, auto)`.

**Config validation:** besides parse errors and invalid `range`s, `config.toml` is refused when:
- `port` or a client's `port` is 0;
- `max_file_size` is 0 (it is a byte limit, not a switch; use `clipboard = false` to stop sharing);
- `telemetry_endpoint` is set but not a valid `https://` URL (empty means the built-in endpoint);
- two clients have the same `hotkey`;
- a client's `hotkey` equals `local_hotkey` or `release_bind` (the default Ctrl+Shift+Super+Alt counts when `release_bind` is unset).

Chords compare as key sets, so order doesn't matter. Each error names the client and the keys involved, e.g. ``client lap: `hotkey` [KeyLeftCtrl, Key1] is also the `hotkey` of client desk``.

An invalid file is reported at startup, on reload and by `mimouse cli doctor`, and it is never overwritten. The service keeps running on the last good config, and saving is refused until the file is fixed. A CLI change that would make the file invalid, such as `mimouse cli set-port <id> 0` or `mimouse cli change-port 0`, fails and changes nothing.

**Saving:** MiMouse writes config.toml by merging its changes into the file as it is on disk. Edits you make while the service runs are kept, as are comments, key order and unknown keys. If you and the service changed the same key, or the file is invalid, the change is refused and the file is not touched. After every successful load or save, a copy is kept in `config.toml.last-good` (readable only by you). If config.toml is invalid at startup, MiMouse runs on that copy and refuses to save changes until the file is fixed; `mimouse cli doctor` shows the error and the command that restores the copy. A config.toml that is a symlink (e.g. home-manager) stays a symlink, and a read-only target makes saves fail instead of being replaced.

## CLI

Talks to the running service; `mimouse cli help` lists commands:
`list`, `watch`, `config`, `discover`, `doctor`, `pair`, `focus`, `add-client`, `remove-client`, `activate`, `deactivate`, `set-host`, `set-port`, `set-position`, `set-ips`, `set-enter-hook`, `change-port`, `authorize-key`, `remove-authorized-key`, `enable-capture`, `enable-emulation`, `save-config`, `telemetry`.

- Commands wait for the service and save `config.toml` on success.
- Exit codes: 0 success, 1 other error (including no reply from the service within 5 s, e.g. a CLI/service version mismatch), 2 invalid arguments, 3 the service is not running (could not connect), 4 the service rejected the request or it failed (including a failed pairing), 5 `doctor` found failing checks.
- `--json`: one JSON value per result; every error also prints one JSON object `{"error": "<message>"}` on stdout. `list --json` includes `capture` and `emulation`: `enabled`, `disabled` or `awaiting_approval` (a portal dialog is waiting). `watch --json` prints the current state, then streams events (clients, connections, clipboard transfers without contents, unauthorized attempts) as JSON lines; schema: `FrontendEvent` in `src/ipc/mod.rs`.
- Control channel: Unix socket in `$XDG_RUNTIME_DIR` (Linux) or `~/Library/Caches` (macOS); on Windows the named pipe `\\.\pipe\mimouse-<user SID>`, accessible only to the current user, local only. The CLI refuses a pipe owned by anyone else.
- Audit log: every state-changing request is appended to `audit.jsonl` next to `config.toml` (time, requesting pid/user/exe, action, details, result); rotated at 10 MiB, 3 old files kept. `list` and `watch` are not logged.

Logging: `MIMOUSE_LOG_LEVEL=debug mimouse`.

## Platform support

Sending needs a capture backend; receiving needs an emulation backend. Both are chosen automatically; override with `--capture-backend` / `--emulation-backend` or `capture_backend` / `emulation_backend` in `config.toml`. The libei capture backend is called `input-capture-portal` there and in `cli list`.

| Desktop | Capture | Emulation | Clipboard | Tested |
|---|---|---|---|---|
| GNOME (Wayland) | `input-capture-portal` | `libei`, `xdp` (remote-desktop portal) | yes | GNOME 50, against Windows 11, 2026-10-14: Windows -> GNOME crossing (commit f281e041); clipboard text and a 60 MiB file GNOME -> Windows via `cli focus` (commit 54a8d55a) |
| KDE Plasma (Wayland) | `input-capture-portal`, `layer-shell` | `libei`, `xdp` | yes | KWin 6.6.6 `--virtual`, 2026-10-14, commit 9703a5b: crossing both ways, Ctrl double-tap release, held-key release on disconnect, clipboard text both ways |
| sway | `layer-shell` | `wlroots` | off | sway 1.11 headless, 2026-10-14, commit 9703a5b: crossing both ways, Ctrl double-tap release, held-key release on disconnect |
| Hyprland | `layer-shell` | `wlroots` | off | Hyprland 0.53.3 nested, 2026-10-14, commit 9703a5b: crossing both ways, held-key release on disconnect |
| X11 | none (receive only) | `x11` (XTest) | off | Xvfb + openbox 3.6.1, 2026-10-14, commit 9703a5b: crossing in, held-key release on disconnect |
| Windows 10/11 | `windows` | `windows` | yes | Windows 11, the peer in every run above (commit 9703a5b): crossing both ways, clipboard text both ways |
| macOS | `macos` | `macos` | no | experimental, not smoke-gated |

- KDE asks three portal dialogs on first start: Input Capture, and Remote Control twice (input emulation and clipboard).
- Linux clipboard sharing needs the `org.freedesktop.portal.RemoteDesktop` portal. GNOME and KDE have it; sway, Hyprland and X11 sessions don't, so clipboard sharing is off there.
- The KDE, sway, Hyprland and X11 runs were nested or headless sessions on one Linux machine, each paired with a Windows 11 machine.

Backends are cargo features (`Cargo.toml`), e.g. `cargo build --no-default-features --features layer_shell_capture,wlroots_emulation`.

## Troubleshooting

`mimouse cli doctor` checks the service, config, key file permissions, firewall and every client (reachable over DTLS/TLS, trusted in both directions) and prints a fix for each failure. It exits non-zero when any check fails; `--json` gives machine-readable output. `doctor` and `mimouse cli discover` give up on external tools (tailscale, ufw, firewall-cmd, netsh, socketfilterfw) after 10 s: doctor reports a warning naming the command, and discover fails with the same message.

An unauthorized peer shows up in the log with the command to approve it (`mimouse cli authorize-key <name> <fingerprint>`). Desktop notices about unknown devices are shown at most once a minute; the next notice says how many more attempts there were.

Outgoing connections (input and clipboard) accept only the certificate pinned in the client's `fingerprint` key. Pairing sets it. For hand-written entries and Lan Mouse peers it is recorded on the first successful connection and logged. A different certificate later is refused and logged; `mimouse cli doctor` shows the pin and fails on a mismatch. If a peer legitimately got a new certificate, delete its `fingerprint` line (or pair again) to re-pin.

Removing a key (`mimouse cli remove-authorized-key`, or deleting it from `authorized_fingerprints`) also closes that peer's open input and clipboard connections within about half a second, and its next connection attempt is refused.


### Known issues

- wlroots without libei on the receiving side ignores modifiers unless the sender uses layer-shell.
- Wayfire needs the `shortcuts-inhibit` plugin.
- Windows hides the cursor while receiving if no physical mouse is attached.
- GNOME can stop accepting emulated input ([lan-mouse#193](https://github.com/feschber/lan-mouse/issues/193)); run `mimouse cli enable-emulation`.

## Telemetry

Off by default. Turn it on with `mimouse cli telemetry on` (or `telemetry = true` in `config.toml`); `mimouse cli telemetry off` (or `telemetry = false`) turns it off and deletes the install id, the counts and the queue on this machine, and asks the collector to delete what it stored for the install. Both take effect without a restart.

When on, MiMouse sends one HTTPS request a day (completed UTC days only, at most the last 7 if uploads failed), plus one at start if a crash record is waiting. Fields (schema: `telemetry-collector/schema.json`):
- a random install id (UUID v4, created when you turn it on), schema version, MiMouse version and commit, OS family and version, Linux desktop (`XDG_CURRENT_DESKTOP`) and session type
- per day: capture and emulation backend in use and whether it was configured or picked automatically; counts of reconnects, ping timeouts, watchdog key releases, capture and emulation disables, permission-dialog waits (<10 s, <60 s, >=60 s), clipboard backend failures; switches by edge, hotkey and `cli focus`; number of configured peers; clipboard and file transfers by size bucket (<1 KiB, <64 KiB, <1 MiB, <64 MiB, >=64 MiB); CLI commands by name, without arguments
- crashes (at most 8): `file:line` relative to `src/`, the panic message only if it is a fixed string (otherwise `<formatted>`), and the backends in use

Never sent: input events or key codes, clipboard or file contents or names, host names, IP or MAC addresses, certificate fingerprints, user names, paths, `enter_hook` commands, or any other config value. Crashes recorded while telemetry is off are never uploaded; `mimouse cli telemetry show` and `mimouse cli doctor` list them.

`mimouse cli telemetry status` shows the install id, endpoint and queue; `mimouse cli telemetry show` prints the exact JSON of the next upload. Official release builds send to the MiMouse collector; other builds have no endpoint unless you set `telemetry_endpoint = "https://..."` (a base URL; https only). HTTP proxies (`HTTPS_PROXY`) are not supported.

The collector (`telemetry-collector/`, a Cloudflare Worker + D1) can also be run yourself. It stores only the fields listed above plus the time the batch was received, never your IP address or request headers, and deletes everything after 90 days. `mimouse cli telemetry off` sends `DELETE /v1/installs/<install id>`, which removes every row stored for this install.

## Development

```sh
cargo fmt && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
git config core.hooksPath .githooks   # optional pre-commit: fmt, clippy, test
```

See [CHANGELOG.md](CHANGELOG.md) for release notes, [AGENTS.md](AGENTS.md) for architecture, [CONTRIBUTING.md](CONTRIBUTING.md) to contribute and [SECURITY.md](SECURITY.md) to report vulnerabilities privately.

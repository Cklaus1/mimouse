# Changelog

All notable changes to MiMouse. Versions follow [Semantic Versioning](https://semver.org/).

## [1.0.0] - Unreleased

First MiMouse release. MiMouse is originally forked from [Lan Mouse](https://github.com/feschber/lan-mouse) 0.11.0 (commit `0d2190e7`) and is developed independently from it. The input protocol is unchanged, so MiMouse and Lan Mouse 0.11 peers can share input. Features marked *MiMouse peers* need MiMouse on both sides.

### Removed

- The GTK frontend. MiMouse is a single background binary configured by `config.toml` and `mimouse cli`.

### Added

- Clipboard sharing over TLS on the MiMouse TCP port: text, HTML, images, and copied files up to `max_file_size` (default 100 MiB) in total. Linux through the xdg-desktop-portal Clipboard interface, plus Windows. Secrets marked by password managers are not shared. *MiMouse peers.*
- Pairing with a 6-digit code (`mimouse cli pair <host> --position <side>` on one machine, `mimouse cli pair --accept` on the other). `mimouse cli discover` lists tailnet machines that answer on the MiMouse port.
- `mimouse cli doctor`: checks the service, the config, key file permissions, the firewall, and each client's reachability and trust, and prints a fix for each failure. `--json` gives machine-readable output.
- `mimouse cli config [--json]`: shows each key's effective value and where it comes from (flag, file, default or CLI), plus changes on disk that wait for a restart.
- `mimouse cli watch`, and `--json` output and non-zero exit codes for every CLI command.
- Machine hotkeys (`hotkey` per client, `local_hotkey`) and `mimouse cli focus <id>`.
- Control returns to the local machine on a Ctrl double-tap.
- Per-client edge ranges (`range = [start, end]`).
- The pointer enters the other machine at the point where it left. *MiMouse peers.*
- Connections to active clients are made at startup and retried with a 1 to 30 s backoff. Control returns when the controlled machine disappears, and the keys it held are released.
- Notifications on each switch of control, on portal approval waits, and on connection attempts from unknown certificates. While it controls another machine, the controlling machine doesn't idle or lock.
- GNOME: `mimouse cli list` shows `awaiting_approval` while a portal dialog waits. With xdg-desktop-portal-gnome 51 or later the InputCapture grant is remembered.
- `audit.jsonl`: every state-changing CLI or IPC request is logged, rotated at 10 MiB.
- Config validation: invalid ranges, duplicate or conflicting hotkeys, `port = 0`, `max_file_size = 0` and a non-HTTPS `telemetry_endpoint` are rejected and the error names the client. An invalid file is never overwritten. MiMouse keeps running on `config.toml.last-good`.
- Config write-backs edit `config.toml` in place, keeping comments, key order and unknown keys. A save merges with changes made to the file in the meantime.
- Opt-in telemetry, off by default: `mimouse cli telemetry on|off|status|show`. See the README for what is sent.
- Packages: `.deb` and `.rpm` (with a systemd user unit and a firewalld service), a Windows `.exe` with version information, and Scoop, winget and Homebrew manifests.

### Fixed

- Windows: the UDP listener no longer dies on `WSAECONNRESET` ([lan-mouse#262](https://github.com/feschber/lan-mouse/issues/262)).
- macOS: modifier shortcuts ([lan-mouse#450](https://github.com/feschber/lan-mouse/issues/450)) and scroll direction ([lan-mouse#481](https://github.com/feschber/lan-mouse/issues/481)).
- Weak networks ([lan-mouse#307](https://github.com/feschber/lan-mouse/issues/307)): undecodable datagrams are skipped, and short bursts of packet loss no longer drop the connection.
- Keys and modifiers are released on every way control can end, including disconnects ([lan-mouse#205](https://github.com/feschber/lan-mouse/issues/205)).
- wlroots (sway, Hyprland): the virtual keyboard no longer loops on keymap uploads, and an empty (`NO_KEYMAP`) keymap no longer ends the connection.
- Malformed datagrams, DTLS certificate chains and IPC requests are rejected instead of panicking. Event streams of a destroyed capture are dropped. Windows drops input that `SendInput` refuses instead of retrying it forever.
- Clipboard: updates arrive in order, the newest wins, and file names are de-duplicated after Windows sanitizing.

### Security

- The certificate key `mimouse.pem` is owner-only (0600, or an owner-only DACL on Windows), and looser permissions are repaired at startup.
- The Windows control channel is a per-user named pipe with an owner-only DACL. The CLI refuses a pipe owned by another user.
- Each client's server certificate is pinned. Revoking a key closes that peer's input and clipboard connections.
- DTLS handshakes time out, and the number of connections and buffered datagrams is capped. The `tailscale` and firewall tools run by `doctor`, `discover` and `pair` time out and are killed.

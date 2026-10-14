# Spec: product review fixes

Make MiMouse dependable day to day, match what Mouse Without Borders users expect, and make setup possible without hand-editing fingerprints. Source: product review 2026-10-11 (session issues, code FIXMEs, upstream Lan Mouse issues ranked by reactions).

## Ground truth (2026-10-11, branch `product-review` from 8d4cd47)

- Outgoing DTLS connections are made lazily inside `MiMouseConnection::send` (`src/connect.rs:122-166`), so the first crossing after a start always fails with `NotConnected`.
- A dropped connection (`receive_loop` -> `disconnect`, `src/connect.rs:247-285`) does not tell the capture task; capture is released only when a later send fails.
- GNOME 50: InputCapture uses legacy `CreateSession` (`input-capture/src/libei.rs:162-172`); xdg-desktop-portal-gnome 50 has no InputCapture persistence (v2 `CreateSession2`/`Start` with `restore_token` arrives in 51). ashpd 0.13.9 already has the v2 API. RemoteDesktop (emulation, clipboard) already restores tokens.
- Wire: `Enter` is `[8, pos]`. Lan Mouse decodes from a reused 21-byte buffer and ignores trailing bytes; an unknown event id drops its connection.
- Every active capture backend knows the crossing point (Windows `ENTRY_POINT`, portal `Activated.cursor_position`, layer-shell pointer enter, macOS `event.location`); X11 capture is unimplemented.
- Emulation can place the pointer absolutely on Windows, libei (mutter absolute device), wlroots, X11, macOS; not on the xdg RemoteDesktop D-Bus fallback.
- Windows LL hook and the macOS tap see keys while the pointer is local; GNOME InputCapture sees keys only after a barrier activation.
- Upstream bugs present in the fork: macOS emulation modifiers (#450) and scroll sign (#481); key-ups skipped when capture ends outside `release_capture` (#205); weak-network disconnects (#307: listener drops the connection on one undecodable datagram, 2 s ping window, 1 s emulation watchdog).

## Principles

- Lan Mouse 0.11 peers keep working: no new event ids, datagrams <= 21 bytes.
- No new third-party crates unless an item says so; `ashpd::zbus`, `windows`, `toml_edit` are already in the tree.
- Every behaviour change has a test where one can fail honestly; platform code that cannot run here is cross-compiled (Windows, macOS) and listed as unverified on hardware.

## Items

### R: reliability

**R1 GNOME approval without surprises.** Use InputCapture v2 (`create_session2` + `start` with `persist_mode` and a restore token in `~/.cache/mimouse/input-capture.token`) when the portal offers it, legacy otherwise. Do not rebuild the capture session for changes that do not move a barrier (hot-reload of hostnames, ips, ports, hooks). While a portal waits for approval, report `awaiting_approval` for capture/emulation in `cli list --json` and `watch --json`, log a warning every 30 s, and raise a notification (P4).
Accept: unit test that non-barrier config changes do not trigger a capture rebuild; `cli list --json` shows `capture: "awaiting_approval"` right after a GNOME restart (smoke); v2 path compiles against ashpd and is chosen only when `version >= 2`.

**R2 Connect at startup.** Connect to every active client when the service starts, when a client is activated, and keep retrying a dropped connection with backoff (1 s doubling to 30 s) while the client stays active.
Accept: after a restart the first crossing reaches the peer (smoke: no `releasing capture: not connected`); test for the backoff schedule.

**R3 Give control back when the peer drops.** When the connection to the client currently being controlled closes (ping timeout or recv error), release capture with the normal key-up path.
Accept: test or smoke where the peer service is stopped while controlled and the local cursor is returned within the ping window.

**R4 Private key permissions on Windows.** Restrict `mimouse.pem` to the current user (protected DACL, owner only), on creation and when an existing file is found readable by others (`src/crypto.rs:67`).
Accept: cross-compiled; on megapc `icacls mimouse.pem` lists only the user (smoke).

**R5 Saving keeps the user's file.** `write_back` edits the existing document with `toml_edit` so comments, order and unknown keys survive; only changed values are rewritten.
Accept: test that a commented config round-trips byte-identical after a no-op save and keeps comments after a client change.

### P: Mouse Without Borders parity

**P1 Machine hotkeys.** Per-client `hotkey = ["KeyLeftCtrl", "KeyLeftAlt", "Key1"]`. Pressed while controlling another machine: switch to that client (or back to local if it is the local hotkey `local_hotkey`). Pressed while local: Windows and macOS start capture for that client directly; GNOME uses `mimouse cli focus <id>` (bind it as a GNOME custom shortcut) which pushes the pointer into that client's barrier with the local emulation session. `cli focus <id>` works on every backend.
Accept: tests for hotkey matching (chord, no false trigger on superset); smoke on megapc: hotkey moves control to gpumaster and back. GNOME barrier push is a spike: if mutter ignores barriers for emulated motion, `focus` returns a clear error on GNOME and the README says so.

**P2 Cursor appears where it crossed.** Capture backends report the crossing point as a fraction of the edge; the sender sends `EnterAt(pos, fraction)` (done: `src/proto.rs`); the receiver places the pointer at that fraction of its matching edge (virtual-desktop bounds, clamped to a real display). Lan Mouse peers (plain `Enter`) keep today's behaviour.
Accept: proto tests (done); tests for fraction <-> point mapping incl. multi-monitor clamping; smoke megapc -> gpumaster where the receiver logs the placed point near the same fraction.

**P3 Copy and paste files.** Copying files (no directories) on Windows (`CF_HDROP`) or Linux (`text/uri-list` / `x-special/gnome-copied-files`) sends them, up to `max_file_size` total (default 100 MiB, MWB's limit), to the client when the cursor enters it; the receiver stores them under the cache dir and puts them on its clipboard as files.
Accept: framing tests (size cap, names with path separators rejected); smoke: copy a file on megapc, paste on gpumaster in a terminal-readable location, checksum matches, and the reverse.

**P4 Show which machine has control.** On every switch, a short notification "Controlling <host>" / "Back on <local>" (Linux: org.freedesktop.Notifications via `ashpd::zbus`, replacing the previous one; Windows: tray icon balloon via `Shell_NotifyIcon`; macOS: `osascript display notification`). `notify_switch = false` turns it off. Also used for R1 approval waits and unauthorized connection attempts.
Accept: smoke on gpumaster (notification visible in `gdbus monitor`) and megapc (balloon API returns success).

**P5 Edge ranges.** Per-client `range = [start, end]` in percent of the edge; only crossings inside the range switch. Enforced by each capture backend (libei: shorter barriers; layer-shell: sized surface; Windows/macOS: range check at activation).
Accept: tests for range checks and barrier segment computation; smoke on megapc with a range excluding y=600 does not switch.

**P6 Keep the controlling machine awake.** While this machine forwards input, inhibit idle/lock: GNOME Inhibit portal (`idle` flag), Windows `SetThreadExecutionState` + a zero-length injected move every 30 s, macOS `IOPMAssertion` (PreventUserIdleDisplaySleep). Released when control returns.
Accept: smoke on gpumaster (`gnome-session-inhibit --list` shows MiMouse while controlling) and megapc (`powercfg /requests` lists mimouse.exe).

### S: setup

**S1 Pair with a short code.** `mimouse cli pair <host> --position <side>` on one machine and `mimouse cli pair --accept` on the other: the two services exchange certificates over the existing TLS port (pairing frames accepted from unknown peers only while `--accept` is waiting, 120 s), both print the same 6-digit code derived from both fingerprints, and each side trusts the other only after its user confirms the code. The initiator adds the client; the acceptor adds the reverse side.
Accept: tests for code derivation (order independent, differs on any fingerprint change) and for the acceptance window; smoke: pair gpumaster and megapc from scratch with temporary configs.

**S1b Find machines on Tailscale.** `mimouse cli discover` lists online tailnet peers (`tailscale status --json`) that answer on the MiMouse port, with LAN addresses preferred when both exist. No mDNS. `pair` accepts a discovered name.
Accept: parser test on a captured status JSON; smoke on gpumaster lists megapc and not the phone.

**S2 `mimouse cli doctor`.** Checks and prints pass/fail with a fix line: service running, config parses, key file permissions, firewall rule for the port (ufw, firewalld, Windows Firewall), each client resolves and answers (DTLS + TLS), each side trusts the other, capture/emulation state (incl. awaiting approval). `--json` supported.
Accept: smoke on both machines shows all green; with the peer stopped, the reachability check fails with the address tried.

**S3 Packages.** `.deb` and `.rpm` with the systemd user unit (cargo-deb / cargo-generate-rpm metadata, built in the release workflow), Scoop and winget manifests and a Homebrew formula under `packaging/`. Windows binaries stay unsigned (decided by user). [needs-human: publishing to winget/Scoop/Homebrew repositories needs accounts.]
Accept: `cargo deb` builds locally and the package installs the binary and unit; manifests validate (`winget validate` if available, JSON schema otherwise).

### U: upstream bugs present in the fork

**U1 macOS modifiers (#450).** Modifier changes post flag-only events with the right keycode (no phantom 'A'), modifiers do not enter the key-repeat task, `Mod5` mapped.
**U2 macOS scroll direction (#481).** Flip the sign in emulation and capture together so mac<->mac stays correct.
**U3 Key-ups on every capture end (#205).** Every path that ends capture (compositor deactivation, capture errors, send failure) goes through the key-up + modifier-reset path; libei handles `Deactivated`/`Disabled`.
**U4 Weak networks (#307).** Listener skips undecodable datagrams instead of dropping the connection (upstream 82766cd); ping window and emulation watchdog tolerate short loss bursts (miss 3 of 4 pings before closing; release keys after 3 s silence rather than 1 s) while keeping stuck keys bounded.
Accept U1-U4: unit tests on extracted pure functions (flag mapping, scroll sign, watchdog decision, listener decode-skip); macOS unverified on hardware.

## Decisions

| ID | Question | Decision | Class |
|---|---|---|---|
| D1 | How to carry the crossing point | Two trailing bytes on `Enter` (same id), u16 fraction | engineering, done |
| D2 | Hotkey local->remote on GNOME | `cli focus` + barrier push via local libei emulation; spike, documented fallback | engineering |
| D3 | When files cross | On cursor-enter of the client, not on every copy; 100 MiB cap | engineering |
| D4 | Notification mechanism | Native per OS, no new crate; on by default, `notify_switch` | engineering |
| D5 | Discovery | Tailscale (`tailscale status --json`), no mDNS | decided by user |
| D6 | Pair trust model | Short code shown on both sides, explicit confirm on both; tailnet membership never implies trust | engineering |
| D7 | Weak-network thresholds | close after 3 of 4 missed pings window (2 s -> 4 s worst case), emulation watchdog 3 s | engineering |
| D8 | Package publication; code signing | publication not done here; no code-signing certificate, Windows binaries unsigned | publication needs-human; signing decided by user |

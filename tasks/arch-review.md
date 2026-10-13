# Spec: architecture review fixes

Close the crash, trust and data-loss holes found by the architecture review, then remove the structural causes behind them. Source: six-slice source review 2026-10-12 (service core, network security, clipboard + platform, config/IPC/CLI, input capture, input emulation) of branch `product-review` at c65ccd8. 59 reviewer findings merged into 54 items (overlaps folded together; A-arch-3..5 synthesized from the reviewers' architecture notes).

Status per item: **verified** = checked against the source by the integrator (file:line below); **reported** = reviewer evidence only, re-check before fixing. Nothing here was reproduced at runtime.

## Ground truth (verified 2026-10-12, c65ccd8)

- Release builds use `panic = "abort"` (`Cargo.toml:16`): every `panic!`/`assert!`/`expect` reachable from peer input takes down the whole service (input, clipboard, IPC).
- The DTLS listener asserts a single peer certificate (`src/listen.rs:82`); outgoing DTLS dials with `insecure_skip_verify: true` and no `verify_peer_certificate` (`src/connect.rs:58-64`), racing every resolved address (`connect_any`, 75-94). The clipboard TLS path verifies both sides.
- `InputCapture::poll_next` copies an event into `pending` for every id at a position (`input-capture/src/lib.rs:403-413`); `destroy` (211-235) never purges `pending`; the capture task's `get_type` expects the handle to exist (`src/capture.rs:291-295`).
- `add_incoming` creates an `EnterOnly` capture with range `None` (`src/service.rs:841`); `EdgeRange::hull` returns `None` (whole edge) if either side is `None` (`input-capture/src/lib.rs:126-127`).
- IPC `send` awaits `write_all` per connection inline (`src/ipc/listen.rs:139-154`), called from the service `select!` loop.
- `Config::new` keeps `config_toml = None` when the file fails validation (`src/config.rs:412-416`); `write_back` uses `unwrap_or_default()` (640) and `merge_config` only deserializes the disk document (686), then `merge_table` removes every key missing from memory (779-781).
- Vendored `webrtc-util` UDP listener inserts each remote address into `conns` (`vendor/webrtc-util/src/conn/conn_udp_listener.rs:236-239`) and never removes it; `UdpConn::close` is a no-op (293-295).
- Windows `send_input_safe` retries `SendInput` in an unbounded busy loop (`input-emulation/src/windows.rs:166-174`).
- Emulation auto-selection ends with `Backend::Dummy` (`input-emulation/src/lib.rs:142`), which always succeeds and logs every received event at `info` (`input-emulation/src/dummy.rs:24`); `NoAvailableBackend` (154) is unreachable.
- wlroots `consume` panics on a conversion failure of peer values and on `WaylandError::Protocol` (`input-emulation/src/wlroots.rs:216-218, 224`).

## Principles

- A peer, authorized or not, must never be able to abort the service; peer values are validated once at decode.
- Lan Mouse 0.11 wire compatibility stays (no new event ids, datagrams <= 21 bytes).
- No blocking calls on the LocalSet thread (AGENTS.md); no typed text or key codes in logs at `info`.
- Every fix gets a failing-before test where one can fail honestly; Windows/macOS code is cross-compiled and listed as unverified on hardware.

## Items

### P1: crashes, trust, data loss (fix first)

**A1 Multi-cert DTLS chain aborts the service.** verified. `listen.rs:82` `assert!(certs.len() == 1)` runs in `verify_peer_certificate` before authorization, so any host reaching the UDP port aborts the service with one handshake (repeatable at every restart). Fix: `let [cert] = certs else { return Err(ErrVerifyDataMismatch) }`; also replace `expect("cert")` at 135.
Accept: unit test calling the verifier with 0 and 2 certs returns `Err`, no panic.

**A2 Verify the server on outgoing DTLS.** verified. Keystrokes go to whichever endpoint answers first at any of the client's addresses (DHCP reuse, ARP/DNS spoofing on LAN or tailnet). Pairing already stores the peer fingerprint on both sides. Fix: set `verify_peer_certificate` against `authorized_fingerprints` (per client once A-arch-2 lands), reject and log unknown servers; `doctor` reports the mismatch.
Accept: test that `connect` to a listener with an unauthorized cert fails; smoke gpumaster <-> megapc still connects.

**A3 Write-back erases the config after a validation error.** verified. A file that deserializes but fails `validate()` (e.g. `range = [80, 20]`) starts the service with `config_toml = None`; the next mutating CLI request or pairing writes defaults + one change over it, deleting `[[clients]]`, `authorized_fingerprints` and settings. After a bad hot-reload the old in-memory config overwrites the user's edits the same way. Fix: refuse write-back unless the disk document validates and matches the snapshot memory was loaded from (three-way: base, disk, memory); never write from `None`.
Accept: test with an invalid-range file: `write_back` returns an error and the file is byte-identical.

**A4 Queued event for a destroyed capture panics.** verified (code path), not reproduced. With two ids at one edge (a Default client plus the EnterOnly capture of the same peer's inbound connection, i.e. any two-way setup like gpumaster/megapc), `destroy` leaves copies in `pending`; the next `next()` yields the dead handle and `get_type` panics. Trigger: the controlled peer's inbound connection hits the emulation watchdog. Fix: `destroy`/`release` retain only live ids in `pending`; `get_type` miss logs and drops the event.
Accept: input-capture unit test with a dummy backend: two ids at one position, destroy one with an event queued, `poll_next` never yields it.

**A5 One stalled IPC reader freezes the service.** verified. `cli watch | less` left unscrolled, a `watch` on Ctrl-Z, or a hung script fills the socket buffer and `write_all` pends inside the main `select!`: no IPC, capture release, notifier, DNS, pairing or reload until it drains. Fix: per-connection bounded outbound queue drained by its own `spawn_local` writer; `send` only enqueues and drops a connection whose queue overflows (it reconnects and gets `sync_events`).
Accept: test with a connected reader that never reads: requests from a second connection still get replies.

**A6 UDP listener never evicts peers.** verified. Each new source address gets a `UdpConn` with an unbounded buffer (up to 4 MiB) that is never removed; after a failed/rejected/timed-out handshake nothing reads it. One unauthenticated sender per source port holds 4 MiB each; every legitimate reconnect leaks an entry. Fix in `vendor/webrtc-util`: remove the entry on `UdpConn::close` and on handshake failure (DTLS conn drop), cap entries and per-conn buffer.
Accept: vendored test: accept, close, map is empty; buffer cap enforced.

**A7 Windows SendInput spins the runtime.** verified (loop), refusal cases reported. `SendInput` returns 0 while the secure desktop (lock screen, UAC) or UIPI blocks injection; the loop then holds the LocalSet thread at 100 % CPU, so pings stop, the peer disconnects and `cli` hangs. `consume` also discards errors (`// FIXME Ok(())`, windows.rs:82). Fix: one call; on 0 return `EmulationError` (or drop with a rate-limited warning).
Accept: cross-compiled; smoke on megapc: lock the screen while controlled from gpumaster, service stays responsive (`cli list` answers).

**A8 Dummy fallback logs remote keystrokes.** verified. When every real backend fails at start (macOS before Accessibility, portal not up at login, wlroots bind failure) emulation silently becomes Dummy: status Enabled, `Pong(true)`, and every peer key code goes to the journal/launchd log at `info`. Fix: drop Dummy from the auto list so `NoAvailableBackend` disables emulation visibly (re-enable later via `enable-emulation`/A-recovery); Dummy only when configured, logging at `debug` without key codes.
Accept: test that auto-selection with all backends failing returns `NoAvailableBackend`.

**A9 One malformed datagram aborts a wlroots receiver.** verified (panic sites), decode path reported. `proto.rs` passes raw `Button.state` / `Axis.axis` through (`src/proto.rs:161-174`); wlroots panics on anything other than 0/1 and on any protocol error. Fix: validate at decode (drop the datagram, keep the connection, per #307 lesson); wlroots maps conversion and protocol errors to `EmulationError`.
Accept: proto test: `state: 2` / `axis: 2` decode to an error; wlroots `consume` never panics (code review, no compositor here).

**A10 Inbound connection disables the edge range.** verified. The EnterOnly capture for an inbound peer has range `None`, so the merged barrier becomes the whole edge and a configured `range = [0, 40]` stops limiting crossings as soon as that peer connects in (two-way setups always do). Fix: give the EnterOnly capture the configured client's range at that position (or exclude EnterOnly ids from the hull and only use them for release).
Accept: input-capture test: Default with `[0,40]` + EnterOnly `None` at the same edge keeps barrier `[0,40]`; smoke on megapc with P5 range after gpumaster has entered once.

**A11 Clipboard secrets.** reported (`src/clipboard/windows.rs:75-84`, `portal.rs:185`, `mod.rs:385-389`). Every local copy goes to all active clients at once, without honoring password-manager markers (Windows `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory=0`, Linux `x-kde-passwordManagerHint`), and clears are never propagated, so a KeePassXC password that auto-clears after 12 s stays on every peer and in its clipboard history. Fix: skip marked content; propagate an empty clipboard as a clear; README states that copies go to all active clients.
Accept: Windows/portal unit tests for marker detection; smoke: KeePassXC copy on gpumaster does not reach megapc.

### P2: correctness and robustness

Network and trust:
- **B1 Handshakes block accept.** reported (`src/listen.rs:122-137`). The DTLS handshake runs inline in the accept loop, which drops it every 2 s; one non-answering ClientHello (spoofable) delays every other peer, and a lossy real handshake past 2 s never completes (works against U4). Fix: accept raw `UdpConn`s, handshake in `spawn_local` with its own timeout.
- **B2 Revoking a key keeps the open session.** reported (`src/listen.rs:128-138`, `src/service.rs:906-918`, 482-486). `remove-authorized-key` and reloads only update the map; a connected controller keeps full input control while it pings. Fix: store the fingerprint with each listener conn; on revocation close matching conns.
- **B3 Unbounded TCP connections; EMFILE spin.** reported (`src/clipboard/transport.rs:303-317`). No cap on pre-auth connections (5 s ClientHello, 10 s refuse); at the NOFILE limit `accept` fails instantly and the loop `continue`s at 100 % CPU. Fix: semaphore on live connections, backoff on accept errors.
- **B4 Pairing trusts the peer's self-reported name.** reported (`src/pair.rs:758-765`, `src/service.rs:609-631`). Two machines named alike (`ubuntu`, fallback `unknown`) collapse into one client; with A2 unfixed input then races both. Fix: key the result on the fingerprint; name only as a label, de-duplicated.
- **B5 Unauthorized-attempt notices are per fingerprint.** reported (`src/notify.rs:70-75`, `src/emulation.rs:183-185`). A fresh cert per handshake yields one desktop notice each (one `osascript` per notice on macOS) and grows `rejected_connections` forever. Fix: global rate limit with a summarized count; bounded map.

Service and config:
- **B6 SIGTERM skips cleanup.** reported (`src/service.rs:209-218`). Only `ctrl_c` (SIGINT) breaks the loop; `systemctl --user stop/restart` sends SIGTERM, so no Leave/key-ups go to the controlled peer and emulation never releases held keys. Fix: also select on `SignalKind::terminate()` and `hangup()`.
- **B7 Mutations stay live after a failed save.** reported (`src/service.rs:299-365`, 432-454, 605-635). Runtime state changes first, then `save_config()?` fails (invalid or read-only config, e.g. Nix/home-manager symlink): the CLI reports failure while `authorize-key` already trusts the key, `add-client` duplicates on retry, `paired()` returns `PairFailed` after trusting. Fix: check writability/validity (A3) before mutating, or apply only after a successful save.
- **B8 Slab handle reuse.** reported (`src/client.rs:144-146`, `src/service.rs:494-503`, 535-544, 780-786; `src/clipboard/mod.rs:273-277`, 396-420). Client ids swap on reload (LIFO reuse), so a GNOME shortcut bound to `cli focus <id>` moves control to another machine; an in-flight DNS result lands on whichever client reused the handle; clipboard `delivered`/`senders` keep a removed client's state for the next one. Reloads also fire when nothing changed (vim rename-on-write). Fix: stable ids (keep a matched client's handle; never reuse a handle within a run, e.g. a generation counter); DNS results carry the hostname and are dropped on mismatch; clipboard gets `ClientRemoved`.
- **B9 Undecodable IPC request gets no reply; CLI waits forever.** reported (`src/ipc/listen.rs:122-128`, 246; `src/cli.rs:374-381`). Version skew after an upgrade (new CLI, old service) hangs `cli focus` per shortcut press. Fix: reply with an error carrying the seq when it can be parsed, else close the connection; CLI reply timeout.

Capture:
- **B10 Events go to every client at a position.** reported (`input-capture/src/lib.rs:386-412`, `src/capture.rs:530-539`, `src/emulation.rs:174`). With two clients on one edge (`[0,50]`/`[50,100]`), one crossing sends Enter and all input to both; a crossing outside every range activates all. `focus` only routes `Begin`. Fix: route by `at` (crossing fraction) to the ids whose range contains it, store `active_ids` until End/release; release if none match (see A-arch-3).
- **B11 layer-shell drops the active grab.** reported (`input-capture/src/layer_shell.rs:572-578`, 849-878). Removing any window at a position clears `focused` even if the grab is on another edge, leaving pointer lock/inhibitor in place; Button/Axis then `unwrap()` `None` and abort. Fix: clear only if the focused window is the removed one.
- **B12 macOS display bounds never shrink.** reported (`input-capture/src/macos.rs:131-139`, 286-297). After unplugging a monitor, `xmax` keeps the old edge so the Right client is unreachable until restart. Fix: recompute from scratch on reconfiguration.
- **B13 libei rebuilds the portal session per barrier change.** reported (`input-capture/src/libei.rs:327`, 378-408). Startup Create(Left)+Create(Right) yields two sessions, i.e. two GNOME 50 approval dialogs; incoming captures churn it further. Fix: drain and batch pending notifies; with A10/B10 incoming EnterOnly captures no longer change barriers.
- **B14 OS callbacks block on bounded channels.** reported (`input-capture/src/windows/event_thread.rs:118-119`, 330, 399, 484; `macos.rs:518`, 563-664). `blocking_send(..).unwrap()` in the LL hook panics inside `extern "system"` when the receiver is dropped first (field order `windows.rs:16-17`); macOS sends while holding a lock. Fix: `try_send` with drop-and-log; fix drop order.

Emulation:
- **B15 Mouse buttons stay pressed after disconnect.** reported (`input-emulation/src/lib.rs:157-171`, 248-270; `src/capture.rs:702-715`). Only keys are tracked; a peer lost mid-drag leaves LEFTDOWN held (macOS turns every later move into a drag). Fix: track buttons centrally like keys, release them on destroy/terminate and in `leave_events`.
- **B16 No self-recovery after compositor/EIS/portal loss.** reported (`src/emulation.rs:378-397`; `input-emulation/src/libei.rs:307-354`). After any backend error the receiver stays dead until `cli enable-emulation` runs on that machine. Fix: retry backend creation with backoff (1 s to 30 s), status `awaiting_approval`/`disabled` meanwhile.
- **B17 Windows horizontal scroll inverted.** reported (`input-emulation/src/windows.rs:238-247`). Both axes are negated; only axis 0 should be. Fix + test.
- **B18 Windows key repeat dies on any key-up.** reported (`input-emulation/src/windows.rs:64-76`, 146-158). Tapping Shift while holding `a` stops the repeat; modifiers repeat. Fix: reuse macOS `KeyPlanner` (`macos_logic.rs:83-118`) as a shared, tested planner.
- **B19 Sub-pixel motion and small scroll steps are truncated.** reported (`input-emulation/src/windows.rs:47-50`, `x11.rs:123`, `input-event/src/macos_scroll.rs:51-66`). Slow GNOME/libei movement does not move a Windows/X11 cursor; precision-wheel steps < 40 do nothing on macOS. Fix: per-handle remainder accumulators.
- **B20 wlroots start can block forever.** reported (`input-emulation/src/wlroots.rs:113-121`, 473). `blocking_dispatch` waits for a keymap that only comes if the seat has a keyboard capability (a KVM target without a keyboard), before IPC is up. Fix: bounded roundtrip; fall back to a default keymap.

Clipboard:
- **B21 An older transfer overwrites a newer copy.** reported (`src/clipboard/mod.rs:402-410`, 466-472). No ordering between directions: a slow file transfer finishing after the user's fresh copy on the receiver replaces it. Fix: generation/timestamp per update; receiver drops updates older than its last local change.
- **B22 Portal clipboard leaks RemoteDesktop sessions.** reported (`src/clipboard/portal.rs:131-214`, 107). Errors after session creation retry after 10 s without `session.close()` (one failed paste is enough); GNOME accumulates sessions. Fix: close on every exit path (guard).
- **B23 Windows broadcasts a stale clipboard at startup.** reported (`src/clipboard/windows.rs:86-90`). The initial `report()` pushes whatever is on the clipboard to all clients on every service restart. Fix: seed `current` without sending.
- **B24 Clipboard backend status invisible.** reported (`src/clipboard/portal.rs:96-107`, 129-145; `src/clipboard/mod.rs:409`). A pending or denied portal dialog is not in approval.rs; after NotGranted updates are ACKed to the peer and dropped. Fix: status watch like capture/emulation, shown in `cli list`/`doctor`; don't ACK when the backend is down.

### P3: cleanups and hardening

- **C1** Repeated `cli focus` for the same client within 1 s fails both requests (`src/capture.rs:588-594`): keep the pending focus.
- **C2** `Destroy` releases before updating bookkeeping (`src/capture.rs:416-424`): `remove_capture(h)` first, log the release error.
- **C3** Config write-back and audit IO block the LocalSet and truncate-then-write is not atomic (`src/config.rs:665-676`, `src/audit.rs:65-70`): temp file + rename in `spawn_blocking`.
- **C4** In-flight connect survives `Connector::stop` (`src/connect.rs:350-370`, 232-238): cancel attempts with the keeper or check activity in `establish`.
- **C5** Audit log stops recording when rotation fails (`src/audit.rs:67-73`): warn and append anyway.
- **C6** Clipboard backend set channel is unbounded (`src/clipboard/portal.rs:89-92`, `windows.rs:183-184`): latest-wins `watch`.
- **C7** Each client sender re-encodes the snapshot; inbound frames allocate up to ~149 MiB per connection (`src/clipboard/mod.rs:466-472`, `transport.rs:380-387`): encode once per digest, limit concurrent inbound transfers, stream files.
- **C8** Windows clipboard writer records `own_seq` even when it never wrote (`src/clipboard/windows.rs:209-233`), so the user's copy is skipped as "ours".
- **C9** `change-port` is never saved (`src/service.rs:674-681`): `Config::set_port` + `save_config`; `cli pair` uses the running port.
- **C10** `tailscale`/`firewall-cmd` calls in doctor/discover/pair have no timeout (`src/discover.rs:209`, `src/doctor.rs:486-523`): `timeout` + `kill_on_drop`.
- **C11** `--json` errors only for request failures; every failure exits 1 (`src/cli.rs:200-212`, `src/main.rs:38-41`): JSON error object for all `CliError`s, distinct exit codes for service-not-running / rejected / checks-failed.
- **C12** Hotkey final-key auto-repeat leaks to the remote (`input-capture/src/windows/event_thread.rs:386-421`, `macos.rs:618-640`): swallow repeats until key-up; share the state machine.
- **C13** libei `DeviceRemoved` clears only the absolute slot; paused devices still get events (`input-emulation/src/libei.rs:409-421`).
- **C14** wlroots reads its socket only in `enter_at` (`input-emulation/src/wlroots.rs:238-249`, 304-318): keymap/output updates never arrive with plain-`Enter` peers.

### Architecture

**A-arch-1 Client lifecycle component.** `Service` has ~27 fields and keeps "active ⇔ barrier ⇔ connection ⇔ hotkeys" by hand across `run`, `replace_clients`, `update_clients`, `handle_request`, `paired`, `update_pos` (`src/service.rs:195-204`, 543-545). Extract `Clients` owning client_manager + capture barrier + connector + hotkey sync + save, with stable ids (B8) and transactional apply-then-save (B7). Incoming registry becomes one map (today `incoming_conns` + `incoming_conn_info`, panic on divergence).

**A-arch-2 One trust policy module.** Peer verification exists five times (`src/listen.rs:78-109`, `connect.rs:57-64`, `clipboard/transport.rs:132-230`, `pair.rs:381-388, 841-930`, `doctor.rs:677-752`); the input path skipping server checks (A2) and revocation covering only per-connection paths (B2) both come from this. Add `crypto::peers` with `Policy::{Authorized, Pinned(fp), Any}` producing DTLS and rustls configs; identify peers by fingerprint everywhere (clipboard echo suppression uses IP today).

**A-arch-3 Per-client capture routing.** The `Capture` trait works per `Position`, while ranges and `focus` belong to clients; `InputCapture` merges ranges into one barrier and fans events out to all ids (A4, A10, B10, B13). Keep barriers per position, but make `InputCapture` the single router from (position, crossing fraction | focus) to one id, with EnterOnly ids excluded from barrier geometry.

**A-arch-4 Normalize emulation input once.** Release, repeat and unit conversion are per backend by convention: keys tracked centrally, buttons nowhere (B15), two repeat planners (B18), per-backend scroll sign and truncation (B17, B19), unvalidated peer values (A9). Add one layer in `InputEmulation` that validates (state/axis 0/1), tracks keys + buttons, accumulates remainders and plans repeats; backends get clean integer events.

**A-arch-5 Config source of truth.** `client_manager` is authoritative at runtime, `config_toml` mirrors it, and hot-reload makes the file authoritative again, with no record of which came last. Keep the snapshot the runtime was loaded from and write back as a three-way merge (base, disk, memory) that refuses on conflict or invalid disk state (A3, B7, B8).

## Order

1. A1, A6, A9, A4, A7 (peer- or OS-triggered aborts/spins), then A2, A5, A8, A3, A10, A11.
2. A-arch-2 with B1-B5; A-arch-3 with B10-B14; A-arch-4 with B15-B20; A-arch-1 + A-arch-5 with B6-B9.
3. B21-B24, then C items.

## Open decisions

| # | Question | Default if not decided |
|---|---|---|
| Q1 | A6: patch the vendored `webrtc-util` further or move to an upstream release with eviction | patch vendor (already forked for #812) |
| Q2 | A11: drop marked secrets entirely, or forward them with the hint so peers' clipboard managers skip history | drop |
| Q3 | B8: ids stable across restarts (persist in config) or only within a run | within a run; README says ids follow config order |

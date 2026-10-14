# Spec: public release (v1)

Ship MiMouse as a publicly supported product. Decided by the product owner on 2026-10-14; the decisions below are canon, not open questions.

## Decisions

| # | Question | Decision | Consequence |
|---|---|---|---|
| D1 | Release gate | All of `tasks/arch-review.md` (P1, P2, P3, architecture, T1-T8) plus this spec | v1 waits for the whole review |
| D2 | Relation to Lan Mouse | Hard fork: no upstream merges, no upstream PRs | Lan Mouse 0.11 protocol compatibility is kept as of today but no longer tracked; README stops promising it for future Lan Mouse versions |
| D3 | Lan Mouse security disclosure | Fix in MiMouse only | Release notes describe A1/A2/A6 fixes without naming Lan Mouse |
| D4 | Interface | CLI only (`mimouse cli`, pair, doctor, discover) | No GUI or tray work |
| D5 | Supported platforms | All Linux desktops (GNOME, KDE, wlroots: sway/Hyprland, X11) + Windows 10/11; macOS experimental | Each supported desktop needs a recorded smoke run (P2 below) |
| D6 | Windows code signing | Unsigned | README explains the SmartScreen prompt |
| D7 | Telemetry | Opt-in, reliability + usage, self-hosted endpoint | Spec T below |

## T: Telemetry

### Rules (invariants)
- **Off by default.** Nothing is sent unless the user turned it on (`telemetry = true` in `config.toml` or `mimouse cli telemetry on`). Turning it off deletes the install id and the local queue.
- **Never collected:** input events or key codes, clipboard or file contents or names, host names, IP addresses, MAC addresses, certificate fingerprints, user names, paths, `enter_hook` commands, config values other than those listed below. Panic messages are included only after removing anything matching a path, address or host name of a configured client.
- **Transparent:** `mimouse cli telemetry show` prints the exact JSON of the next upload; README lists every field.
- **Off the input path:** counters are in-memory increments; collection and upload never block the LocalSet or delay input forwarding (TR §28).

### Content (schema version 1)
- Identity: random install id (UUID v4, created at opt-in, stored in the config dir), schema version, MiMouse version (`CLAP_LONG_VERSION`), OS family + version, Linux desktop (`XDG_CURRENT_DESKTOP`) and session type.
  [REVISED: send `PKG_VERSION` + `SHORT_COMMIT`, not `CLAP_LONG_VERSION` (multi-line, includes branch, build time with the builder's timezone, rustc version).]
- Reliability: capture and emulation backend in use and whether it was configured, auto-selected or a fallback; panics (message, file:line, backend); count of reconnects, connection closes by missed pings, watchdog key releases, capture/emulation disable events, approval waits (duration bucket), clipboard backend failures.
- Usage (per day): switches of control, number of configured peers, clipboard transfers (count, size bucket), file transfers (count, size bucket), names of CLI commands used (no arguments), hotkey vs edge vs `cli focus` switches.
  [REVISED: "CLI commands used" counts IPC requests the service receives; `doctor` and `discover` run without the service and are counted only when the service is up (the CLI sends a `CountCommand` request, best effort, never blocking the command).]

### Behaviour
- **T-1 Client.** Counters in a `telemetry` module, persisted to `<cache dir>/mimouse/telemetry.json` at most once a minute and on shutdown. One upload per day (and at start if a crash record is waiting): HTTPS POST of one JSON batch, timeout 10 s, retry next day on failure, queue capped at 7 days. Reuse `tokio-rustls`/`rustls`; a root-certificate crate is the only new dependency (license check against `deny.toml`). Upload runs in its own task and is dropped on shutdown.
  [REVISED: no HTTP client in the tree, so the upload is a hand-written HTTP/1.1 POST over `tokio-rustls` (ring); no `HTTPS_PROXY` support (stated in README). Install id from `ring::rand::SystemRandom` (no `uuid` dependency). The runtime is current_thread + LocalSet and tokio has no `fs` feature: the `telemetry.json` write runs in `spawn_blocking`. Counters are atomics (bumped from the Windows hook / macOS tap threads too). Cache dir via `clipboard::files::cache_dir`. Root certificates: `rustls-native-certs` (MIT/Apache-2.0/ISC; `webpki-roots` is CDLA-Permissive-2.0, not on the allow-list).]
- **T-1a deny.toml + CI.** [NEW, Step 1] Migrate `deny.toml` to the current cargo-deny format (version 2), allow the licenses actually in the tree (incl. the project's own GPL-3.0-or-later, Unicode-3.0, Zlib if present), add a `cargo deny check licenses bans sources` job to `rust.yml` and to the AGENTS gate.
- **T-2 Crash capture.** Release builds use `panic = "abort"`, so a panic hook writes a crash record to the cache dir before abort; the next start uploads it if telemetry is on, otherwise `cli telemetry show` and `doctor` mention it.
  [REVISED: the record holds `file:line` with everything before `src/` or the crate root removed, the backend from a global atomic, and the message only when the payload is `&'static str`; formatted `String` payloads become `"<formatted>"` (e.g. `wlroots.rs` formats input events into panics). Writes are synchronous `std::fs` in the hook. SIGSEGV, OOM and stack overflow are not captured. Records created while telemetry was off are never uploaded later; they are only shown locally. The no-PII test includes a panic carrying an event.]
- **T-3 Endpoint.** The release workflow bakes the endpoint URL in at build time (`MIMOUSE_TELEMETRY_URL`); `telemetry_endpoint` in `config.toml` overrides it. Without either, telemetry can't be turned on and the CLI says why. Dev/CI builds have no endpoint.
  [REVISED: set `MIMOUSE_TELEMETRY_URL` only on tag builds (not `main-<sha>` pre-releases); `option_env!` returning `Some("")` counts as unset; Homebrew/source builds need `telemetry_endpoint`.]
- **T-4 CLI.** `mimouse cli telemetry on|off|status|show [--json]`; `on` prints what is collected and the endpoint. Changes are audited (`audit.jsonl`) and saved to `config.toml`.
  [REVISED: `status` prints the install id; `off` also sends a best-effort `DELETE /v1/installs/<id>` so server rows go too; README names that route. Editing `telemetry = false` in the file (hot reload) runs the same purge as `cli telemetry off`. `show` prints the queued batch (completed days + pending crash records), which is exactly what the next upload sends.]
- **T-5 Collector.** `telemetry-collector/`: Cloudflare Worker + D1. Validates the schema version and field set, rejects unknown fields, never stores the client IP or request headers, keeps rows 90 days, rate-limits per install id. Includes the D1 schema, a local `wrangler dev` test and a README with deploy steps. **needs-human:** Cloudflare account, domain and deployment.
  [REVISED: also disable Workers Logs/observability and request logging in `wrangler.toml`; a cron trigger deletes rows older than 90 days; implements `DELETE /v1/installs/<id>`. Tests run with `wrangler dev`/miniflare locally. Writing and testing the collector is in scope; account, domain and deployment stay needs-human.]
- **T-6 Docs.** README "Telemetry" section (opt-in, full field list, how to turn off, retention); AGENTS invariant: new fields only by updating the schema version, README list and the no-PII test.

### Acceptance
- Default config: no network connection other than peers (`ss`/`strace` during smoke, or a test with a failing endpoint that must not be contacted).
- A test builds a payload from a config and runtime state full of host names, IPs, fingerprints and paths and asserts none of them appear in the JSON.
- `cli telemetry show` output equals the bytes the next upload sends.
- Panic in a test binary → crash record on disk → uploaded on next start against a local collector (`wrangler dev` or a test HTTPS server).
- Collector rejects unknown fields and does not store the source IP.

## P: Public support

- **P1 Supported-desktop smoke.** Record a two-machine (or loopback) smoke for each D5 desktop: GNOME Wayland (gpumaster, done), Windows 11 (megapc, done), KDE Plasma Wayland (`kwin_wayland --virtual`), sway/Hyprland (`WLR_BACKENDS=headless`), X11 (Xvfb + a window manager). Each: capture start, crossing both ways, held-key release on disconnect, clipboard text. Results go into the README platform table with the date and version tested. Desktops that can't be run headless here: **needs-human** test on real hardware.
  [REVISED: X11 capture is a stub (`input-capture/src/x11.rs:13-15`), so the X11 row is receive-only: crossing into X11, held-key release, clipboard; README says "X11: receive only". A macOS row is listed as "experimental, not smoke-gated". An X11 capture backend is not in v1 (needs-human N4 below).]
  [SMOKE 2026-10-14 18:28-18:57, sway 1.11 headless (wlroots 0.19) nested on gpumaster as client `top` of megapc, port 4343, capture `layer-shell`, emulation `wlroots`; fix commit 9703a5b. Crossing megapc -> sway: enters at the edge (`entered at 677,1078 (bottom edge)`). Capture start + crossing sway -> megapc: a wlr virtual pointer pushed into sway's bottom edge, megapc logged `entered at 2441,1 (top edge)` and its cursor followed; moving back up, megapc's top edge entered sway (`entered at 1064,1078`), Ctrl double-tap on megapc returned control. Held-key release on disconnect FAILED at first: (1) a `NO_KEYMAP` event (seat's active keyboard had none) was uploaded to the virtual keyboard and sway killed the connection (`no_memory`); (2) after that fix, every keymap event was re-uploaded, wlroots re-broadcasts each upload, the loop flooded the socket and `consume` discarded events ("can't keep up"), the release among them; each `Capabilities` event also added a seat keyboard (multiplying the loop). Fixed in `input-emulation/src/wlroots.rs` (ignore NO_KEYMAP, upload only a changed keymap, one seat keyboard) + `input-capture/src/layer_shell.rs` (keep the seat keyboard so it is released). After: 3/3 rounds hold A, kill megapc's mimouse, release reaches the focused client within 6 s; 15 keymap events, 3 uploads. Clipboard on sway: disabled (no `org.freedesktop.portal.RemoteDesktop`; Linux clipboard is portal-only), README must say so. Sway row: pass except clipboard. Hyprland not smoked (not installed; `hyprland` candidate 0.53.3 in apt).]
  [SMOKE 2026-10-14 19:07-19:12, KWin 6.6.6 `--virtual` nested on gpumaster (own dbus + PipeWire) as client `top` of megapc, port 4343, capture `input-capture-portal`, emulation `libei`, clipboard via the KDE portal; build 9703a5b (sha 1276ca5ae22db470). All pass: the three portal dialogs (Input Capture, two Remote Control: emulation and clipboard) were accepted with Enter; crossing megapc -> KDE (`entered at 677,1078 (bottom edge)`) and back through KDE's InputCapture barrier (megapc `entered at 1721,1 (top edge)`); Ctrl double-tap release (`releasing capture: Ctrl double-tap`); held-key release on disconnect 2/2 rounds (A released ~6 s after killing megapc's mimouse, `releasing keys ... not responding!`, no libei errors); clipboard text KDE -> megapc (`Get-Clipboard` matched) and megapc -> KDE (`wl-paste` matched). Harness only: KWin ran with `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1` because the nested session's portal was not granted `zkde_screencast_unstable_v1`; xdg-desktop-portal-kde refuses every RemoteDesktop session without that global (PipeWire must be running), input-only included. Input was driven through KWin's own EIS (`org.kde.KWin.EIS.RemoteDesktop.connectToEIS`). KDE row: pass.]
  [SMOKE 2026-10-14 19:14-19:15, X11 (Xvfb :7 1920x1080 + openbox 3.6.1) on gpumaster as client `top` of megapc, port 4343, emulation `x11` (XTest), build 9703a5b (sha 1276ca5ae22db470). Crossing megapc -> X11: `entered at 677,1078 (bottom edge)`, X pointer at 677. Held-key release on disconnect 2/2 rounds (autorepeat off, xev: exactly one KeyPress A, then one KeyRelease within 6 s of killing megapc's mimouse; `releasing keys ... not responding!`). Capture: `disabled` as documented (stub). Clipboard: `disabled` (no `org.freedesktop.portal.RemoteDesktop` frontend on X11; Linux clipboard is portal-only), README must say so, as for sway. X11 row: receive-only pass, clipboard off.]
- **P2 README for strangers.** Install per platform (`.deb`, `.rpm`, Windows zip + SmartScreen note, from source), firewall per OS, first pairing in five steps, troubleshooting via `doctor`, supported platforms table (D5), Lan Mouse compatibility statement (D2), telemetry (T-6).
  [REVISED: the release ships a bare `mimouse-windows-x86_64.exe`, not a zip; P2 documents the .exe, SmartScreen, and macOS Gatekeeper quarantine (`xattr -d com.apple.quarantine`).]
  [REVISED 2026-10-14: also a Microsoft Defender note. A wave-1 build without any PE resource was quarantined on megapc as `Trojan:Win32/Bearfoos.B!ml` (ML heuristic, severity 5); the same code with a VERSIONINFO resource or with symbols kept passed both the write and the execution scan. W1 (build.rs, `winresource`) embeds VERSIONINFO in every Windows build. README tells users how to report a false positive (microsoft.com/wdsi) rather than to disable Defender. Owner decision 2026-10-14: no Defender exclusions on test machines, Windows stays unsigned (D6).]
- **P3 Issue intake.** `.github/ISSUE_TEMPLATE/` bug form asking for `mimouse --version`, OS/desktop, `mimouse cli doctor --json` and the log; feature form; `SECURITY.md` with a private reporting channel (GitHub private advisories); `CONTRIBUTING.md` with the gate commands from AGENTS.md.
- **P4 Versioning and release.** Semantic versioning starting at 1.0.0; `CHANGELOG.md` from this branch's history; merge `product-review` into `main`; tag; the existing release workflow publishes `.deb`, `.rpm` and the Windows zip. Publishing the tag: **needs-human** (go/no-go).
  [REVISED: bump `Cargo.toml`, `Cargo.lock` and the packaging manifests that `render-manifests.py` checks against the Cargo version to 1.0.0; CHANGELOG starts at the fork point (merge-base with Lan Mouse `0d2190e7`); `v0.11.0` already exists as an upstream tag, so the first MiMouse tag is `v1.0.0`; enable `rust.yml` CI on pull requests to `main` so `product-review` is checked before merge. Merging into `main` and pushing the tag are the go/no-go: needs-human.]
- **P5 Lan Mouse wording (D2).** README and `--help` say "originally forked from Lan Mouse" (GPL attribution stays); remove claims about tracking upstream.
  [REVISED: also remove `.github/FUNDING.yml` (it points at the Lan Mouse author; a hard fork must not imply his endorsement), update the winget locale text and README "Changes from upstream"; `--help` gets "originally forked from Lan Mouse" via clap `after_help` (there is no existing text to edit).]

## Order

1. `tasks/arch-review.md` in its own order (D1).
2. T-1..T-4, T-6 (client side) after A-arch-1, since the counters hook into the client lifecycle.
3. T-5 collector (independent; can run in parallel with step 1).
4. P1 smoke matrix after step 1, P2/P3/P5 docs, P4 last.

## Needs-human (excluded from the autonomous run)

| # | Item | Recommendation |
|---|---|---|
| N1 | T-5 deployment: Cloudflare account, domain, `wrangler deploy`, setting `MIMOUSE_TELEMETRY_URL` as a repo secret | Deploy after v1 code lands; until then release builds have no endpoint and `telemetry on` explains why |
| N2 | P1 on real hardware for desktops that can't run headless here (KDE/sway/Hyprland on a real GPU, macOS) | Smoke headless here; record real-hardware runs as they happen |
| N3 | P4 go/no-go: merge `product-review` into `main`, push `v1.0.0` | Merge after the implementation review and smoke are green |
| N4 | X11 capture backend (X11 can only receive today) | Not in v1; README says receive-only |

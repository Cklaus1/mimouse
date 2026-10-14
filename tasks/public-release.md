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
- Reliability: capture and emulation backend in use and whether it was configured, auto-selected or a fallback; panics (message, file:line, backend); count of reconnects, connection closes by missed pings, watchdog key releases, capture/emulation disable events, approval waits (duration bucket), clipboard backend failures.
- Usage (per day): switches of control, number of configured peers, clipboard transfers (count, size bucket), file transfers (count, size bucket), names of CLI commands used (no arguments), hotkey vs edge vs `cli focus` switches.

### Behaviour
- **T-1 Client.** Counters in a `telemetry` module, persisted to `<cache dir>/mimouse/telemetry.json` at most once a minute and on shutdown. One upload per day (and at start if a crash record is waiting): HTTPS POST of one JSON batch, timeout 10 s, retry next day on failure, queue capped at 7 days. Reuse `tokio-rustls`/`rustls`; a root-certificate crate is the only new dependency (license check against `deny.toml`). Upload runs in its own task and is dropped on shutdown.
- **T-2 Crash capture.** Release builds use `panic = "abort"`, so a panic hook writes a crash record to the cache dir before abort; the next start uploads it if telemetry is on, otherwise `cli telemetry show` and `doctor` mention it.
- **T-3 Endpoint.** The release workflow bakes the endpoint URL in at build time (`MIMOUSE_TELEMETRY_URL`); `telemetry_endpoint` in `config.toml` overrides it. Without either, telemetry can't be turned on and the CLI says why. Dev/CI builds have no endpoint.
- **T-4 CLI.** `mimouse cli telemetry on|off|status|show [--json]`; `on` prints what is collected and the endpoint. Changes are audited (`audit.jsonl`) and saved to `config.toml`.
- **T-5 Collector.** `telemetry-collector/`: Cloudflare Worker + D1. Validates the schema version and field set, rejects unknown fields, never stores the client IP or request headers, keeps rows 90 days, rate-limits per install id. Includes the D1 schema, a local `wrangler dev` test and a README with deploy steps. **needs-human:** Cloudflare account, domain and deployment.
- **T-6 Docs.** README "Telemetry" section (opt-in, full field list, how to turn off, retention); AGENTS invariant: new fields only by updating the schema version, README list and the no-PII test.

### Acceptance
- Default config: no network connection other than peers (`ss`/`strace` during smoke, or a test with a failing endpoint that must not be contacted).
- A test builds a payload from a config and runtime state full of host names, IPs, fingerprints and paths and asserts none of them appear in the JSON.
- `cli telemetry show` output equals the bytes the next upload sends.
- Panic in a test binary → crash record on disk → uploaded on next start against a local collector (`wrangler dev` or a test HTTPS server).
- Collector rejects unknown fields and does not store the source IP.

## P: Public support

- **P1 Supported-desktop smoke.** Record a two-machine (or loopback) smoke for each D5 desktop: GNOME Wayland (gpumaster, done), Windows 11 (megapc, done), KDE Plasma Wayland (`kwin_wayland --virtual`), sway/Hyprland (`WLR_BACKENDS=headless`), X11 (Xvfb + a window manager). Each: capture start, crossing both ways, held-key release on disconnect, clipboard text. Results go into the README platform table with the date and version tested. Desktops that can't be run headless here: **needs-human** test on real hardware.
- **P2 README for strangers.** Install per platform (`.deb`, `.rpm`, Windows zip + SmartScreen note, from source), firewall per OS, first pairing in five steps, troubleshooting via `doctor`, supported platforms table (D5), Lan Mouse compatibility statement (D2), telemetry (T-6).
- **P3 Issue intake.** `.github/ISSUE_TEMPLATE/` bug form asking for `mimouse --version`, OS/desktop, `mimouse cli doctor --json` and the log; feature form; `SECURITY.md` with a private reporting channel (GitHub private advisories); `CONTRIBUTING.md` with the gate commands from AGENTS.md.
- **P4 Versioning and release.** Semantic versioning starting at 1.0.0; `CHANGELOG.md` from this branch's history; merge `product-review` into `main`; tag; the existing release workflow publishes `.deb`, `.rpm` and the Windows zip. Publishing the tag: **needs-human** (go/no-go).
- **P5 Lan Mouse wording (D2).** README and `--help` say "originally forked from Lan Mouse" (GPL attribution stays); remove claims about tracking upstream.

## Order

1. `tasks/arch-review.md` in its own order (D1).
2. T-1..T-4, T-6 (client side) after A-arch-1, since the counters hook into the client lifecycle.
3. T-5 collector (independent; can run in parallel with step 1).
4. P1 smoke matrix after step 1, P2/P3/P5 docs, P4 last.

# Opportunities

- agent-control.md A5 assumed GNOME cannot switch capture programmatically; P1's `cli focus` barrier push works on GNOME 50 (verified 2026-10-12). Reconcile A5 with it. (P2)
- Clipboard (and therefore P3 files) is not implemented on macOS. (P2)
- Pairing between gpumaster and megapc works (verified 2026-10-12). Pairing by host name stores the Tailscale addresses MagicDNS returns, even on the same LAN; pair by LAN address (`mimouse cli pair 192.168.50.144 ...`) or `set-ips` to keep traffic off the tailnet. (P3)
- GNOME 50: removing the last client and adding one (e.g. re-pairing) recreates the capture session, so the approval dialog appears again. Keeping the session alive with no barriers would avoid it. (P3)
- AGENTS.md `## Commands`: the Windows clippy line lacks `--all-targets` and the macOS cross-clippy line (`/tmp/fakecc` shims) is only in tasks/build-loop.md. (P3, from D2)
- `AsyncFrontendListener::send` no longer awaits anything (I1); make it sync and drop `async` from `handle_frontend_pending` (`src/service.rs:555`). (P3, from I1)

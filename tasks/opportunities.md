# Opportunities

- agent-control.md A5 assumed GNOME cannot switch capture programmatically; P1's `cli focus` barrier push works on GNOME 50 (verified 2026-10-12). Reconcile A5 with it. (P2)
- Clipboard (and therefore P3 files) is not implemented on macOS. (P2)
- Pairing between gpumaster and megapc works (verified 2026-10-12). Pairing by host name stores the Tailscale addresses MagicDNS returns, even on the same LAN; pair by LAN address (`mimouse cli pair 192.168.50.144 ...`) or `set-ips` to keep traffic off the tailnet. (P3)
- GNOME 50: removing the last client and adding one (e.g. re-pairing) recreates the capture session, so the approval dialog appears again. Keeping the session alive with no barriers would avoid it. (P3)
- AGENTS.md `## Commands`: the Windows clippy line lacks `--all-targets` and the macOS cross-clippy line (`/tmp/fakecc` shims) is only in tasks/build-loop.md. (P3, from D2)
- `AsyncFrontendListener::send` no longer awaits anything (I1); make it sync and drop `async` from `handle_frontend_pending` (`src/service.rs:555`). (P3, from I1)
- layer-shell `update_windows` (`input-capture/src/layer_shell.rs:587`): `active_windows.clear()` on an output change leaves `focused` on a dropped window while the grab may be held; no End is emitted. `grab` (`:464`) still unwraps `focused`. (P3, from K2)
- `cli watch` prints a clipboard clear as `clipboard sent to X: 0 bytes ()` (`src/cli.rs:465-470`); print `cleared`. (P3, from L1)
- GNOME: mutter's clipboard manager restores the previous owner's content after any clear, so a local clear on GNOME rarely sticks; peers follow what the local clipboard really holds. Older MiMouse GNOME peers that receive a clear restore and resend their old content. (P3, from L1)

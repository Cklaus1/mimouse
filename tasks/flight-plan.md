# Flight plan: agent control P1 (2026-10-18)

Default: **Fastest at 90 %**: four parallel worker tracks (emulation, desktop capture, physical-input detection, MCP) while the main agent builds trust/grants/sessions/transport/CLI; one integration pass; smoke on gpumaster + megapc; one review loop.

⚠ production-level: this spec gives a remote peer and local agents control of keyboard, pointer, screen and clipboard (security/auth surface). Recommended: **Most reliable**: add a second review pass dedicated to the trust path (grant checks, revocation, audit redaction) and a longer two-machine soak before merging to `main`.

| Phase | Items | Proof |
|---|---|---|
| Contract | C1 wire types, chord parser, IPC requests, desktop API | committed b76bb6a, 7 proto tests |
| Parallel build | W1 emulation (A6/A7/V2), W2 desktop (V1/K2/K3), W3 preemption (A2), W4 MCP (A9); main: grants, sessions, transport, requester, CLI, A8 | each track's gates green |
| Integration | K1 Hello, observe, downscale, preemption wiring | service-level tests on Dummy backends |
| Smoke | self on gpumaster, gpumaster -> megapc, MCP flow, clipboard/pairing unchanged | build-loop.md SMOKE_SCENARIO |
| Close | README, AGENTS.md, CHANGELOG, review loop | reviewer clean |

Not in this run: K4 ask/kill switch, K5 time-boxed grants, K6 handoff, K7 idle inhibit, K8 telemetry, P2.

Hardware limits: macOS paths compile-checked only (no Mac). Linux preemption is a heuristic (idle time), masked during continuous injection.

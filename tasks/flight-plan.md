# Flight plan: public release v1 (2026-10-14)

Mission: close every arch-review item, add opt-in telemetry and the public-support work, and leave `product-review` ready for a human v1.0.0 go/no-go.

Engineering decisions (Step 2):
- A2 pinning -> per-client `fingerprint` + trust-on-first-use, not `authorized_fingerprints` (keeps Lan Mouse 0.11 / one-way targets working).
- A3/A-arch-5/T7/C3/B7 -> one snapshot merge (base, disk, memory), built once (G1).
- A6 -> explicit `evict(raddr)` + idle timeout + caps in vendored webrtc-util, with B1 per-handshake tasks (Q1: patch vendor).
- A7 -> drop + rate-limited warn on SendInput 0, never an error (no retry exists before B16).
- A8 ships with B16; B16 retries transient errors only, never while a dialog is pending.
- A10 + B10 -> B10 is false; EnterOnly ids leave the hull only when a Default id shares the edge.
- A11 -> drop marked secrets (Q2); clear only when the owner reports no formats.
- B8 -> ids stable within a run (Q3); ⚠ close call: persisted ids rejected as config churn.
- B13 -> one portal session: disable / SetPointerBarriers / enable; recreate only on Disabled.
- B14 -> Begin failure rolls back ACTIVE_CLIENT; key-ups/End never dropped.
- B18 -> new evdev-keyed repeat planner inside A-arch-4, used by Windows and macOS.
- B20 -> bounded roundtrip, creation error, B16 retries (no xkbcommon).
- C3 -> atomic write keeps symlink target and mode.
- T-1 -> hand-written HTTP/1.1 POST over tokio-rustls, `rustls-native-certs` (only new dep), ring RNG for the id.
- T-2 -> crash message only for `&'static str` payloads; paths trimmed to `src/`.
- T-1a (new) -> cargo-deny v2 config + CI job.

Needs-human (excluded): N1 telemetry deployment, N2 real-hardware desktops, N3 merge to `main` + `v1.0.0` tag, N4 X11 capture. Pruned: deployment and tag only; code for all four paths is built.

Step 1 revisions: A2, A3, A6, A7, A8, A10, A11, B1, B3(+B3b), B4, B5, B6, B7, B8, B9, B10 (false), B11 (P2 -> P1), B13, B14, B16, B18, B20, B21, B22, B24, C3, C9, A-arch-2, A-arch-3, T2, T7, T8; public-release T-1..T-5, P1, P2, P4, P5, identity/usage fields.

Critical path: G1 (config snapshot merge; round-trip table gate) and N3 (trust module; loopback DTLS/TLS policy matrix gate). Either BLOCKED halts the run.

Shape: 35 nodes, 5 waves, longest chain 11 (N1 -> … -> R1), up to 9 parallel tracks in wave 1. TEST_CMD `cargo test --workspace --release`, baseline 125 passed / 0 failed + lint warnings 2/4/4. RUN_CMD mimouse.service (gpumaster) + MiMouse task (megapc); smoke = scripted megapc -> gpumaster crossing with log signals. RUN_BUDGET unbounded. Reviewer: `reviewer` agent.

First tasks (wave 1, parallel): N1 (A1 DTLS assert), E1 (A9 decode validation), K1 (A4 pending purge), G1 (A-arch-5 chain), I1 (A5 IPC writer), L1 (A11 secrets), X0 (deny), X2 (collector), D2 (issue intake).

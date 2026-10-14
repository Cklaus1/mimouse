# Todo: public release v1 (derived from commit history; see tasks/build-loop.md)

Wave 1
- [x] N1 A1
- [x] N2 A6, B1
- [x] E1 A9
- [x] E2 A7, A8, B16
- [x] K1 A4
- [x] K2 B11
- [x] K3 A10 (B10)
- [x] G1 A-arch-5, A3, T7, C3, B7
- [x] I1 A5, B3b
- [x] I2 B9, C11
- [x] L1 A11
- [x] X0 T-1a
- [x] X2 T-5
- [x] D2 P3

Wave 2
- [x] N3 A-arch-2, A2, B4
- [x] N5 C4, C10
- [x] E3 A-arch-4, B15, B17, B18, B19, B20
- [x] E4 C13, C14
- [x] E5 T6
- [x] K4 A-arch-3, B12, B13, B14
- [x] K5 C1, C2, C12
- [x] L2 B21, B22, B23, B24
- [x] L3 C6, C7, C8
- [x] G2 C5, C9

Wave 3
- [x] N4 B2, B3, B5
- [x] S1 A-arch-1, B6, B8

Wave 4
- [x] T5 T5
- [x] T3 T3
- [x] X1 T-1, T-2, T-3, T-4
- [x] T2 T2

Wave 5
- [x] P1 smoke matrix
- [x] D1 T4, T-6, P5
- [x] D4 T1, T8
- [x] D3 P2
- [x] R1 P4

Wave 5 smoke (2026-10-14 20:04, 1.0.0 at 63b84ca + README CHANGELOG link): linux aa221f07, win d4271845 deployed; megapc -> gpumaster `entering client 0`, gpumaster `entered at 1,337 (left edge)`, Ctrl double-tap `releasing capture`, `cli list` exit 0 + active on both. megapc clock is 8.6 s behind gpumaster.

Stop condition
- [x] Implementation review + fixes

Review (2026-10-14): 7 reviewers, 17 findings, fixed as 14 nodes R-1..R-14 (R-1 docs, R-2 telemetry off purge, R-3/R-4/R-5 clipboard portal + B21, R-6..R-9 config/IPC, R-10 Windows end session, R-11/R-12 wlroots, R-13 the four doc findings, R-14 clipboard TOFU after ACK). Gates green at 02c8043 (220 + 43 + 23 + 3 tests, clippy -D warnings on 3 targets, deny). Smoke at 4eb23c1: crossing megapc -> gpumaster and back, B21 6/6, R-10 three messages.
- [x] Final smoke, end-of-run report

Final (2026-10-14 20:47-20:49): the merged tree gated at 02c8043 is the deployed 1.0.0 (linux da53b227, win 75eff093). Only tasks/AGENTS/CHANGELOG changed after it. The crossing smoke and R-10 smoke passed, and `git status --ignored` lists only known build dirs. All v1/* branches are merged, and the 15 track worktrees are removed. Open items: needs-human N1-N5 (tasks/public-release.md).

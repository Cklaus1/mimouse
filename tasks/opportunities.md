# Opportunities

- agent-control.md A5 assumed GNOME cannot switch capture programmatically; product-review P1 adds `cli focus` with a barrier push. Reconcile A5 with P1 once P1 lands. (P2)
- Clipboard (and therefore P3 files) is not implemented on macOS. (P2)
- P1 GNOME barrier push (`cli focus` while local on gpumaster) is unverified: capture was waiting for the GNOME approval dialog during the smoke. Approve it, run `mimouse cli focus 0`, and record in the README whether mutter activates the barrier for emulated motion. (P1)
- Pairing (S1) was smoked on gpumaster loopback with temporary configs, not between gpumaster and megapc. (P2)

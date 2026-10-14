# Contributing to MiMouse

MiMouse is a CLI-only software KVM, originally forked from Lan Mouse (hard fork: no upstream merges). Read `AGENTS.md` for the code layout and invariants before changing code.

## Gates

Every change must pass these (the same commands as `AGENTS.md` and CI); warnings are errors:

```sh
cargo fmt && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo clippy --release --target x86_64-pc-windows-gnu --no-default-features -- -D warnings
cargo deny check licenses bans sources   # license allow-list in deny.toml; a new license needs a GPL-3.0 compatibility check
cd telemetry-collector && npm ci && npm test   # collector tests
```

Cross-target clippy (Windows and macOS code must compile even when you only touch Linux):

```sh
cargo fmt --check
cargo test --workspace --release
cargo clippy --workspace --all-targets --all-features --release -- -D warnings
cargo clippy --release --target x86_64-pc-windows-gnu --all-targets --no-default-features -- -D warnings
CC_aarch64_apple_darwin=/tmp/fakecc AR_aarch64_apple_darwin=/tmp/fakear cargo clippy --target aarch64-apple-darwin --all-targets --no-default-features -- -D warnings
```

The macOS line only type-checks; `/tmp/fakecc` and `/tmp/fakear` are stub scripts standing in for the Apple C toolchain.

Run locally with debug logging:

```sh
MIMOUSE_LOG_LEVEL=debug cargo run
```

## Tests

- Behaviour changes and bug fixes start with a regression test that fails before the fix and passes after it.
- Failure-path tests check that the caller recovers on its next call.
- Keep platform logic in pure, platform-independent functions where possible (as with the `doctor` firewall parsers and `macos_logic.rs`) and test those instead of the OS glue.

## Commits and pull requests

- One logical change per commit, with a short imperative subject.
- Keep the diff confined to the change; no TODOs, dead code or unrelated reformatting.
- Do not commit secrets, keys or certificates.
- Report security issues privately (see `SECURITY.md`), not in a pull request.

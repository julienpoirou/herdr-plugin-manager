# Contributing

Use a focused branch and a pull request. Keep changes scoped and explain any
security or compatibility tradeoff.

Before submitting, run:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked && sh tests/e2e.sh
```

The E2E command needs `herdr` on `PATH` on Linux/macOS or WSL; the suite creates
an isolated environment. Never run it against native Windows Herdr. Windows
changes must also pass the Windows CI job and `scripts/build.ps1`.

For security-sensitive changes, include a regression that demonstrates the
observable boundary: no pre-confirmation execution, no terminal control
sequence, safe Git arguments, or preserved Herdr state. Do not commit real
credentials, private repository URLs, or user state.

Changes to untrusted manifest/source parsing should also run the corresponding
fuzz target; see [Fuzzing](../docs/guide.md#fuzzing). Keep generated corpora and
crash artifacts out of commits; add a minimized regression fixture for a fixed
consumer-visible bug.

By contributing, you agree that your changes are licensed under the MIT
license in `LICENSE`.

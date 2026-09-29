# BUG19 local verification

Fresh clone of `https://github.com/manvendersingh21/HIVE.git`; branch
`fix/workspace-gc-disk-placement` created from `origin/main` at `90577fd`.
Verification ran on mac-air using the installed Rust 1.98.1 / Cargo 1.98.1
and Python 3.9.6. No shared toolchain settings or existing services were changed.

All Rust builds/tests used:

```sh
export RUSTFLAGS="-D warnings"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$PWD/target"
```

Commands ran from `hive/` unless otherwise noted. `RUSTFLAGS="-D warnings"`
was also set for the verification scripts.

| Actual command | Result |
| --- | --- |
| `python3 --version` | PASS: Python 3.9.6 |
| `cargo build --workspace --locked` | PASS |
| `cargo test --workspace --locked --no-run` | PASS |
| `cargo test --workspace --locked` | PASS: 671 passed, 0 failed, 19 existing ignored tests/doc-tests |
| `python3 scripts/check-nvidia.py` | PASS: CLI, memory, authenticated web chat; 14 mock requests checked |
| `python3 scripts/check-chat-history.py` | PASS: saved chat integration |
| `python3 scripts/changelog.py check` | PASS: 14 fragments validated |
| `git diff --check` | PASS |
| `(cd hive-core/src/delegation/runner && python3 -m unittest test_runner test_adapters)` | PASS: 85 tests |

The four new Rust integration tests cover the grace-period boundary and all
three terminal states, live/paused/waiting states, overlap protection, cache-only
deletion under a temporary HOME, outside paths and symlinks, durable coordinator
metadata, live runner journal and pending follow-up protection, failed runner
inbox entries, and low/boundary/healthy disk placement (including command argv).
No existing test expectations were changed.

Attempt history: the initial no-run and full Rust test commands passed. The
first two smoke-script attempts failed because they preceded `cargo build`
and the CLI/web binaries did not yet exist. Both passed after the build.
Subsequent code review added command-argv detection and failed-inbox handling;
the build, no-run, full tests, both smoke scripts, changelog check, and diff check
were then rerun successfully against the final code. Runner contract tests passed.

The frontend was not touched, so the conditional npm/TypeScript/Playwright suite
was not run. Existing installed toolchains were used rather than changing global
Rust or Node installations. Build output is removed after verification.

# Agent Instructions for HIVE

Guidelines for automated coding agents working in this repository.

## Commands

```sh
# Build and test
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked

# Verification scripts
python3 scripts/check-nvidia.py
python3 scripts/check-chat-history.py
python3 scripts/changelog.py check
git diff --check

# Runner contract tests
(cd hive-core/src/delegation/runner && python3 -m unittest test_runner test_adapters)
```

## Changelog Workflow

- **Do not edit `CHANGELOG.md` directly.** Modifying `CHANGELOG.md` directly creates merge conflicts when multiple agents or contributors work concurrently.
- Instead, record notable changes by creating a fragment file in `changelog.d/<name>.md` (for example, `changelog.d/fix-login-path.md`).
- Use valid Keep a Changelog section headings:
  - `### Added`
  - `### Changed`
  - `### Deprecated`
  - `### Removed`
  - `### Fixed`
  - `### Security`
- Follow headings with concise bullet points describing the changes.
- Ensure the fragment is non-empty and contains at least one valid section heading.
- Verify fragments with `python3 scripts/changelog.py check`.

## Development Hygiene

- Never commit credentials, real hostnames, SSH account names, or literal personal home paths.
- Keep dev and test profiles warning-free (`RUSTFLAGS="-D warnings"`).
- Set `CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target"`.
- Use git worktrees for isolated work and clean up worktrees after pushing branches.

# Claude Guidelines for HIVE

Guidelines for Claude agents working in the HIVE repository.

## Commands

```sh
# Workspace build and test
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

- **Do not edit `CHANGELOG.md` directly.** Concurrent edits to `CHANGELOG.md` cause merge conflicts across branches and agent sessions.
- Always record user-visible changes by creating a fragment file in `changelog.d/<name>.md`.
- Allowed section headings:
  - `### Added`
  - `### Changed`
  - `### Deprecated`
  - `### Removed`
  - `### Fixed`
  - `### Security`
- Follow headings with concise bullet points describing the changes.
- Ensure the fragment is non-empty and contains at least one valid section heading.
- Run `python3 scripts/changelog.py check` to validate your fragment.

## Development Rules

- Keep dev and test profiles warning-free (`RUSTFLAGS="-D warnings"`).
- Set `CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target"`.
- Never commit credentials, private transcripts, real hostnames, SSH accounts, or personal home paths.
- Use git worktrees for isolated tasks and clean up the worktree after push.

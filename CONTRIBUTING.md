# Contributing to HIVE

HIVE owns agent hosting and orchestration. Protocol semantics, wire schemas,
conformance fixtures, and the `hacp` library belong in the separate
[HACP repository](https://github.com/manvendersingh21/hcap).

## Development checks

Use stable Rust/Cargo and Python 3. Default tests require no agent account or
production service. From the checkout root:

```sh
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked
git diff --check
```

Both dev and test profiles should be warning-free. CI runs these checks on Linux
and macOS. Rebuild the executable before any live binary check: `cargo test`
alone does not rebuild `target/debug/hive` or `hive-web`.

## Dependency workflow

HACP is pinned by full Git revision in the root `Cargo.toml`; `Cargo.lock` records
the resolution. Do not vendor a new `hacp/` directory or commit a dependency on
your home folder. Change protocol behavior in its repository first, run its
conformance and packaging checks, publish the commit, then update HIVE's revision
and lockfile and run the HIVE tests. See [the integration guide](docs/HACP-HIVE.md)
for temporary local development overrides.

## Live and destructive tests

Live tests are explicit opt-ins, not part of the default suite. They may consume
model credits, create remote files and tmux sessions, or interrupt their own
test processes. Use only authorized disposable workspaces and known SSH aliases.
Read a test before enabling it; never run all ignored tests blindly.

Do not install software, change SSH trust, clean up unrelated tmux sessions, or
alter production data merely to make a diagnostic pass. Preserve paused tasks
for inspection. Capture the exact commands and evidence, distinguish seeded
faults from naturally occurring failures, and report skipped tests honestly.

## Pull requests

- Explain the problem, implementation, limitations, and exact validation run.
- Add regression tests for behavior changes and update the relevant public guide.
- Keep PRs focused and preserve unrelated worktree changes.
- Never commit credentials, private transcripts, or literal personal home paths.
  Use SSH aliases and neutral example identities, not private fleet details.
- Keep vendor/model identities in private runtime metadata, not HACP envelopes
  or counterpart briefs and contracts.
- Do not claim a roadmap item is complete based only on code compilation.

Open an issue to discuss substantial changes before implementation. Contributions
are under [Apache-2.0](LICENSE). No particular AI tool, account, session URL, or
invented co-author attribution is required to contribute.

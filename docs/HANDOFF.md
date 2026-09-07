# Maintainer handoff

Start with the [README](../README.md), [contributor guide](../CONTRIBUTING.md),
and [documentation index](README.md). These are the current instructions for
public contributors; no maintainer-specific machine, private planning file,
provider account, or particular AI coding tool is required.

HACP has moved to [its own repository](https://github.com/manvendersingh21/hcap).
Do not recreate a protocol crate inside HIVE. Follow the
[dependency workflow](HACP-HIVE.md) when changing protocol behavior.

Before declaring a code change verified:

```sh
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked
git diff --check
```

Preserve unrelated changes and suspended tasks. Never commit credentials or
personal home paths. Use exact test evidence, rebuild before live checks, and
obtain authorization before modifying production state or running paid agents.
The watchdog is not a sandbox; read [SECURITY.md](../SECURITY.md).

The [Release 1 audit](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07)
records distributed acceptance. [Current limitations](../README.md#current-limitations)
describe unfinished product work. Historical plans and findings are evidence,
not an instruction to claim those features are implemented.

The superseded September 5 handoff remains available in
[Git history](https://github.com/manvendersingh21/HIVE/blob/87dd12251a6e2375d48d77f85b64aba58b4c3d19/docs/HANDOFF.md).

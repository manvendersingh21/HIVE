# HACP repository separation — 2026-09-07

## Result

HACP is now maintained in the public
[manvendersingh21/hcap repository](https://github.com/manvendersingh21/hcap).
Its protocol acronym and Rust package remain `HACP` and `hacp` respectively.
Both HACP and HIVE are Apache-2.0 licensed; GitHub recognizes the new repository's
license as `Apache-2.0`.

The local protocol checkout is `~/Documents/hcap`, but no dependency embeds that
path. HIVE resolves the library from GitHub at commit
`697eae62e950e862b64984ef8f0b2ee86f2aeb34`, pinned in its root manifest and lockfile.
Cargo metadata confirms that HACP is external and HIVE has seven workspace
members. No other lockfile dependency changed during separation.

## Source and documentation

- Extracted the former `hacp/` directory using `git subtree split`, preserving
  protocol history from HIVE's pre-extraction commit
  `87dd12251a6e2375d48d77f85b64aba58b4c3d19`.
- Kept the full Apache-2.0 license, specifications, schemas, example, goldens,
  conformance tests, and independent Python peer in the new repository.
- Removed the HIVE in-tree protocol copy and duplicate Python test peer only
  after the standalone repository was committed and pushed. They remain
  recoverable from Git history and the separate checkout.
- Updated both READMEs, contributor and security guides, documentation links,
  and Linux/macOS CI definitions. HIVE's current guides distinguish implemented
  bilateral hosting from unfinished discovery and autonomous fleet scheduling.
- Replaced outdated active handoff instructions with public contributor guidance;
  historical plans and run evidence remain explicitly labeled and accessible.
- Added a safe empty-worker configuration example without changing the
  maintainer's existing runtime configuration.

No wire semantics, runtime scheduling behavior, or model adapters changed in
this extraction. The Cargo package remains 1.1.0 with a separate HACP/2.0 draft
namespace. No crates.io publication or release tag is implied.

## Verification

```text
Standalone HACP checkout:
  cargo build --locked --offline                 -> passed, no warnings
  cargo test --locked --offline --no-run         -> passed, no warnings
  cargo test --locked --offline                  -> 146 passed, 0 failed, 0 ignored
  cargo run --locked --offline --example bilateral
    -> settled; content, size, digest verified; session closed
  cargo doc --locked --offline --no-deps         -> passed, no warnings
  cargo package --locked --offline              -> clean isolated package build passed

HIVE, with no in-tree protocol crate:
  cargo build --workspace --locked              -> passed, no warnings
  cargo test --workspace --locked --no-run       -> passed, no warnings
  cargo test --workspace --locked --quiet       -> 420 passed, 0 failed, 17 ignored
  cargo metadata --locked --offline --filter-platform aarch64-apple-darwin
    -> HACP is a Git dependency at the pinned revision, not a workspace member
```

The two default suites account for the previous 566 passing tests: 420 HIVE
tests plus 146 HACP tests. The 17 ignored live/helper tests remain explicit
opt-ins; they were not rerun or represented as passing during this packaging
and documentation change.

[HACP's first GitHub Actions run](https://github.com/manvendersingh21/hcap/actions/runs/34164334826)
completed successfully. Runtime live-agent evidence remains the dated
[Release 1 audit](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07), not a new
model-provider run performed for this extraction.

Production services, databases, SSH trust, running/paused agent sessions, and
installed software were not changed.

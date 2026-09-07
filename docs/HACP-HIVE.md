# HACP integration

[HACP](https://github.com/manvendersingh21/hcap) is a standalone protocol and
Rust library. HIVE is a consuming runtime. The protocol has no dependency on
HIVE, an agent vendor, CLI, database, async runtime, or network client.

## Ownership

| HACP repository | HIVE repository |
|---|---|
| Wire types, canonicalization, schemas | Agent CLI adapters and model arguments |
| Bilateral sessions and contract state | Local/SSH hosting and tmux supervision |
| Grants, permits, escalation objects | Durable journal, delivery, inspection, recovery |
| Artifact/evidence/verification objects | Artifact transfer and actual acceptance execution |
| Protocol conformance and independent Python peer | Runtime regressions and live-device acceptance |

Protocol validation does not execute a command, authenticate a peer, or prove a
task is correct. HIVE owns those integration responsibilities. In particular,
deserializing a wire object is not a substitute for validation and authorization.

## Dependency and versions

The root `Cargo.toml` declares `hacp` with an HTTPS Git URL and a full commit
revision. `hive-core` and `hive-adapter` consume that workspace dependency. The
lockfile records the exact source. There is no HACP source directory or submodule
in HIVE, and a contributor does not need a local protocol checkout to build it.

The package remains version **1.1.0**. Root modules implement frozen HACP/1.1;
`hacp::v2` implements the separate HACP/2.0 draft. Cargo package versions do not
identify the wire version, and these two wire protocols are not compatible.

- `hive-core/src/collab` and `hive-adapter` use the legacy 1.1 APIs.
- `hive-core/src/runtime` and `hive collab` use the v2 bilateral APIs.

This is Git distribution; no crates.io publication is implied.

## Develop a protocol change

Clone the protocol separately:

```sh
git clone https://github.com/manvendersingh21/hcap.git
cd hcap
cargo test --locked
cargo run --locked --example bilateral
cargo package --locked
```

For a temporary HIVE integration check against unpublished edits, add a local
override to HIVE's root manifest:

```toml
[patch."https://github.com/manvendersingh21/hcap.git"]
hacp = { path = "../hcap" }
```

Use the path to your own checkout. This is a developer-only override; do not
commit it. Run Cargo without `--locked` once to resolve the patch, then test.
Before submitting, remove the override, publish the tested protocol commit,
update HIVE's `rev`, and run `cargo build --workspace` to resolve the new source
and update the lockfile.
Review that diff and rerun the locked HIVE build and tests.

## Tests after separation

`cargo test --workspace --locked` in HIVE tests HIVE's integration against the
pinned library; it does not execute a dependency's own unit tests. Run those
in the HACP repository. The compatibility wrapper for the independent peer is:

```sh
HACP_CHECKOUT=../hcap interop/run-interop.sh
```

That command tests the supplied checkout, not necessarily HIVE's pinned commit;
check its revision when using it as release evidence. Historical combined totals
must not be compared with HIVE-only totals as though tests were deleted.

## Protocol and runtime references

- [HACP/1.1 specification](https://github.com/manvendersingh21/hcap/blob/main/spec/HACP.md)
- [HACP/2.0 draft](https://github.com/manvendersingh21/hcap/blob/main/spec/HACP-2.0-draft.md)
- [Schemas](https://github.com/manvendersingh21/hcap/tree/main/spec/schemas)
- [Independent test peer](https://github.com/manvendersingh21/hcap/blob/main/tests/interop/peer.py)
- [Protocol contributing guide](https://github.com/manvendersingh21/hcap/blob/main/CONTRIBUTING.md)
- [HIVE distributed workflow](DISTRIBUTED-COLLABORATION.md)
- [Release 1 evidence](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07)

The earlier integration ledger is preserved in
[HIVE's pre-extraction history](https://github.com/manvendersingh21/HIVE/blob/87dd12251a6e2375d48d77f85b64aba58b4c3d19/docs/HACP-HIVE.md).

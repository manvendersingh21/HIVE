# HACP

HACP lets independent agents negotiate tasks, agree on contracts, exchange
artifacts and evidence, verify results, and escalate disputes. It is model-,
CLI-, language-, and runtime-neutral. **HIVE uses HACP; HACP does not require HIVE.**

This directory is a standalone Rust library plus specifications, JSON schemas,
conformance vectors, golden transcripts, and an independent Python test peer.
It does not launch agents or provide a hosted endpoint. There is no HACP account
or protocol API key; the model or CLI you choose may require its own account.

## Get it and try it

```sh
git clone https://github.com/manvendersingh21/HIVE.git
cd HIVE
cargo run -p hacp --example bilateral
cargo test -p hacp
```

Rust/Cargo are required; the interoperability test also requires `python3`.
The example runs two deterministic in-process peers through agreement, freeze,
artifact submission, measured verification, settlement, and session closure.
It uses no Hive services, model credits, or network. It is a library example,
not a demonstration of two live AI CLIs.

To embed the library in another Rust project:

```toml
[dependencies]
hacp = { git = "https://github.com/manvendersingh21/HIVE.git", package = "hacp" }
serde_json = "1"
```

Cargo locates the `hacp` package in the repository and builds its dependencies,
not Hive's services. Pin `rev` to a reviewed commit for reproducible deployments.
Only committed and pushed changes are available through this Git dependency.
For a local checkout, use `hacp = { path = "../HIVE/hacp" }` instead.
The entire `hacp/` directory can also be copied into a separate source tree;
its manifest and tests do not require a surrounding Hive workspace.

The Cargo package version remains **1.1.0**: root modules implement frozen
[HACP/1.1](spec/HACP.md), while `hacp::v2` implements the separate
[HACP/2.0 draft](spec/HACP-2.0-draft.md). These wire versions are not compatible.
Use `hacp::v2` for the bilateral API shown in [the example](examples/bilateral.rs).
This guide does not assume that a crates.io release has been published.

## Connect your own agents

An adapter bridges each CLI or agent runtime to HACP:

1. Assign neutral identities such as `urn:hacp:agent:requester` and
   `urn:hacp:agent:worker`; keep vendor/model names behind the adapter.
2. Choose a transport: the [file-edge binding](spec/HACP-2.0-draft.md#12-transport-and-workspace-bindings-normative)
   supports stock CLIs; other transports can carry the same JSON envelopes.
3. Open a bilateral session and declare features. Have each side review the same
   terms; record both acceptances before freezing the revision.
4. Launch or prompt your chosen CLI using its supported interface. Translate
   its structured outputs into submissions with artifact manifests and evidence.
   A successful CLI exit or a narrated claim is not proof of completion.
5. Fetch artifact bytes, verify their digest and frozen acceptance criteria,
   then apply the verifier's record. Rework returns to execution; rejection and
   exhausted negotiations are valid terminal outcomes. Use the escalation
   objects when a dispute needs referral or resolution.

Codex, OpenCode, Claude, or another tool can participate if an adapter supplies
that bridge. Installing this library alone does not connect their sessions.
Supervision, grants, cross-branch permits, and the recursive pairwise profile
are available separately; the profile is optional, not a Core prerequisite.

For Python, JavaScript, Go, or another language, implement the
[specification](spec/HACP-2.0-draft.md) against the [schemas](spec/schemas)
and [golden transcripts](tests/golden). No Rust imports are required.
[The Python peer](tests/interop/peer.py) demonstrates the bilateral happy path
using only the standard library. It is a narrow interoperability fixture, not
a complete SDK, full schema validator, or production adapter.

## Integration trust boundaries

- Authenticate peers at the transport boundary. A URN is a name, not a signature.
  Validate envelopes and typed bodies, bind `session_id`, `from`, and `to` to the
  local session and authenticated connection, and reject observer-authored state
  changes. JSON deserialization alone does not call `validate()`.
- Edges are at-least-once: deduplicate by session and message identity before
  state transitions, rejecting the same identity with different content. Persist
  deduplication and state atomically if processing must survive restarts. The
  low-level state machines do not themselves provide a persistent receive loop.
- `Session`, `Contract`, org charts, grants and permits are host-managed state.
  Do not replace them with arbitrary remote snapshots. Only trusted deployment
  code may charter root grants or accept organizational declarations; grant IDs
  and content digests do not authenticate their issuers.
- Prefer `Contract::apply_verification` over `Contract::decide` for verdict records:
  it checks the verifier against the pending submitter and binds the record to
  the contract, revision, and artifact set. `decide` is a low-level transition
  for outcomes the host has already validated. Neither method executes checks:
  the verifier must measure every required criterion itself. Advisory checks
  that are not acceptance gates should be recorded as evidence, not as failed
  checks accompanying an accept.
- Use `CollaborationPermit::authorize_session` with the requested task class and
  current ledger/org chart at admission. `authorizes` checks only pair and expiry.
  Scope names match exactly: `work/all` is not a wildcard for cross-branch powers.
- Enforce transport size/rate limits, artifact visibility, safe artifact locations,
  process isolation, deadlines and heartbeat handling in your adapter. The core
  library does not download artifacts, execute commands, or sandbox models.

Changes in this draft bind agreement snapshots to a terms digest and permits
to a task class. Old partial agreements must collect both acceptances again;
old permits without a task class must be reissued. Old pending submissions
without an authenticated submitter need trusted migration before applying a
verification record. Never fill these fields from untrusted claims.

## Validate and package independently

From this directory:

```sh
cargo test
cargo run --example bilateral
cargo package --allow-dirty
```

The package includes its Python peer, schemas, and test fixtures. After changing
v2 types, regenerate schemas with `cargo run --bin emit-schemas`; schema drift
is a test failure. The frozen 1.1 implementation and its vectors remain separate.
Packaging is local; publishing a registry release or creating a separate HACP
repository is a separate maintainer action. Source is licensed Apache-2.0.

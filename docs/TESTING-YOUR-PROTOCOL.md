# How to test your own protocol

This is the testing method HACP/2.0 was built under, written down as a reusable
playbook. It assumes what we assumed: you have a normative spec, you are writing a
reference implementation of it, and you want proof — not vibes — that the protocol
works, interoperates, and survives contact with real agents.

The core discipline: **every layer of testing exists to catch a specific class of
lie**, and the layers are ordered from cheapest to most expensive. Run the cheap
ones constantly; run the expensive ones at every phase boundary.

```
L0  canonical vectors        — catch: "two builds disagree about the bytes"
L1  refusal vectors          — catch: "the engine permits what the spec forbids"
L2  schema gate              — catch: "the contract drifted from the model"
L3  golden transcripts       — catch: "the wire changed and nobody noticed"
L4  independent peer         — catch: "the spec can't be implemented from itself"
L5  live heterogeneous agents— catch: "the protocol assumes agents are honest"
```

---

## L0 — Canonical vectors: pin the bytes

If your protocol digests anything, the digest input must be byte-defined. Write the
canonicalization rules once, then pin each rule with a test that fails if the rule
drifts:

- keys sorted byte-wise (not "sorted" — `\u` vs ASCII differs),
- integers only; a float anywhere in a digested object is an error, not a rounding,
- minimal string escapes, timestamps to the second, digests lowercase hex,
- **cross-language vectors**: compute your test digests with a *different* tool
  (`python3 hashlib` pinned ours) so the Rust tests can't quietly agree with a Rust
  bug.

Where to look: `hacp/src/v2/canon.rs` (rules + vectors), spec §5.1.

**Your exit condition:** a second implementation, written by someone who never read
your source, computes the same digest from the spec text alone.

## L1 — Refusal vectors: test what must NOT happen

A protocol engine is defined as much by its refusals as its happy path. For every
state machine, enumerate the illegal transitions and assert each one errors:

- contract negotiation: counter resets consensus, rounds exhausted → `NoAgreement`,
  stale revision refused, withdrawal pre-freeze only;
- authority: a grant exceeding the grantor's delegable scope is invalid **ab initio**
  — at every layer of the chain, not just the root;
- evidence: an `accept` with no artifacts or no passing check is refused *by
  construction* (§9.4);
- escalation: no resolution without referral, no arbiter who is a party, closed is
  closed.

Where to look: `#[cfg(test)]` blocks inside every `hacp/src/v2/*.rs` module — the
vectors live beside the machine they pin.

**Your exit condition:** every `MUST NOT` in the spec has a test whose name says so.

## L2 — The schema gate: one source of truth

Schemas are *derived* from the semantic model, never the reverse:

1. Types carry `JsonSchema`; a single emitter renders them in canonical form
   (`cargo run -p hacp --bin emit-schemas`).
2. The rendered schemas are committed (`hacp/spec/schemas/*.json`).
3. A gate test compares the committed files to the types on every test run and
   fails on drift **in either direction** — including stale files.

An independent implementer builds against the committed schemas; that is their
contract with you.

**Your exit condition:** `cargo test` fails if anyone edits a schema by hand or
changes a type without re-emitting.

## L3 — Golden transcripts: the wire, frozen

Record real message sequences once, commit them, and replay them on every run:

- normalize **volatile fields only** (message ids, timestamps — occurrence-ordinal
  per field, so ordering survives while content doesn't),
- comparison failures pinpoint the exact frame that diverged,
- record with `HACP_RECORD_GOLDEN=1 cargo test -p hacp --test <name>`, replay
  always.

Where to look: `hacp/tests/common/mod.rs` (harness), `hacp/tests/golden/*.jsonl`
(committed transcripts: a full lifecycle and a no-agreement).

**Your exit condition:** any wire-format change — even field order — fails CI with a
frame-level diff.

## L4 — The independent peer: the leak test

This is the single highest-value test we ran. Write a second implementation of the
whole protocol in a different language, under an **information barrier**: it may
read only the spec, the committed schemas, and the goldens — never your source, never
your libraries. Then run a full lifecycle between reference and peer.

What it catches that nothing else catches: every place your spec was
under-specified. Our peer could not recompute a freeze digest from the spec text —
that was a genuine spec defect, found exactly as designed, fixed by pinning the
preimage normatively.

Mechanics: file-based edge (write envelopes to `a-out/`/`b-out/`, watch dirs), both
sides record their view of the exchange, and the test asserts the two views agree
frame-for-frame. See `interop/peer-python/peer.py` and `hacp/tests/v2_interop.rs`
(`interop/run-interop.sh`).

**Your exit condition:** the peer completes a full lifecycle and both transcripts
agree — without the peer's author ever opening your reference source.

## L5 — Live heterogeneous agents: honesty at scale

Scripts pretending to be agents prove nothing about agents. Run the lifecycle with
real CLI agents (we used codex, claude, agy, opencode in cross-lab pairs) where:

- **the agents do the thinking** (author terms, review contracts, execute, verify),
- **the adapter does the protocol** (envelopes, digests, transcripts), and
- **the adapter mechanically re-verifies every claim** — file existence, digests,
  acceptance criteria — before any verdict goes on the wire.

What we caught at this layer (findings 8–10, `docs/findings/adapter-edge.md`): two
different CLIs narrated successful file creation that never happened; every run
exited 0 regardless of outcome; one CLI resolved "current directory" to its own
scratch space. §9.4 (evidence over signals) refused all of it.

Where to look: `interop/live/hacp-live.py`, transcripts under
`interop/live/transcripts/`.

**Your exit condition:** two cross-vendor pairs settle, and every recorded verdict
check survives adapter re-computation.

---

## Standing rules (the ones that keep the tables honest)

1. **A conformance row moves to ✅ only after the proving command ran.** Paste the
   command output under the table (see `docs/HACP-HIVE.md` §6).
2. **Zero warnings, all tests green, at every phase boundary** —
   `cargo build --workspace && cargo test --workspace`.
3. **Frozen baselines never change.** The 1.1 vectors still run, still pass; 2.0
   never edited them.
4. **Spec defects found by tests get fixed in the spec first**, with a changelog
   entry naming the finder. The implementation follows the spec, never the reverse.
5. **Commit the evidence**: goldens, transcripts, run reports. A test that ran once
   and left nothing behind is a rumor.

## Quick reference — run every layer right now

```
# L0–L3 + unit refusal vectors, hermetic:
cargo test -p hacp

# L4 — the leak test (reference ↔ independent peer):
interop/run-interop.sh

# L5 — live cross-lab agents (requires the CLIs installed):
python3 interop/live/hacp-live.py \
  --run-dir /tmp/hacp-live-run \
  --supervisor claude --worker codex \
  --schemas hacp/spec/schemas \
  --transcripts-out interop/live/transcripts

# The full gate:
cargo build --workspace && cargo test --workspace
```

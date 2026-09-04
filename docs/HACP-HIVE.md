# HACP in Hive — the binding, and where it actually stands

The protocol is specified in [`hacp/spec/HACP.md`](../hacp/spec/HACP.md) and implemented
as types in the standalone [`hacp`](../hacp) crate. **Neither names Hive.** That is
deliberate: HACP describes how heterogeneous agents collaborate, and Hive is *a*
reference implementation of it, not the definition of its limits. A second implementation,
in another language, is meant to be possible from the spec and the conformance vectors
alone.

This document is the other half: how Hive binds that protocol to a real transport, a real
filesystem, and real agentic CLIs — and an honest account of how much of it exists.

---

## 1. Status

**Updated 2026-09-04.** Verified rows were checked by running the command shown; nothing
is marked implemented on the strength of a file existing.

> **HACP 1.1 is FROZEN as of 2026-09-04** (Phase S smoke complete — see
> [`findings/adapter-edge.md`](findings/adapter-edge.md)). The 1.1 spec and its 43 conformance
> vectors are retained as the implemented reference; no further 1.1 protocol work is planned.
> All collaboration design now moves to **HACP/2.0** — bilateral contracts, sessions, recursive
> supervision — drafted in [`hacp/spec/HACP-2.0-draft.md`](../hacp/spec/HACP-2.0-draft.md) per
> [`adr/ADR-0001-hacp-core-is-bilateral.md`](adr/ADR-0001-hacp-core-is-bilateral.md).

> This table previously lived in the spec itself and claimed six components as
> "implemented" that had never been written. That is the failure mode this project's
> [`STATUS.md`](STATUS.md) exists to prevent, so the table now lives here, in the
> implementation's own document, where a claim about Hive belongs.

| Spec element | Where it binds | Status |
|:---|:---|:---|
| Envelope, URNs, kind registry, version gate (§3, §5, §6) | `hacp/src/envelope.rs` | ✅ implemented — conformance vectors pass |
| InterfaceContract, validation, canonical digest (§9) | `hacp/src/contract.rs` | ✅ implemented — includes cycle rejection, absent in 1.0 |
| Topology, decomposition, capability manifest (§4, §7, §8) | `hacp/src/topology.rs` | ✅ types + validation implemented |
| Disputes and rulings (§4, §6) | `hacp/src/dispute.rs` | ✅ types implemented |
| Post-freeze evolution, impact notices, rework (§9.2, §11.1) | `hacp/src/evolution.rs` | ✅ types implemented |
| Reports, verdicts, run summary (§10, §11) | `hacp/src/report.rs` | ✅ types implemented |
| Run state machine + limits (§7, §9, §11.1, §12) | `hacp/src/state.rs` | ✅ implemented, transition table asserted |
| Conformance vectors (§15) | `hacp/tests/conformance.rs` | ✅ 43 passing (39 + 4 pinning defects the bindings found) |
| Binding seams (traits) | `hive-core/src/collab/mod.rs` | ✅ declared and frozen |
| Durable bus (§13.1) | `hive-core/src/collab/bus.rs` | ✅ `SqliteBus`, offline-tested |
| Run workspace + file edge (§13.2, §14) | `hive-core/src/collab/workspace.rs` | ✅ `FileRunStore`, offline-tested |
| Verification, acceptance test (§11) | `hive-core/src/collab/verify.rs` | ✅ `HiveVerifier`, checks 1–7, offline-tested |
| Session supervision | `hive-core/src/collab/session.rs` | ✅ `LocalSessionHost`, 13 offline + 3 live tmux tests |
| Formation (§7) | `hive-core/src/collab/formation.rs` | ✅ `LlmFormation`, offline-tested — **N never yet observed to vary on a real goal** |
| Orchestrator FSM | `hive-core/src/collab/orchestrator.rs` | ⬜ not written |
| Adapter (§13.2) | `hive-adapter/` | ✅ built, 37 tests, smoke-tested against a throwaway coordinator — and against a real CLI (codex) in the Phase S smoke, 2026-09-04 |
| HTTP transport (§13.1) | `hive-web/src/collab.rs` | ⬜ not written |
| CLI surface | `hive collab` | ⬜ not written |
| Live two-agent run | — | ⬜ never run (Phase S exercised one real CLI + sink, not two agents; claude leg blocked by session limit) |
| Live N≥3 federated run | — | ⬜ never run |

**What "offline-tested" means here, and does not.** Every ✅ above rests on `cargo test`:
270 tests pass across the workspace, 0 fail, and `cargo build --workspace` is warning-free.
Not one of these components has yet exchanged a message with another, and no frontier-model
CLI has ever been driven through this code. The bindings were written in parallel against
frozen traits, so they compile together — that is evidence about the seams, not about the
system. Every row above stays at this level until a run happens.

Verified by:

```
$ cargo test -p hacp
test result: ok. 39 passed; 0 failed; 0 ignored
```

---

## 6. HACP/2.0 — where the redesign stands

Design lives in [`hacp/spec/HACP-2.0-draft.md`](../hacp/spec/HACP-2.0-draft.md) with its
section-freeze ledger; the boundary decisions are
[`adr/ADR-0001`](adr/ADR-0001-hacp-core-is-bilateral.md). Same honesty rule as §1: a row
moves to ✅ only after the proving command ran.

| 2.0 element | Where | Status |
|:---|:---|:---|
| Canonical form + digests (§5.1) | `hacp/src/v2/canon.rs` | ✅ rule-pinning vectors incl. independently computed digest vectors |
| Envelope + kind registry (§5.2–§5.3) | `hacp/src/v2/envelope.rs` | ✅ incl. no-execute-kind and forward-compat round-trip vectors |
| Agents + capability vocabulary (§3, §6.3) | `hacp/src/v2/agent.rs` | ✅ shape vectors |
| Bilateral sessions, observers (§6) | `hacp/src/v2/session.rs` | ✅ observers-cannot-author, lifecycle-events-only, feature-intersection vectors |
| Contract engine (§7) | `hacp/src/v2/contract.rs` | ✅ full §7.3 machine incl. implicit EXECUTE, NoAgreement terminals, immutable revision digests |
| Artifacts + provenance (§9.1) | `hacp/src/v2/artifact.rs` | ✅ uuid4/digest shapes, ancestry walk refusing cycles, visibility ladder |
| Evidence (§9.2) | `hacp/src/v2/evidence.rs` | ✅ shape vectors |
| Verification, evidence-over-signals (§9.3–§9.4) | `hacp/src/v2/verification.rs` | ✅ accept-without-basis refused by construction; attestation closure |
| Canonical JSON schemas (§14) | `hacp/spec/schemas/*.json` | ✅ 7 schemas behind the drift gate (`cargo run -p hacp --bin emit-schemas`) |
| Golden wire transcripts (§14) | `hacp/tests/golden/*.jsonl` | ✅ lifecycle + no-agreement, replayed on every test run |
| **Independent interop peer** (§14 Independence) | `interop/peer-python/peer.py` | ✅ Python stdlib, spec+schemas only; see below |
| Phase 3 exit test (ADR-0001) | `hacp/tests/v2_interop.rs` | ✅ reference ↔ independent peer, file edge, mutual transcript agreement |
| Delegation/CapabilityGrant objects (§8) | — | ⬜ types only inside Contract today; full grant machinery is Phase 4 |
| Cross-branch permits, escalation engine (§10–§11) | — | ⬜ spec frozen; Phase 4+ |
| HIVE profile module (§13) | — | ⬜ spec frozen; re-enters with the runtime waves |

Verified by (2026-09-04):

```
$ cargo test -p hacp
test result: ok. 47 passed; 0 failed; 0 ignored        (lib: v2 + frozen 1.1 units)
test result: ok. 43 passed; 0 failed; 0 ignored        (frozen 1.1 conformance vectors)
test result: ok. 3 passed; 0 failed; 0 ignored         (v2 lifecycles vs goldens)
test result: ok. 5 passed; 0 failed; 0 ignored         (transcript harness)
test result: ok. 1 passed; 0 failed; 0 ignored         (Phase 3 exit: independent peer)
$ interop/run-interop.sh
test an_independent_peer_interoperates_over_the_file_edge ... ok
```

**What the exit test proved.** Two implementations that share no code — the Rust
reference and a Python peer built from the spec, the committed schemas, and the goldens
— negotiated, froze (both computing the same §7.5 revision digest independently),
executed, submitted an artifact with a manifest, and settled, over the file edge, with
frame-for-frame agreement between their two views of the exchange. The peer's need for a
pinned digest preimage was caught as a spec defect and fixed (spec changelog, §7.5):
that is the leak test working.

### Live heterogeneous runs (2026-09-04)

Not scripts pretending to be agents — real CLIs as the two minds, `interop/live/hacp-live.py`
as their hands. The supervisor CLI authors the contract terms; the worker CLI reviews the
proposal and executes the frozen contract; the supervisor CLI verifies for real; the adapter
mechanically re-verifies every claimed check before any `accept` goes on the wire (§9.4).

| Pair | Result | Frames | Verdict | Corroborated checks |
|:---|:---|:---|:---|:---|
| claude (sup) × codex (wrk) | settled | 9 | accept | 4 |
| agy (sup) × opencode (wrk) | settled | 9 | accept | 3 |

Transcripts + run reports: `interop/live/transcripts/`. Two failures were observed on
other pairings/roles and are recorded as findings 8–10 in
[`findings/adapter-edge.md`](findings/adapter-edge.md) — the headline: two different CLIs
narrated successful file creation that never happened, and the adapter's evidence-over-signals
rule refused both. The protocol's core bet — verification is mechanical, not social — held
against real agents lying (confidently, unintentionally) to it.

---

## 2. What the spec deliberately does not say, and Hive does

| Spec (abstract) | Hive (concrete) |
|:---|:---|
| "a coordinator" and "an arbiter" as separate duties (§2) | one `hive-web` process performs both; the arbiter's reasoning is an `LlmRouter` call |
| "an ingest operation" and "a poll operation" (§13.1) | `POST /api/collab/runs/{run_id}/ingest`, `GET /api/collab/runs/{run_id}/messages?since=&agent=` |
| "a token per run per role, delivered out of band" (§13.3) | environment variable `HIVE_COLLAB_TOKEN`, read by the adapter |
| "safety supervision, pausing rather than killing" (§13.3) | Tier-1 regex on every tailed line plus periodic Tier-2 LLM review; pause is SIGSTOP to the pane's foreground process group |
| "a worker's workspace" (§14) | a git worktree on the worker's own branch |
| "implementation-defined evidence" (§10, `EvidenceRef`) | tmux session names and log paths |

## 3. Which CLI runs which role

The mapping from an agent URN to the tool behind it lives **only** here, in the master's
run record. §3 forbids it appearing on the bus, in a brief, or in a contract — a worker
that learns its peer's vendor will condition on that, and the protocol's claim is that
collaboration happens through interfaces.

Installed and driven non-interactively on this machine:

| Tool | Non-interactive invocation |
|:---|:---|
| `claude` | `claude -p <prompt> --output-format json` |
| `codex` | `codex exec --sandbox workspace-write <prompt>` — read-only sandbox by default; the flag is required for any file-edge work (measured 2026-09-04, Phase S) |
| `agy` | `agy -p <prompt> --output-format json` |

`opencode` is **not installed here**; nothing in the design depends on which tools are
present, which is the point of C1.

## 4. Reuse, not reinvention

The binding leans on machinery Hive already has, and the reasoning behind each is recorded
where it was learned:

- **Capability-based role assignment** — `memory::machines::machines_with_capabilities`
  and the ranking policy in [`PLACEMENT.md`](PLACEMENT.md) §3. §8's admission/assignment
  split maps exactly onto PLACEMENT §4's rule: never substitute silently.
- **Session supervision** — `workers::ssh`'s tail-and-scan loop, and the two bugs
  [`STATUS.md`](STATUS.md) records: a bare session name is not a tmux target-pane, and
  `C-c` is a kill rather than a pause.
- **Prompt shape** — briefs are the planner problem again. PLACEMENT §3 measured a worked
  table at 14/14 against a prose rule at 11/15 on the same local model. Briefs should show
  the `{kind, body}` shape, not describe it.

## 5. Honesty rule

A row in §1 moves to ✅ only after the command that proves it has been run and its output
pasted here. Anything else reproduces the exact defect this document was created to fix.

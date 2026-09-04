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
> supervision — drafted alongside, per ADR-0001.

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

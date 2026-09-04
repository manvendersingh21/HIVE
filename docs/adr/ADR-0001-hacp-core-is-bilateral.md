# ADR-0001: HACP Core is a bilateral protocol; HIVE is a profile, not the protocol

**Status:** Accepted (2026-09-04)
**Supersedes:** the topology assumptions of HACP/1.1 (spec frozen same date — see
[`docs/HACP-HIVE.md`](../HACP-HIVE.md) and
[`docs/findings/adapter-edge.md`](../findings/adapter-edge.md))
**Affects:** `hacp/spec/HACP-2.0-draft.md`, all HACP/2.0 work packages (W2–W7)

## Context

HACP/1.1 modeled a collaboration as one run, one contract, N workers, and a master that was
simultaneously the truth source, the market maker, and the escalation endpoint. Three pressures broke
that model:

1. **The arbiter requirement.** 1.1 made an arbiter mandatory at N≥3. In practice that makes the
   arbiter a single point of both failure and trust, and it conflates *organizational* authority
   with *dispute resolution*.
2. **The star.** A master→N-workers topology is one organizational shape. Protocol semantics
   should not know the org chart.
3. **Interoperability.** 1.1's own charter — a second implementation, in another language, from
   the spec alone — is undermined when the spec's unit of collaboration (the run) is defined by
   one implementation's runtime.

The revised architecture (2026-09-04) resolves these by making the **bilateral contract** the
protocol's only primitive.

## Decision

### 1. Three layers, one boundary each

| Layer | Owns | Does not own |
|:---|:---|:---|
| **HACP Core** | agent identity, bilateral sessions, contract negotiation, artifacts & provenance, evidence, verification, escalation semantics, transport-independent messages | any organizational shape, scheduling, process management |
| **HACP Recursive Pairwise Profile** | a named, versioned profile constraining Core: parent/child delegation with `max_direct_subordinates = 2`, LCA-based escalation routing, sibling preauthorization | anything Core does not already express |
| **HIVE runtime** | spawning processes, worktrees, tmux supervision, the bus, scheduling, adapters' hosting | contract semantics, authority rules |

**Spawning is HIVE. Delegation is HACP.** A runtime may start any process it likes; the moment
authority over a task passes between two agents, it is a HACP delegation and carries the
protocol's obligations (scope, evidence, escalation path).

### 2. Bilateral is the primitive

Every contract has exactly two accountable participants. Multi-agent collaboration is a *graph*
of bilateral contracts, not an N-party object. N-party coordination is an emergent property of
the org layer (HIVE), never of the protocol.

### 3. Seven roles, independently held

Supervisor, Transport/router, Verifier, Arbiter, Agent host, Scheduler, Artifact store. Any
deployment may co-locate them; the protocol must not require it. Notably:

- **Arbiter is optional at every N.** Disputes resolve by escalation semantics
  (§11 of the 2.0 draft), and `NO_AGREEMENT` — the parties record evidence and part — is a
  valid terminal outcome, not a failure of the protocol.
- **Truth is durable protocol objects**, not a master process: frozen contract revisions with
  digests, artifact manifests, verification records.

### 4. Five graphs, kept separate

Organizational (who supervises whom), task (decomposition), contract (who owes what to whom),
communication (sessions/messages), artifact (provenance). Edges in one graph MUST NOT be
inferred from another; each is declared and queryable on its own.

### 5. The invariant

**Authority flows downward; evidence flows upward; collaboration flows sideways when
authorized.** Every normative rule in the 2.0 draft is an instance of this. Delegation obeys
`child_authority ⊆ parent_delegable_authority`; verification consumes the artifacts and evidence
the children produced; cross-branch sessions exist only under an explicit permit.

### 6. Four refinements locked at plan approval

1. **Profile boundary.** Max-two-children, LCA routing, tree balancing, sibling
   preauthorization are normative *only* within the HIVE Recursive Pairwise Profile. Core
   defines supervisory vocabulary (relationship declarations, escalation paths) with no arity
   constraint. Core-conformance ≠ profile-conformance; a deployment declares its profile in
   capability negotiation.
2. **Participants vs observers.** A session has exactly two *participants* — the contract's
   accountable parties, the only endpoints that can author session messages or bind the
   contract. Optional *observers* (supervisors, auditors, a prospective arbiter) may subscribe
   to lifecycle events under an explicit grant; they cannot author, cannot alter state, receive
   lifecycle events rather than necessarily every token, and their presence is recorded.
3. **EXECUTE is a lifecycle state, not a wire message.** Entry is implicit on freeze; the
   wire-visible events are only the transitions requiring mutual knowledge (PROPOSE, COUNTER,
   ACCEPT, FREEZE, SUBMIT, verdicts).
4. **`NO_AGREEMENT` is a valid terminal.** Both for negotiation deadlock and for exhausted
   escalation.

### 7. What survives from 1.1

Lab-neutral URNs and the vendor-opacity rule; kind-registry forward compatibility (unknown
kinds delivered, never rejected); freeze semantics with canonical digests; bounded negotiation;
mechanical verification with adapter-synthesized reports; the file edge as *a* binding, now
first among several. The 1.1 implementation and its 43 conformance vectors remain the frozen
regression baseline.

## Consequences

- `hacp::v2` is a clean namespace beside frozen 1.1 modules; no retrofit, no version blur.
- 1.1 orchestrator/web/CLI work is shelved (already frozen); the HIVE runtime re-enters at
  Wave 5 redesigned around recursive delegation.
- Phase 3's exit test gains an independent-peer requirement: at least one peer importing
  neither HIVE nor the reference `hacp` crate, interoperating from the normative spec + schemas
  alone — the leak test for this boundary.
- Every later phase finishes with five deliverables: normative semantics/state machines, worked
  examples, canonical schemas, golden wire transcripts, conformance vectors.

## Alternatives considered

- **Evolve 1.1 in place (1.2).** Rejected: bilateral sessions and recursive supervision are
  breaking conceptual changes; retrofitting would blur what the 43 vectors pin.
- **Keep the arbiter mandate, add recursion around it.** Rejected: preserves the single point
  of trust and the star's assumption inside every subtree.

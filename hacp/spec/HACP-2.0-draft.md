# HACP/2.0 — Heterogeneous Agent Collaboration Protocol (DRAFT)

**Status:** draft skeleton, 2026-09-04. This document specifies HACP/2.0 alongside the
**frozen** [`HACP.md`](HACP.md) (1.1), which remains the implemented reference with its 43
conformance vectors. Nothing here is implemented yet; sections marked *(normative)* are the
binding intent that `hacp::v2` and every conformance vector will be written against.
Rationale for the boundary reset: [`docs/adr/ADR-0001`](../../docs/adr/ADR-0001-hacp-core-is-bilateral.md).

**The one-sentence protocol:** two agents who do not trust, know, or understand each other's
internals negotiate a bilateral contract, exchange addressable artifacts under that contract,
and verify the result — with authority flowing down, evidence flowing up, and sideways
collaboration only when authorized.

## Section-freeze ledger

Downstream work packages gate on sections of this document. A section is *frozen* when listed
here with a date; later changes require a changelog entry and re-freeze.

| § | Section | Gate for | Status |
|:---|:---|:---|:---|
| ① | 5. Canonical form and envelope | W3 (harness), W6 (independent peer) | frozen 2026-09-04 (skeleton level) |
| ② | 6. Sessions and capability negotiation | W4 | frozen 2026-09-04 (skeleton level) |
| ③ | 7. Contracts | W4 | frozen 2026-09-04 (skeleton level) |
| ④ | 8. Delegation and authority · 9. Artifacts, evidence, verification | W5 | frozen 2026-09-04 (skeleton level) |
| ⑤ | 12. Transport and workspace bindings | W6, runtime waves | frozen 2026-09-04 (skeleton level; Phase S findings merged) |

"Skeleton level" means the semantics, state machines, and object shapes are fixed; worked
examples, canonical JSON schemas, golden transcripts, and conformance vectors land with the
implementing phase (the five-fold definition of done). Each phase may *extend* a frozen section
appendix-style; it may not reopen frozen normative statements.

## 1. Design constraints

Carried from 1.1, unchanged in spirit:

- **C1 — stock CLIs are first-class participants.** No agent can be assumed to speak anything
  but files-and-a-process. The protocol's edge for such agents is the file edge (§12.1).
- **C2 — vendor opacity.** No peer learns which tool backs another role. Carried from 1.1 §3.
- **C3 — at-least-once edges, idempotent consumers.** Duplicates are ordinary traffic.
- **C4 — mechanical verification.** What a machine can check, a machine checks; what it cannot,
  it records as evidence for a human or a designated verifier.

New in 2.0:

- **C5 — bilateral primitive.** Every contract binds exactly two accountable participants.
- **C6 — small vocabulary.** Eleven semantic objects, one kind registry, no special-cased
  choreography. EXECUTE is a state precisely because it needs no message.
- **C7 — independence.** A second implementation, in another language, from this spec plus the
  canonical schemas and golden transcripts alone, is a conformance requirement (§14), not an
  aspiration.

## 2. Layers and profiles *(normative)*

Three layers (ADR-0001 §1):

- **HACP Core** — this document's §3–§12: identity, sessions, contracts, delegation and
  authority, artifacts, evidence, verification, cross-branch authorization, escalation, and the
  message envelope. Core knows no organizational shape.
- **Profiles** — named, versioned constraint sets over Core, declared in capability
  negotiation. The first profile is the **HACP Recursive Pairwise Profile** (§13), which binds
  HIVE. Core-conformance and profile-conformance are separate claims.
- **Runtimes** — implementations that host agents, spawn processes, and carry Core messages
  over real transports (§12). HIVE is one. A runtime may be a peer's entire world, or a peer
  may be a bare process speaking a transport binding directly.

**Spawning is runtime work; delegation is protocol work.** When authority over a task passes
between two agents, a HACP delegation exists regardless of who started which process.

## 3. Agents and identity *(normative)*

An **Agent** is a durable identity with advertised capabilities. Identity is a URN, neutral of
vendor, product, and model (carried from 1.1 §3):

```
urn:hacp:agent:<local-name>
```

- `local-name` is deployment-unique. Nothing in `local-name` may encode the backing tool.
- Agents MAY be long-lived (a service) or per-engagement (spawned per contract); the protocol
  does not distinguish.
- **Capability advertisement** is a set of feature identifiers (§6.3) plus free-form descriptors.
  Advertisement is provenance, not proof (carried from 1.1 §8): it informs matching, never
  admission.

## 4. Semantic object vocabulary *(normative)*

Eleven objects. Each is a durable, addressable protocol object with an identifier and a
declaring section. Runtimes keep them; the truth of a collaboration is this set, not any
process.

| Object | One-line definition | § |
|:---|:---|:---|
| Agent | durable identity + advertised capabilities | 3 |
| Session | the bilateral communication relationship between exactly two participants | 6 |
| Task | a unit of work with an owner and a completion claim | 7.1 |
| Contract | the bilateral agreement binding two parties to a task's interface | 7 |
| Delegation | a contract of kind `delegation`, carrying authority downward | 8 |
| CapabilityGrant | a subset of authority, granted and revocable | 8 |
| Artifact | an addressable work product with digest and provenance | 9 |
| Evidence | an attestation about artifacts or process, produced for verification | 9 |
| Verification | a verdict record over a submission, with reasons | 9 |
| Escalation | a dispute's journey through the supervisory graph | 11 |
| Amendment | a proposed revision to a frozen contract | 7.6 |

## 5. Canonical form and envelope *(normative)* — §①

### 5.1 Canonical form

Every digest in the protocol is taken over the **canonical form** of a JSON value:

- UTF-8, no byte-order mark, no whitespace between tokens.
- Object members sorted by key, byte-wise over the UTF-8 encoding, ascending.
- Numbers MUST be integers in canonical form. Non-integer numbers are forbidden in any object
  that is digested. Quantities that are naturally fractional are carried as strings with an
  explicit unit.
- Strings escape only `"` `\` and control characters below 0x20; controls use `\uXXXX` with
  lowercase hex. Everything else is literal UTF-8.
- Timestamps are RFC 3339, UTC (`Z`), seconds precision, no fractional part:
  `YYYY-MM-DDTHH:MM:SSZ`. Producers emitting finer precision MUST NOT — canonical timestamps
  are exactly this shape.
- Digest: SHA-256 over the canonical UTF-8 encoding, encoded as 64 lowercase hex characters.

### 5.2 Envelope

The wire unit is carried from 1.1 §5, extended with session correlation:

```
protocol      "HACP/2.0"
message_id    unique per message, "m-" + at least 12 hex chars
session_id    the session this message belongs to (§6)
from          agent URN (a participant of that session, or an edge service acting for one)
to            agent URN
kind          registry string (§5.3)
timestamp     canonical timestamp
in_reply_to   optional: message_id being answered
body          kind-specific JSON object
```

- `session_id` is mandatory except for the messages that establish a session (§6.1), where it
  names the *prospective* session.
- Unknown fields in a received envelope MUST be preserved on re-serialization (carried from
  1.1: envelopes are forward-compatible containers).
- `protocol` mismatch with a peer's negotiated version is a session-level error (§6.4), not a
  transport failure.

### 5.3 Kind registry (initial, v2.0)

```
session.open          session.features      session.close
contract.proposed     contract.countered    contract.accepted    contract.frozen
contract.amendment.proposed  contract.amendment.decided
contract.withdrawn    contract.no_agreement
submission.delivered  verification.delivered
escalation.raised     escalation.referred   escalation.resolved  escalation.no_agreement
collaboration.request collaboration.permit
heartbeat             error
```

Deliberately absent: any `execute` kind. EXECUTE is a contract lifecycle state (§7.5); entry is
implicit on freeze. An implementation wanting to *notify* may send an informational kind of its
own — unknown kinds are delivered, never rejected (carried from 1.1 §6) — but no conformance
vector may require one.

## 6. Sessions and capability negotiation *(normative)* — §②

### 6.1 Bilateral by construction

A **Session** has exactly two **participants** — the only endpoints that can author messages
in the session or bind its contract. Sessions are established by `session.open` from one
participant and accepted by the other (acceptance is the first message the second participant
authors).

### 6.2 Observers

Optional **observers** — supervisors, auditors, a prospective arbiter — MAY be granted
subscription to a session's **lifecycle events** (a subset of kinds: frozen, amendment decided,
submitted, verdicts, closed, escalated). Normatively:

- An observer cannot author messages in the session and cannot alter its state; a message from
  a non-participant is a session-level error.
- Observers receive the granted lifecycle events, not necessarily every token.
- The grant is recorded on the session object (who observes what). Presence is visible to both
  participants.
- Observer support is a negotiated capability (§6.3); a participant that does not support
  observers is never required to host them.

### 6.3 Capability negotiation

On `session.open`/accept, participants exchange `session.features` declaring feature
identifiers, minimally: `supervision`, `delegation`, `observer-events`, `artifact-digest`,
`cross-branch`, plus transport-level capabilities per §12. A feature a participant did not
declare is a feature the other participant MUST NOT rely on in that session. Capability
mismatch on a *required* feature prevents contract formation cleanly: the session closes with
`session.close` carrying `reason: capability-mismatch` — an ordinary outcome, not an error.

### 6.4 Session lifecycle

```
OPENING → ACTIVE → CLOSED
              ↘ ABANDONED
```

- `OPENING`: `session.open` sent, not yet accepted. Timeouts expire to `ABANDONED`.
- `ACTIVE`: both participants bound. Messages flow; contracts may form.
- `CLOSED`: terminal by mutual `session.close`. Sessions are never reopened; parties open a
  new session (new `session_id`, fresh negotiation).
- `ABANDONED`: terminal on failure, timeout, or unresponsive peer (three missed heartbeats,
  carried from 1.1 §12). Evidence of the abandonment is retained.

## 7. Contracts *(normative)* — §③

### 7.1 Task and contract

A **Task** names a unit of work, its owner, and its completion claim. A **Contract** binds
exactly two participants to a task's interface: inputs, outputs (artifact specifications),
dependencies, acceptance criteria, budget, and escalation path (§8.3). One task may correspond
to many contracts (decomposition); one contract references exactly one task.

### 7.2 Relationship kinds

A contract's `relationship` is either `collaboration` (peers) or `delegation` (§8). The state
machine is identical; the difference is the authority and escalation semantics attached.

### 7.3 State machine

```
                    counter (bounded)
   ┌─────────┐   ┌──────────────┐
   │PROPOSED │ ⇄ │  COUNTERED   │        pre-freeze negotiation loop,
   └────┬────┘   └──────┬───────┘        bounded by max_rounds (7.4)
        │ accept        │ accept / expire
        ▼               ▼
   ┌──────────────────────┐  withdraw (pre-freeze only)
   │       ACCEPTED       │ ──────────────► WITHDRAWN
   └──────────┬───────────┘
              │ freeze — both digests recorded
              ▼
   ┌──────────────────────┐
   │   FROZEN  (rev 1)    │◄──────────────┐
   └──────────┬───────────┘               │ re-freeze (rev N+1)
              │ implicit entry            │
              ▼                           │
   ┌──────────────────────┐   amend    ┌──┴──────────┐
   │      EXECUTING       │◄───────────│  AMENDING   │
   └──────────┬───────────┘  (7.6)     └─────────────┘
              │ submission.delivered
              ▼
   ┌──────────────────────┐
   │      VERIFYING       │
   └───┬──────┬───────┬───┘
       │      │       │
   accepted  rework  rejected
       ▼      ▼       ▼
   ACCEPTED  (→EXECUTING, rework scope)  REJECTED
```

Pre-freeze exhaustion (rounds or deadline without acceptance) terminates in **`NO_AGREEMENT`** —
a valid terminal, recorded with the full negotiation transcript as evidence.

### 7.4 Bounded negotiation *(carried from 1.1)*

Silence does not consent. Each contract carries `max_rounds` and `max_amendments`; reaching
either bound without agreement is `NO_AGREEMENT`, never an implicit freeze.

### 7.5 Freeze, EXECUTE, SUBMIT

- **FREEZE** makes the contract revision immutable: both participants record the canonical
  digest of revision N; every later reference (submissions, verdicts, amendments) names that
  digest.
- **EXECUTE** is the state between freeze and submission. It is entered implicitly on freeze;
  no wire message is required or defined for entry. It is observable through liveness
  (heartbeats) and terminated by `submission.delivered`.
- **SUBMIT**: the performing participant delivers artifacts (by reference, §9) plus evidence
  and a completion claim against the frozen revision.

### 7.6 Amendments

Post-freeze change is an **Amendment**: a proposed revision N+1 negotiated through the same
bounded loop as 7.3 (states `AMENDING`), then re-frozen with a new digest. References to "the
contract" always name a revision digest; history is never rewritten.

### 7.7 Withdrawal

Either participant may withdraw pre-freeze (`WITHDRAWN` with reason). Post-freeze exit is not
withdrawal; it is failure under the contract — recorded as `REJECTED` by the counterparty, or
by escalation (§11).

## 8. Delegation and authority *(normative)* — §④ (part one)

### 8.1 Delegation

A **Delegation** is a contract with `relationship: delegation`: parent → child, referencing a
task, carrying a CapabilityGrant and the declared escalation path.

### 8.2 CapabilityGrant and monotonic authority

A **CapabilityGrant** names grantor, grantee, an authority scope set, a validity window, and a
`delegable` flag per scope element. **Inviolable rule:**

```
grantee_authority ⊆ grantor_delegable_authority
```

enforced at grant time — a grant exceeding the grantor's delegable authority is invalid ab
initio, and every layer of the chain must satisfy the same test. Revocation closes the grant;
work under a revoked grant is a contract failure, not a crime — it is recorded and escalated.

### 8.3 Escalation path

Every delegation declares its parent chain. The chain is the *organizational* fact; it never
implies communication or artifact edges (ADR-0001 §4).

## 9. Artifacts, evidence, verification *(normative)* — §④ (part two)

### 9.1 Artifacts

An **Artifact** is an addressable object independent of any filesystem:

```
artifact_id   urn:hacp:artifact:<uuid4>
media_type    e.g. text/plain, application/json
digest        SHA-256 of content (§5.1), never "TBD"
size          bytes
producer      agent URN
task_id, contract_id   (revision digest for the latter)
derived_from  [artifact_id] — provenance, independent of the org chart
location      binding-specific reference (path, URL, store key)
visibility    participants | supervisors | session-observers | deployment
```

Artifacts are referenced by id in contracts and submissions, never embedded in envelopes
(carried from 1.1's size ceiling). The **artifact graph** (provenance via `derived_from`) is
queriable without knowing who supervised whom.

### 9.2 Evidence

**Evidence** attests to process or provenance: command transcripts, log excerpts, test output,
signatures. It is produced by the performing side and consumed by verification. Evidence never
*is* truth; it is the input to a verdict.

### 9.3 Verification

A **Verification** is a verdict record: verifier, subject submission, contract revision digest,
verdict (`accepted | rework | rejected`), mechanical checks run, reasons for anything a machine
did not decide alone. Verifiers MAY attest recursively: a supervisor's verification of an
integration may reference children's verification records — attestation composes, authority
does not (ADR-0001 §5).

### 9.4 Evidence over signals *(Phase S finding)*

A peer's process exit code, exit file, or self-report is a **signal**; artifacts and evidence
are the **basis** of verdicts. A verifier MUST NOT accept on signals alone. (Measured: a
sandbox-blocked stock CLI exited 0 having produced nothing.)

## 10. Cross-branch collaboration *(normative)*

Two agents in different subtrees MAY form a `collaboration` contract only under authorization:

1. `collaboration.request` — one agent asks its own supervisor chain; names the prospective
   peer, task, scope, expiry.
2. `collaboration.permit` — issued by the **lowest common supervisor** with authority over both
   branches (LCA discovery walks the declared chains of §8.3), or by preauthorization: a
   standing CapabilityGrant with `cross-branch` scope for a named class of peers/tasks.
3. The resulting session records the permit id; its provenance is part of the run's evidence.

The permit authorizes the *session*, not the outcome: the contract still negotiates, freezes,
and verifies like any other.

## 11. Escalation *(normative)*

```
escalation.raised (same-parent dispute)
        │ mediated by the shared supervisor
        ▼ unresolved
escalation.referred (to the LCA — walks §8.3 chains)
        │ LCA rules structurally (split, reassign, deadline)
        │ or MAY invoke an Arbiter role — optional at every N, never mandatory
        ▼ unresolved
escalation.no_agreement — valid terminal; all evidence retained
```

An **Escalation** object records the journey: parties, subject (contract/task/artifact),
path taken, ruling or its absence. `escalation.resolved` carries the ruling. There is no N at
which an arbiter becomes mandatory (ADR-0001 §6.1); a deployment that wants standing arbiters
declares them in its profile.

## 12. Transport and workspace bindings *(normative for bindings)* — §⑤

Bindings carry Core messages; they never define semantics. Three normative binding rules apply
to all of them, each carrying a Phase S measurement
([`docs/findings/adapter-edge.md`](../../docs/findings/adapter-edge.md)):

1. **Write rights are a launch-contract concern.** A binding that hosts stock CLIs MUST
   establish workspace write semantics at spawn time (e.g. sandbox mode); nothing post-spawn
   can repair a worker that cannot write. (Finding 1: `codex exec` defaults read-only.)
2. **Exit codes are not outcomes.** Process status is liveness, not verdict (§9.4).
   (Finding 2: exit 0 with zero work done.)
3. **Delivery filters by addressee.** A poll/collect operation MUST return only traffic
   addressed to the requesting agent; anything else is cross-role noise and leakage.
   (Finding 4: an unfiltered sink flooded every INBOX with every agent's heartbeats.)

### 12.1 File edge

The default binding for agent hosts of stock CLIs (verified byte-exact by a stock CLI in Phase
S): `BRIEF.md` in, `INBOX/` in, `OUTBOX/*.json` out (`{"kind", "body"[, "in_reply_to"]}`),
`REPORT.json` out. The adapter stamps identity fields; at-least-once with `message_id`
deduplication (verified, including adapter crash-restart idempotency).

### 12.2 HTTP binding

Carried from 1.1 §13.1 as one binding among several: `POST …/ingest` answering
`{status, seq}` (409 duplicate), `GET …/messages?since=&agent=` answering
`{state, seq, messages}` — with rule 3 above now normative. Bearer token per session per role,
delivered out of band (environment), never in argv (carried from 1.1 §13.3).

### 12.3 Other bindings

stdio, queues, and in-process buses are conformant if they preserve: envelope integrity,
addressee filtering, at-least-once or stronger delivery, and out-of-band credentials.

## 13. Profile: HACP Recursive Pairwise Profile *(normative profile)*

The first named profile, version `hive-recursive-pairwise/1`. Declared in capability
negotiation; binds only deployments that declare it. Normative **only here** (ADR-0001 §6.1):

- `max_direct_subordinates = 2` — supervisory arity is capped; growth goes downward.
- **LCA escalation routing** is the default escalation path shape (§11 walks it).
- **Sibling preauthorization**: siblings under one supervisor MAY hold standing
  `cross-branch` CapabilityGrants for named task classes, at the supervisor's discretion.
- The Supervisor role and the Verifier role SHOULD be held by different agents where the
  deployment can afford it (role independence, ADR-0001 §3).

Nothing in Core requires this shape; a deployment with 40 flat workers and no supervision is
Core-conformant.

## 14. Conformance *(normative)*

- **Vectors.** Every normative MUST in §3–§13 maps to at least one conformance vector in the
  reference implementation's test suite, named for its section.
- **Golden transcripts.** Each lifecycle (session open, negotiation, freeze, amendment, rework,
  escalation, no-agreement, cross-branch permit) has a golden wire transcript; implementations
  replay and byte-compare canonical forms.
- **Independence.** At least one conforming peer MUST be implementable — and be implemented —
  from this spec plus the canonical schemas and golden transcripts alone, importing neither
  HIVE nor the reference library. Ambiguities that force an implementer to read the reference
  code are spec defects.
- **Levels.** Core conformance and profile conformance are certified separately.

## 15. Relation to HACP/1.1

1.1 is frozen: implemented, 43 vectors, retained as the regression baseline. The mapping is
deliberately not item-by-item; where concepts carry (envelope, freeze, bounded negotiation,
verification, file edge) this document says "carried" and the semantics above govern. Where
they break (bilateral contracts, sessions, optional arbiter, addressable artifacts) 2.0 stands
alone. An implementation cannot be 1.1-conformant and 2.0-conformant simultaneously; they are
different protocols that share a philosophy.

## 16. Changelog

- 2026-09-04: skeleton drafted; §①–⑤ frozen at skeleton level; Phase S findings merged into
  §9.4 and §12. (W2)

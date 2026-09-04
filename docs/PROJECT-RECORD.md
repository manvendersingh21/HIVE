# The HACP/2.0 Protocol — Project Record

**Complete engineering record of designing, specifying, implementing, testing, and
proving a lab-neutral agent-collaboration protocol.** Written 2026-09-04, at the
milestone: *HACP/2.0 Core complete — every normative section implemented, tested by
two independent implementations, and exercised by real cross-vendor AI agents.*

This document is the map. Each section links to the artifact that holds the detail.

---

## 1. What was built, in one paragraph

HACP (**H**ive **A**gent **C**ollaboration **P**rotocol) /2.0 is a bilateral contract
protocol for AI agents: exactly two participants negotiate a task contract, freeze
it with a cryptographic digest, execute it, submit artifacts, and settle through
mechanical verification — with authority flowing down a declared organizational
chain, evidence flowing up, and collaboration sideways only when permitted. It has a
normative spec (`hacp/spec/HACP-2.0-draft.md`), a Rust reference implementation
(`hacp/src/v2/`), ten canonical JSON schemas, golden wire transcripts, an independent
Python peer built under an information barrier, and live proof: two cross-vendor CLI
agent pairs (claude×codex, agy×opencode) completing the full lifecycle over a file
edge, with the adapter mechanically refusing every false claim the agents made.

The prior protocol, HACP/1.1, was frozen mid-project (`docs/HACP-HIVE.md`) and
survives untouched as the reference baseline — its 43 conformance vectors still run
green in every test pass.

## 2. Timeline and commits

| Commit | Work package | What happened |
|:---|:---|:---|
| `aad98a0` | W1 — Phase S smoke | A real stock CLI (codex) driven through the 1.1 file-edge adapter against a throwaway sink; full lifecycle byte-exact; adapter crash-restart idempotency proven; 1.1 frozen; adapter-edge findings 1–7 recorded |
| `79a4a97` | W2 — Phase 0 | ADR-0001 (the boundary reset) + the full 2.0 spec skeleton with its section-freeze ledger §①–⑤ |
| `3f1c902` | W3 — harness first | Canonical-form module with rule-pinning vectors, schema emission pipeline + drift gate, golden-transcript harness (record/replay/diff) |
| `f1211b2` | W4 — kernel + contracts | Envelope/kind registry, agent identity, bilateral sessions with observers and capability negotiation, the full contract state machine, first goldens |
| `5e432dc` | W5 — §9 core | Artifacts with provenance walks, evidence, verification with the evidence-over-signals rule enforced by construction |
| `d5f8680` | W6+W7 — interop + exit | Independent Python peer (spec+schemas only); Phase 3 exit test: reference ↔ peer, mutual transcript agreement; spec defect found & fixed (digest preimage pinned) |
| `e148d4d` | W6 live | Real heterogeneous agents: `interop/live/hacp-live.py`; two cross-lab pairs settled; findings 8–10 (agents narrating success without artifacts) |
| `716d2e2` | preserved infra | The 1.1-era implementation layer (adapter, session host, watchdog, hacp 1.1 modules) committed per the freeze plan |
| `e58e434` | license | Relicensed GPL-3.0 → Apache 2.0 |
| `4a2a6fe` | Phase 4 — Core complete | §8 CapabilityGrant machinery, §10 cross-branch permits, §11 escalation ladder, §13 HIVE profile; three more schemas; testing playbook |

## 3. The architecture decision (ADR-0001)

The project's hinge was recognizing that 1.1 conflated three things, and separating
them ([`docs/adr/ADR-0001-hacp-core-is-bilateral.md`](adr/ADR-0001-hacp-core-is-bilateral.md)):

```
Layer 3   HIVE runtime          supervisor loops, spawning, org bootstrapping
Layer 2   HACP Recursive        max-two-children, LCA routing — a *profile*
          Pairwise Profile
Layer 1   HACP Core             bilateral sessions, contracts, artifacts,
                                evidence, verification, authority
```

- **The bilateral primitive**: a session has exactly two participants; multi-party
  is composed pairwise. Observers may watch lifecycle events but never author.
- **"Spawning is HIVE, delegation is HACP"**: the protocol never knows how a process
  starts; it only knows what two agents agreed to.
- **Five separated graphs** (ADR-0001 §4): org, task, contract, communication,
  artifact — each queriable without the others. The org chart never implies
  communication edges; provenance never implies supervision.
- **Authority flows down, evidence flows up, collaboration goes sideways only when
  authorized** (§10 permits), under the inviolable rule
  `grantee_authority ⊆ grantor_delegable_authority`.
- **Arbiters are optional at every N; `NO_AGREEMENT` is a valid terminal.** No rung
  of any ladder ever becomes mandatory.
- **Profile law stays out of Core**: max-two-children lives in
  `hive-recursive-pairwise/1` and binds only deployments that declare it. A flat
  40-worker deployment is fully Core-conformant.

Four decisions were locked by the owner before implementation and are visible in the
artifacts: 2.0 is a clean draft (never a retrofit of 1.1); 1.1's orchestrator is
shelved but its reusable infra preserved; every phase's deliverable is five-fold —
normative semantics, worked examples, canonical schemas, golden transcripts,
conformance vectors; and the Phase 3 exit test must use a peer importing neither
HIVE nor the reference crate.

## 4. The spec

`hacp/spec/HACP-2.0-draft.md` — the normative document, with a section-freeze ledger
(§① canonical form → §⑤ transports) so semantic layers froze in dependency order.
Highlights that made everything else work:

- **§5.1 canonical form**: byte-exact rules (sorted keys, integers only, minimal
  escapes, RFC3339 Z seconds) + SHA-256 → digests that two implementations can
  agree on without sharing code. Pinned by cross-language test vectors.
- **§5.3 kind registry**: 21 wire kinds, deliberately including **no `execute`
  kind** — EXECUTE is a lifecycle state entered implicitly at freeze.
- **§7 contracts**: propose → counter/accept ×2 → freeze → submit → verdict, with
  counter-resets-consensus, bounded rounds → `NoAgreement`, immutable revision
  history, and the freeze digest pinned to the canonical form of
  `{contract_id, revision, content}` (a leak-test catch; see §7 changelog).
- **§9.4 evidence over signals**: a verifier MUST NOT accept on exit codes or
  self-reports — measured originally when a sandbox-blocked CLI exited 0 having
  produced nothing.
- **§12 binding rules**: write rights are established at spawn time; exit codes are
  liveness, not verdicts; delivery filters by addressee. All three carry Phase S
  measurements.

## 5. The reference implementation

`hacp/src/v2/` — one module per spec section, vectors living beside the machine
they pin:

| Module | Implements | Load-bearing property |
|:---|:---|:---|
| `canon.rs` | §5.1 | digest vectors pinned against python3 `hashlib` |
| `envelope.rs` | §5.2–5.3 | forward-compat `extra` round-trip; URN minting |
| `agent.rs` | §3, §6.3 | capability vocabulary |
| `session.rs` | §6 | `[String; 2]` participants; observers can't author |
| `contract.rs` | §7 | full state machine; refusal-tested |
| `artifact.rs` | §9.1 | uuid4 + digest shapes; cycle-refusing provenance walks |
| `evidence.rs` | §9.2 | attestations, never truth |
| `verification.rs` | §9.3–9.4 | accept-without-basis impossible by construction |
| `grant.rs` | §8, §10 | monotonic authority at issue time, every layer; OrgChart + LCA; both permit paths |
| `escalation.rs` | §11 | raise→refer→resolve/no_agreement; arbiter never mandatory |
| `profile.rs` | §13 | HIVE profile law, held out of Core |
| `schema.rs` + `bin/emit-schemas.rs` | §14 | canonical schemas; drift gate both directions |

Ten schemas are committed under `hacp/spec/schemas/`; two golden transcripts
(`bilateral-lifecycle`, `bilateral-no-agreement`) replay on every test run.

## 6. How it was tested

The method is written up as a reusable playbook —
[`docs/TESTING-YOUR-PROTOCOL.md`](TESTING-YOUR-PROTOCOL.md) — six layers, each
catching a specific class of lie:

```
L0 canonical vectors         two builds disagree about the bytes
L1 refusal vectors           the engine permits what the spec forbids
L2 schema gate               the contract drifted from the model
L3 golden transcripts        the wire changed and nobody noticed
L4 independent peer          the spec can't be implemented from itself
L5 live heterogeneous agents the protocol assumes agents are honest
```

L4 is the leak test: `interop/peer-python/peer.py` is a second full implementation
(Python stdlib) built reading only the spec, schemas, and goldens. Its first real
act was catching a genuine spec defect — the freeze-digest preimage was
under-specified. The exit test (`interop/run-interop.sh`) drives a complete
lifecycle between reference and peer and asserts frame-for-frame transcript
agreement plus cross-implementation digest equality.

Current gate, enforced at every boundary: **344 workspace tests green, zero
warnings**, including the frozen 1.1 vectors.

## 7. Live proof — real agents, cross-vendor

`interop/live/hacp-live.py` runs the lifecycle with real CLIs as the two minds
(codex / claude / agy / opencode, any pairing) and the adapter as their hands. The
supervisor CLI authors contract terms; the worker CLI reviews the proposal and
executes the frozen contract; the supervisor CLI verifies; the adapter re-verifies
every claimed check before any accept reaches the wire.

**Settled pairs** (9 frames each, accept verdicts, transcripts committed):
`claude × codex` and `agy × opencode`.

The failures were as valuable as the successes —
[`docs/findings/adapter-edge.md`](findings/adapter-edge.md) holds all ten:

1–7 (Phase S): sandbox defaults read-only; exit 0 with zero work done; brief-as-file
works byte-exact; unfiltered sinks leak cross-role traffic; adapter crash-restart is
idempotent.

8–10 (live): two different CLIs confidently narrated file creation that never
happened; one resolved "current directory" to its own scratch space; every call
exited 0 regardless of outcome. **The §9.4 rule refused every one of them.** The
protocol's core bet — verification is mechanical, not social — held against real
agents lying to it, unintentionally but fluently.

## 8. What is frozen, what remains

- **Frozen**: HACP/1.1 (spec + vectors + implementation, never to be edited);
  HACP/2.0 Core §1–§14 (implemented, schema'd, golden'd, interop'd).
- **Preserved, shelved**: the 1.1 orchestrator/web/CLI and its reusable infra
  (adapter, session host, watchdog) — commit `716d2e2`.
- **Remaining**: the HIVE runtime — supervisor loops, spawn/delegate lifecycles,
  org bootstrapping on top of Core + the profile. That work inherits: a proven
  protocol, a testing method, and an adapter that already knows how real agents
  misbehave.

## 9. Reading order for a new implementer

1. `docs/adr/ADR-0001-hacp-core-is-bilateral.md` — why the boundary is where it is
2. `hacp/spec/HACP-2.0-draft.md` — the normative text (start at the ledger)
3. `hacp/spec/schemas/` — the wire contract
4. `hacp/tests/golden/` — worked examples of everything
5. `docs/TESTING-YOUR-PROTOCOL.md` — how to prove your implementation
6. `interop/` — the peer, the exit test, and the live harness

## 10. Run everything

```
cargo test -p hacp            # L0–L3 + refusal vectors, hermetic
interop/run-interop.sh        # L4 — reference ↔ independent peer
python3 interop/live/hacp-live.py \
  --run-dir /tmp/run --supervisor claude --worker codex \
  --schemas hacp/spec/schemas \
  --transcripts-out interop/live/transcripts   # L5 — live agents
cargo build --workspace && cargo test --workspace   # the full gate
```

Conformance tables with pasted proving commands live in
[`docs/HACP-HIVE.md`](HACP-HIVE.md) — the honesty rule there is the project's oldest
standing law: a row turns ✅ only after the command ran.

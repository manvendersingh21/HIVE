# HACP — Heterogeneous Agent Collaboration Protocol

**Version:** 1.1 · **Status:** implementable draft

HACP lets two or more **worker agents** — built by different organizations, running
different models, invoked through their own stock command-line tools — collaborate on
one goal. The agents agree on **interface contracts**: the decided abstractions between
them, stating what each side produces and consumes and what they exchange. Contracts are
protocol artifacts, not conversational context.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are to be interpreted as
described in RFC 2119.

This document specifies the protocol only. It names no product, no vendor, no model, no
programming language, and no implementation's endpoints or configuration. An
implementation binds the protocol to a concrete transport and runtime; a binding document
belongs with the implementation, not here. Conformance is defined in §15 and is testable
against the vectors distributed with this specification.

---

## 1. Design constraints

Three constraints produce nearly every decision below.

**C1 — Workers cannot be modified.** A worker is usually a *stock* agentic CLI that
cannot be taught a protocol. Therefore the protocol's **edge is files**: a worker reads a
brief and writes files in its own workspace. A thin, vendor-neutral **adapter** process
beside the worker owns all protocol behavior on its behalf. Any tool that can read a
prompt and write a file can participate.

**C2 — Participants do not trust each other's claims.** A worker reporting "done" is
evidence of nothing. Every claim a worker makes about its own output is re-verified
mechanically against the contract (§11).

**C3 — No participant learns who its peers are.** Actor names are vendor-neutral URNs.
A worker knows a peer exists and what that peer produces; it never learns the peer's
vendor, product, model, or address (§3).

## 2. Participants and roles

HACP separates two duties that a naive design merges into one "master". Keeping them
apart is what allows the two-agent case to work without an orchestrating intelligence,
and what makes the multi-agent case honest about needing one.

| Role | Reasons about content? | Duties |
|:---|:---|:---|
| **Coordinator** | **No — purely mechanical** | Assigns run and message ids, authenticates senders, enforces role binding, persists messages, imposes a total order, delivers to recipients. MUST NOT author, alter, summarize, or withhold protocol content. |
| **Arbiter** | **Yes — it is an agent** | Decomposes the goal, sizes the team, drafts and adjudicates contracts, rules on disputes, verifies reports, runs integration. |
| **Worker** | Yes | Reads its brief, works inside its own workspace, negotiates interfaces with peers, ends with a report. |
| **Adapter** | **No** | A file↔transport shuttle beside a worker. Relays messages without interpreting content, wraps outbound files into envelopes, synthesizes a fallback report, enforces deadlines by suspending. |

A conformant deployment MUST provide a coordinator. Whether it provides an arbiter is
determined by the topology (§4). One process MAY perform both the coordinator and arbiter
roles; the separation is of duties, not of machines.

The coordinator and the adapter are both **content-blind**: they MAY validate the *shape*
of a message, and MUST NOT act on its *meaning*. This is what keeps the protocol
auditable — every content decision is attributable to a named agent.

## 3. Actor naming *(normative)*

- Coordinator: `urn:hacp:coordinator:<name>`
- Arbiter: `urn:hacp:arbiter:<name>`
- Worker: `urn:hacp:agent:<role>-<run-short>`, where `<role>` is a coordinator-assigned
  lowercase role id (`a`, `b`, `api`, `store`, …) stable for the run, and `<run-short>`
  is the first 4–8 hex characters of the run id.
- Broadcast: `urn:hacp:all` — every participant on the run. Valid only in `to`.

`<name>` is a short deployment-unique string.

URNs MUST NOT encode vendor, product, or model. The mapping from URN to the actual tool
that runs a role MUST NOT appear on the bus, in any brief, or in any contract. It lives
only in the coordinator's private run record.

> **Why.** A worker that learns its peer is a particular vendor's tool will condition on
> that. The protocol's claim is that heterogeneous agents collaborate through *interfaces*,
> and that claim is only testable if identity is unavailable as a shortcut.

## 4. Topology *(normative)*

The number of workers is **a decision, not a configuration**. It is chosen per run from
the goal's structure (§7), and it determines the required topology.

```
  N = 1   solo        no contract required; conformant but degenerate
  N = 2   peer        A <-> B address each other directly; arbiter OPTIONAL
  N >= 3  federated   arbiter REQUIRED; peers still address each other directly
```

```
                              ARBITER
                          /      |      \            binding rulings,
                         /       |       \           contract authority
                   Agent A   Agent B   Agent C
                         \_______|_______/           direct peer traffic:
                                                     recorded by the coordinator,
                                                     not relayed by the arbiter
```

Rules:

- **Direct addressing.** A worker MAY address another worker directly (`peer.*`, §6).
  Such a message travels the same bus, under the same authentication and role binding, and
  is persisted in the same ordered log. "Direct" describes **addressing, not transport**:
  there is no side channel. The arbiter is not in the request path and MUST NOT author,
  rewrite, or suppress peer messages.
- **The arbiter is an authority, not a router.** It intervenes when asked (`dispute.raised`)
  or when the contract must change (§9), and its rulings bind.
- **N ≥ 3 requires an arbiter** because an unarbitrated disagreement among three or more
  parties has no bounded resolution.
- **N = 2 MAY omit the arbiter.** The two workers then negotiate the contract between
  themselves (`peer.proposal` / `peer.accepted`) and the coordinator freezes on mutual
  acceptance. With no arbiter, an unresolved disagreement past `respond_by` MUST fail the
  run rather than deadlock it.
- A run MUST NOT change topology class after `contract.frozen`. Adding a worker to a
  frozen run is a new run.

## 5. Message envelope *(normative)*

Every message is one JSON object:

```json
{
  "protocol": "HACP/1.1",
  "message_id": "m-3f0c1b2e-...",
  "run_id": "run-8a41...",
  "from": "urn:hacp:agent:a-3f0c",
  "to": "urn:hacp:agent:b-3f0c",
  "kind": "peer.question",
  "in_reply_to": "m-9d2e...",
  "timestamp": "2026-09-04T10:00:00Z",
  "body": { }
}
```

- `protocol` — `"HACP/<major>.<minor>"`. A differing **major** version MUST be rejected
  with an `error.protocol` body listing `supported_versions`. The same major with a
  **higher minor** MUST be accepted: minor versions are additive.
- `message_id` — unique (`m-<uuid>`). Edges are at-least-once; the coordinator MUST
  deduplicate on `message_id`, and re-ingesting a known id MUST be an idempotent no-op.
- `run_id`, `from`, `to` — the routing triple. An adapter MUST only ever emit `from` equal
  to its own agent URN, and the coordinator MUST enforce this against the sender's
  credentials (§13.3). `to` MAY be `urn:hacp:all`.
- `kind` — a string from the registry (§6). **Unknown kinds MUST be persisted and
  delivered, never rejected.** A consumer that does not recognize a kind MUST ignore it
  silently. This is the sole forward-compatibility mechanism, and it is why `kind` is a
  string rather than an enumeration.
- `in_reply_to` — causal link. REQUIRED on `answer`, `peer.answer`, `role.accepted`,
  `role.declined`, `contract.amendment.accepted`, `contract.amendment.rejected`,
  `dispute.ruling`, `report.verdict`, and `rework.completed`.
- `timestamp` — RFC 3339 UTC.
- Envelope size MUST NOT exceed 1 MiB. Artifacts are **referenced** (path plus digest),
  never embedded.

Unknown **body fields** MUST also round-trip unchanged.

## 6. Message kinds *(normative registry, v1.1)*

Kinds marked **1.1** are new since 1.0. No 1.0 kind changed shape.

| Group | Kind | Direction | Body (required fields) |
|:--|:--|:--|:--|
| session | `hello` | agent→coord | `CapabilityManifest` (§8) |
| session | `heartbeat` **1.1** | agent→coord | `{state, note?}` |
| run | `run.started` | coord→all | `{goal, deadline_secs?, participants: [urn...]}` |
| run | `run.plan` **1.1** | arbiter→all | `TaskDecomposition` (§7) |
| run | `run.completed` | coord→all | `RunSummary` |
| run | `run.failed` | coord→all | `{reason, last_state, preserved_paths: [...]}` |
| roles | `role.offer` | coord→agent | `{role_id, description, produces, consumes, required_capabilities, round}` |
| roles | `role.accepted` **1.1** | agent→coord | `{role_id}` |
| roles | `role.declined` **1.1** | agent→coord | `{role_id, reason}` |
| contract | `contract.drafted` | arbiter→all | `{contract, round, rounds_remaining, respond_by}` |
| contract | `contract.amendment` | agent→arbiter | `{target_version, rationale, additions, changes}` |
| contract | `contract.amendment.accepted` | arbiter→agent | `{new_version, note}` |
| contract | `contract.amendment.rejected` | arbiter→agent | `{reason}` |
| contract | `contract.frozen` | coord→all | `{contract, interface_digests}` |
| peer | `peer.question` **1.1** | **agent→agent** | `{about, text}` |
| peer | `peer.answer` **1.1** | **agent→agent** | `{text}` |
| peer | `peer.proposal` **1.1** | **agent→agent** | `{artifacts: [ArtifactSpec], rationale}` |
| peer | `peer.accepted` **1.1** | **agent→agent** | `{proposal_id}` |
| peer | `peer.rejected` **1.1** | **agent→agent** | `{proposal_id, reason}` |
| dispute | `dispute.raised` **1.1** | agent→arbiter | `{about, positions: [{agent, position}], question}` |
| dispute | `dispute.ruling` **1.1** | arbiter→parties | `{about, decision, rationale, binds: [urn...]}` |
| evolution | `contract.change.requested` **1.1** | agent→arbiter | `{artifact_id, change, reason, breaking}` |
| evolution | `contract.amended` **1.1** | arbiter→all | `{contract, new_version, interface_digests, changed: [artifact_id]}` |
| evolution | `interface.impacted` **1.1** | arbiter→consumers | `{artifact_id, was_digest, now_digest, what_changed, action_required}` |
| work | `work.started` | agent→coord | `{}` |
| work | `artifact.published` | agent→coord | `{artifact_id, path, sha256}` |
| mediation | `question` | agent→arbiter | `{about, text}` |
| mediation | `answer` | arbiter→agent | `{text, scope}` |
| reporting | `report.submitted` | agent→coord | `CompletionReport` (§10) |
| reporting | `report.verdict` | arbiter→agent | `VerificationResult` (§11) |
| rework | `rework.requested` **1.1** | arbiter→agent | `{report_id, failed_checks, round, rounds_remaining}` |
| rework | `rework.completed` **1.1** | agent→arbiter | `{report_id, summary}` |
| error | `error.protocol` | any | `{code, detail, supported_versions?}` |

**Peer messages are private between their endpoints.** The coordinator persists them for
audit and MUST deliver them only to `to` and to the arbiter when one exists. A worker MUST
NOT be delivered a peer message it is neither sender nor recipient of.

**On mediation vs. peer traffic.** An interface question SHOULD go to the peer that owns
the interface (`peer.question`). `question` to the arbiter is for questions about the
*contract* or the *goal*. An arbiter MUST NOT invent interface facts that the contract does
not contain; if it does not know, it says so, and the asker escalates or asks the peer.

## 7. Formation *(normative)*

Formation is the phase before any contract exists. It answers: *how many agents does this
goal need, what must each be able to do, and who gets which role?* Its output is a
`TaskDecomposition`, broadcast as `run.plan` so that the most consequential reasoning in a
run is auditable rather than implicit.

```json
{
  "decomposition_id": "d-1a2b...",
  "goal": "...verbatim...",
  "analysis": "Seven components; three share one interface, so a coordinating arbiter is required.",
  "components": [
    {
      "component_id": "ingest",
      "description": "Accept a job from the command line and persist it.",
      "required_capabilities": ["file-write", "shell"],
      "produces": ["job-store"],
      "consumes": []
    }
  ],
  "roles": [
    {"role_id": "a", "components": ["ingest"], "required_capabilities": ["file-write", "shell"]}
  ],
  "agent_count": 2,
  "topology": "peer",
  "rationale": "Two components with one shared interface: two agents, peer topology, no arbiter required."
}
```

Rules:

- `agent_count` MUST equal `roles.length` and MUST be consistent with `topology` per §4.
- Every `component_id` MUST appear in exactly one role's `components`.
- Every artifact named in a component's `consumes` MUST be `produces`d by some component.
- `rationale` MUST state why this count, not merely what the count is. An implementation
  that always emits the same count for every goal is not conformant with this section: the
  count is required to be *derived*.
- Role assignment MAY consider a candidate's declared capabilities (§8). Assignment is the
  only place capability claims may influence anything.

## 8. Capability manifest *(normative)*

Sent by every adapter as `hello`, before any other message:

```json
{
  "agent": "urn:hacp:agent:a-3f0c",
  "capabilities": ["file-write", "shell", "git", "report-json"],
  "declared_by": "adapter-default"
}
```

Capabilities are **declarative** — an adapter cannot introspect a stock tool — and are
therefore provenance, not proof.

- **Admission MUST NOT be gated on manifest contents.** An agent that declares nothing, or
  declares wrongly, MUST still be admitted. Anything else lets a mis-declared manifest lock
  a capable agent out of a run for no reason a human would endorse.
- **Assignment MAY be informed by manifest contents** (§7). This is the distinction that
  makes capability discovery useful without making it a gate.
- A manifest MAY carry additional fields for provenance. It MUST NOT carry vendor, product,
  or model identity, because the coordinator relays manifests to the arbiter and §3 forbids
  that leaking.
- `report-json` declares that the worker was briefed to write its own report. Its absence
  tells the coordinator to expect an adapter-synthesized one (§10).

## 9. InterfaceContract *(normative)*

The decided abstraction between workers: drafted, negotiated, frozen with digests, and —
new in 1.1 — **amendable after freeze through a controlled path**.

```json
{
  "contract_id": "c-1a2b...",
  "version": 2,
  "goal": "...the run's goal, verbatim...",
  "artifacts": [
    {
      "artifact_id": "job-store",
      "produced_by": "urn:hacp:agent:a-3f0c",
      "path": "src/store",
      "format": "file",
      "schema": null,
      "interface_files": ["api.md"],
      "symbols": ["submit_job", "get_status"],
      "examples": [{"input": "submit echo-hi", "output": "job-1"}],
      "check": {"kind": "command", "command": "make build-store"}
    }
  ],
  "dependencies": [{"consumer": "urn:hacp:agent:b-8a41", "consumes": "job-store"}],
  "integration": {"command": "make test"},
  "workspace_rules": ["agents write only inside their own workspace"]
}
```

**Digest scope.** A `sha256` claim in a report (§10) covers a **single file**. v1 defines no
canonical digest for a directory tree, so an artifact whose `path` is a directory cannot be
integrity-checked, and a verifier MUST report that check as *verified nothing* rather than
as a pass. `interface_files` are digested individually and are the mechanism that actually
freezes a directory artifact's interface.

**Resolving `interface_files`.** Each entry resolves relative to the artifact's `path` when
that path is a directory, and relative to the directory *containing* it when the path is a
regular file — a path cannot be joined onto a file. Digests are computed over the entries in
declared order; reordering `interface_files` changes the digest, so the order is part of the
frozen interface.
```

Field rules:

- `artifact_id` — unique within the contract.
- `path` — relative to the run's shared repository root (§14).
- `format` — `"rust-crate" | "json" | "file"`. When `"json"`, `schema` MUST be a valid
  JSON-Schema document; otherwise `schema` MUST be null.
- `interface_files` — paths, relative to the artifact, whose contents are frozen at
  `contract.frozen`. The canonical digest per artifact is sha256 over each listed file's
  bytes, files taken **in listed order**, newline-joined, hex-encoded, prefixed `sha256:`.
- `symbols` — grep-level interface claims, checked literally. Shallow by design (§11).
- `examples` — input/output pairs compiled into the run's acceptance test. In v1 both MUST
  be JSON strings. At least one example per consumed artifact is RECOMMENDED.
- `check` — a command run from the repository root that MUST exit 0 when the artifact is
  correctly built.

**Validation**, at draft and after every accepted amendment: the document parses; every
`schema` present is valid JSON-Schema; every artifact has exactly one `produced_by`; every
`artifact_id` is unique; every `dependencies` entry references an existing `artifact_id`;
no dependency cycle exists; `check.command` and `integration.command` are non-empty.

### 9.1 Negotiation before freeze

```
formation -> planning -> drafted(r=1) <-> amending -> drafted(r+1)      r <= max_rounds
drafted --(rounds exhausted OR no amendment past respond_by)--> frozen
```

- **The arbiter decides every version bump** — never a worker. Where there is no arbiter
  (N = 2), mutual `peer.accepted` on a `peer.proposal` is what advances the version, and the
  coordinator records it.
- Recommended adjudication: amendments that are **strictly additive** — new artifacts, new
  symbols, new optional examples — MAY be auto-accepted into a new draft. Amendments that
  mutate or remove anything existing MUST be rejected unless the arbiter can show they do
  not break a consumer. In doubt, reject with a reason.
- Each new draft MUST re-broadcast the **complete** contract. Workers are not expected to
  apply patches.
- `respond_by` bounds each round; silence past it counts as consent. `max_rounds` and
  `respond_by` together guarantee negotiation terminates.

### 9.2 Amendment after freeze *(new in 1.1)*

A frozen contract that can never change is brittle on precisely the goals HACP exists for —
those where no participant knows the right interface in advance. Freeze therefore keeps its
meaning while gaining one controlled door.

```
frozen/working --(contract.change.requested)--> amending --(accept)--> contract.amended
                                                        \--(reject)--> unchanged, work continues
```

- A worker that finds it must change a frozen `interface_files` MUST send
  `contract.change.requested` and MUST NOT change the file first. Changing it first is a
  violation, detected at verification (§11.3).
- The arbiter adjudicates. On acceptance it broadcasts `contract.amended` carrying the full
  contract, a bumped `version`, and fresh `interface_digests`.
- The arbiter MUST additionally send `interface.impacted` **to every consumer of each
  changed artifact**, and only to them. This is the protocol's answer to *"detect when one
  agent's work affects another"*: the consumer is told what changed and what it must do,
  rather than discovering it at integration.
- Where there is no arbiter (N = 2), the peer that consumes the artifact plays this part:
  the change requires its `peer.accepted`, and its acceptance is the impact acknowledgement.
- Amendments after freeze MUST be bounded by `max_amendments`. Exhausting it fails the run
  honestly rather than looping.

## 10. CompletionReport *(normative)*

The structured end of a role's work. "Done" is not a report.

```json
{
  "report_id": "r-...",
  "agent": "urn:hacp:agent:a-3f0c",
  "outcome": "success",
  "summary": "Implemented the job store and its status query.",
  "artifacts": [{"artifact_id": "job-store", "path": "src/store", "sha256": "sha256:...", "exists": true}],
  "diffstat": {"files_changed": 3, "insertions": 120, "deletions": 4},
  "tests": {"command": "make test-store", "passed": 4, "failed": 0, "output": "...tail..."},
  "contract_status": "satisfied",
  "deviations": [],
  "follow_ups": [],
  "evidence": {"log_path": "agents/a/agent.log"},
  "duration_secs": 240,
  "source": "agent"
}
```

- `outcome` — `success | partial | failure | blocked`.
- `contract_status` — `satisfied | deviated | partial | not-started | not-reported`.
- `source` — `agent | adapter-synthesized`. An adapter that produces the report on the
  worker's behalf (from exit code, diffstat, and artifact existence) MUST set
  `source: "adapter-synthesized"` and SHOULD set `contract_status: "not-reported"`.
- Per C2, **no consumer of a report may treat its contents as established** (§11).
- `evidence.log_path` MUST point at a retained log, so a human can see what the agent did.

## 11. Verification *(normative for the arbiter)*

For each `report.submitted`, in order, recording every check with its evidence:

1. **Existence** — each declared artifact exists at its declared path.
2. **Integrity** — sha256 of each artifact matches the report's claim, where claimed.
3. **Interface freeze** — the producer's `interface_files` digests equal the digests from
   `contract.frozen`, **or** from the most recent `contract.amended` (§9.2). This check is
   the heart of the protocol: it is how a decided abstraction stays decided, and how an
   *undeclared* change is distinguished from an agreed one.
4. **Build probe** — each artifact's `check.command` exits 0.
5. **Symbol check** — each `symbols` entry is found literally in the artifact tree. Shallow
   by design and stated as such: grep is not proof of semantics.
6. **Schema validation** — `format: "json"` artifacts validate against their JSON-Schema.
7. **Integration** — all worker outputs are merged and `integration.command` is run,
   including the **arbiter-authored acceptance test generated from the contract's
   `examples`**. Integration success is therefore the frozen contract executed, not any
   agent's self-assessment.

**Executing an `examples` pair.** For each pair, `input` is supplied on the artifact's
`check.command` standard input and the run passes if `output` appears in the command's
combined output. An artifact carrying `examples` but no `check.command` has no runnable
pair, and a generated acceptance test MUST say so visibly rather than silently omitting it —
an acceptance test that quietly tests nothing is worse than one that reports it cannot.

Each check yields `{name, passed, evidence}`. A failed check MUST quote its evidence:
paths, digests, command output tails. Deeper verification (property tests, mutation
analysis) is out of scope and MUST NOT be implied by a passing verdict.

### 11.1 Rework *(new in 1.1)*

A failed verdict SHOULD produce `rework.requested` to the responsible worker, carrying the
failed checks verbatim. The worker repairs and answers `rework.completed`, and the arbiter
re-verifies. Rework MUST be bounded by `max_rework_rounds`; exhausting it fails the run
with the last verdict attached. An implementation MAY set the bound to zero, which is
conformant and means "fail honestly on first failure".

## 12. Liveness *(normative)*

`respond_by` bounds negotiation, but nothing in 1.0 bounded `working` — one wedged worker
could stall a run until its global deadline.

- A worker's adapter SHOULD send `heartbeat` at least every `heartbeat_secs`.
- A coordinator that has seen no message from a worker for `3 x heartbeat_secs` MUST mark
  it unresponsive and notify the arbiter, which MAY reassign the role, proceed without it,
  or fail the run.
- A worker MUST NOT be considered failed merely for being quiet while its process is alive
  and its deadline has not passed; silence is a signal to check, not a verdict.

## 13. Transport binding

### 13.1 Requirements on any binding *(normative)*

A binding MUST provide: an **ingest** operation accepting one envelope, returning distinctly
for accepted / duplicate / protocol-mismatch / unauthorized; a **poll or push** operation
returning envelopes addressed to the caller (or to `urn:hacp:all`) after a caller-supplied
cursor, together with the run's current state; a **monotonic per-run sequence number**
establishing total order; and **at-least-once** delivery with `message_id` deduplication at
both ends.

A binding MUST NOT require a worker to speak it. Only adapters and the coordinator do.

### 13.2 The file edge *(normative)*

The adapter presents the protocol to its worker as ordinary files. This is the C1 surface,
and it is the only part of the protocol a worker sees.

```
  BRIEF.md                        the worker's prompt: role, goal, rules, templates
  INBOX/<seq>-<kind>.json         adapter-written, one file per message, ordered by name
  OUTBOX/<id>-<kind>.json         worker-written: {"kind": ..., "body": {...}, "to"?: ...}
  REPORT.json                     worker-written (SHOULD)
```

The worker writes `{kind, body}` and MAY write `to`; the adapter fills in `protocol`,
`message_id`, `run_id`, `from`, and `timestamp`, and supplies `to` when the worker omitted
it. `to` MUST default to the coordinator, and an adapter MUST validate a worker-supplied
`to` as a neutral actor URN (§3) and reject anything else.

`to` is not optional decoration: §6 requires a worker to address a peer **directly**, and
only the worker knows which peer it means. Without it the file edge cannot express the peer
channel at all, and a `peer.question` could only ever be sent to the coordinator — which is
the relaying master that 1.1 exists to remove. This is the one field where C1's "a worker
writes a JSON file" meets §6's "peers address each other", so it is the one field a
conforming brief MUST document. **A tool that can read a prompt and write a JSON
file is therefore conformant**, with no vendor cooperation of any kind.

`BRIEF.md` SHOULD stay under roughly two pages, and MUST state: the role, the goal, the
workspace rules, where INBOX and OUTBOX live, the `{kind, body}` shape, how to address a
peer, and that a report is expected at the end.

### 13.3 Security *(normative)*

- One high-entropy token per run per role, delivered to each adapter out of band. Every
  ingest and poll MUST present it.
- Tokens are **role-bound**: ingest MUST reject an envelope whose `from` does not match the
  token's role. A worker cannot impersonate its peer, the arbiter, or the coordinator.
- Peer messages are subject to the same binding. Direct addressing grants no extra trust.
- A coordinator MUST NOT log message bodies at ordinary verbosity; `(seq, from, to, kind)`
  is the ordinary record. Secrets MUST NOT appear in briefs, contracts, or logs.
- An implementation SHOULD apply safety supervision to worker output and SHOULD pause,
  rather than kill, a session it flags — killing destroys the state a human needs to judge
  the flag.
- v1 assumes adapters and coordinator share a trusted network. Transport security for
  untrusted networks is deferred to v2.

## 14. Run workspace *(normative)*

```
<run_root>/
  run.json                        coordinator-written: goal, roles, state (never tokens)
  DECOMPOSITION.json              the run.plan body (audit trail of team sizing)
  CONTRACT.json                   current frozen contract
  CONTRACT.draft.<n>.json         every draft round
  AMENDMENTS.jsonl                every amendment and change request, with its decision
  repo/                           shared repository, base revision
  integration/                    the merge of all worker outputs
  agents/<role>/
    BRIEF.md  MANIFEST.json  INBOX/  OUTBOX/  artifacts/
    workspace/                    where the worker actually does its work
    REPORT.json | REPORT.fallback.json
    agent.log                     the worker's tee'd output; supervision target
```

`workspace/` is named here because the rule below — a worker writes ONLY inside
`agents/<role>/` — leaves nowhere else for a worker's working tree to live. `MANIFEST.json`
is the worker's capability manifest (§8) as admitted; whichever component performs admission
writes it, and a run that never received one MAY omit the file.

- A worker writes ONLY inside `agents/<role>/`. The coordinator is the only writer of
  run-level files and the only merger of worker output.
- Artifact paths in a contract are relative to `repo/`; a worker realizes them inside its
  own workspace, and the coordinator merges for integration.

## 15. Conformance

**A conformant coordinator MUST:** assign all ids; validate envelope shape, protocol
version, authentication, and role binding; persist every message durably with a total
order; deduplicate by `message_id`; deliver peer messages only to their endpoints and the
arbiter; store and relay unknown kinds without failing; never author, alter, or withhold
protocol content; enforce §4's topology rules; emit a terminal `run.completed` or
`run.failed`.

**A conformant arbiter MUST:** produce a `TaskDecomposition` whose `agent_count` is derived
from the goal and whose `rationale` says why (§7); decide every contract version; bound
negotiation, amendment, and rework; freeze deterministically with `interface_digests`; send
`interface.impacted` to every consumer of a changed artifact and to no one else; verify
reports mechanically (§11) and never accept a report's self-assessment as evidence; rule on
disputes with a stated rationale.

**A conformant worker MUST** (through its adapter): send `hello` first; work only inside
its own workspace; never change a frozen `interface_files` without an accepted change
request; address peers directly rather than asking the arbiter for interface facts it does
not own; finish with a report, its own or the adapter's, marked as such.

**A conformant adapter MUST:** relay worker files unmodified, validating shape only; fill
envelope metadata correctly; deduplicate; mark synthesized reports `adapter-synthesized`;
enforce deadlines by suspending the worker's process group rather than terminating it.

Conformance is testable: the vectors distributed with this specification exercise envelope
round-tripping (including unknown kinds and unknown body fields), URN parsing, version
gating, contract validation and digest computation, decomposition consistency, and the
state machine's legal transitions. **An implementation is a reference implementation only
if it passes them; passing them is not a claim that it is the only correct one.**

## 16. Compatibility with 1.0

1.1 is additive. Every 1.0 message remains valid, and a 1.0 implementation ignores 1.1
kinds under §5's unknown-kind rule, degrading to master-mediated two-party operation.
Removed from 1.0 is one *prohibition*, not a feature: 1.0's "workers never talk to each
other" is lifted by §4.

## 17. Future work (non-normative)

- Transport security for untrusted networks; push delivery in place of polling.
- Federation: an arbiter appearing as a worker on another run's contract.
- Richer capability description, and capability *evidence* rather than declaration.
- Partial-failure semantics: completing a run when one worker of many is unrecoverable.

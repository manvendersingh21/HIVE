# HACP guardrails for delegated agents

Status: design brief, 2026-09-27. Scope: the web/chat delegation path
(`hive-web/src/delegation.rs`, `hive-core/src/delegation/`, the remote
`runner.py`). The `hive collab` runtime (`hive-core/src/runtime`) already runs
full HACP/2.0 bilateral contracts and is the reference for everything below;
reuse its code (`attest`, `hacp::v2` contract types, digests) instead of
writing a second implementation.

## Where enforcement lives

Every peer message between delegated agents already passes through the
coordinator. An agent writes a `peer` event to its own journal on its device.
`sync_run` reads it over SSH, stages it in `delegated_messages`, and delivers
it to the addressee's inbox. The coordinator is therefore the guardian: keys,
policy and the audit log live in the coordinator's database and never in an
agent workspace.

This is HACP Secure's **degraded (relay) mode**, and the code and UI must say
so. Agents run as the same OS user as their runner (yolo autonomy), so nothing
on the agent's own machine is a trust boundary. What the relay guarantees is
integrity and accountability *after* a message leaves the agent's journal: who
sent it, to whom, in which order, unmodified, within budget. It does not
guarantee that the agent's own reasoning was not injected.

## WP-A: secure peer relay

1. **Identity.** When a run is created, generate an Ed25519 keypair with the
   `hacp::secure` primitives (hacp `guardian` feature). Store the secret key only
   in the coordinator database (new table, file mode unchanged). Never serialize
   it into an assignment, a prompt, a journal or an API response. Expose only the
   public key and its fingerprint on the run.
2. **Envelope.** At staging time, build a canonical envelope:
   `{id, task_id, source, destination, kind, text_digest, seq, staged_at}`.
   `seq` counts messages per (source, destination) pair and is strictly
   increasing. Sign it with the source run's key and store the signature next to
   the message.
3. **Verify before delivery.** Before pushing a staged message into an inbox,
   recompute the digest and verify the signature. A mismatch is never delivered.
   It becomes an `integrity` incident on both runs, visible in the UI.
4. **Replay and order.** Reject a message whose `id` was already delivered with a
   different payload (this is already checked), or whose `seq` is not greater
   than the last delivered one for that pair.
5. **Rate budget.** At most N peer messages per run per minute (config, default
   10) and M per task per minute (default 30). Over-budget messages are held,
   not dropped, and the run shows why. This matches the HACP guardian's
   per-minute budget.
6. **Audit log.** Add an append-only, hash-chained table: each row stores the
   previous row's digest. It records stage, verify, deliver, hold and reject
   events. Add an API (`GET /api/runs/{id}/audit`) and a panel on the session
   page, labeled "Relay-attested (HACP Secure degraded mode)".

Tests: key never appears in any serialized run, assignment or API output;
tampered payload not delivered; replayed or out-of-order seq rejected; budget
holds and later releases; the audit chain verifies and detects a removed row.

## WP-B: coordination rules

1. **Ownership.** The planner schema gains `owned_paths` per assignment (repo
   globs). Two assignments in one task must not own overlapping paths
   (validate). The team manifest (WP-C) shows owners.
2. **Frozen interface contracts.** A peer `agreement` message becomes a HACP v2
   contract proposal between the two runs. When the other side answers with an
   `agreement` that references the same digest, the contract is frozen with
   `hacp::v2` canonical digests and stored. A later `agreement` changing a frozen
   contract is an amendment and needs both sides again. Show frozen contracts on
   both session pages.
3. **Evidence over claims (adapter-edge findings 2, 8, 10).** A run is not
   "completed" because its agent said so or exited 0. After the agent's final
   turn, the coordinator runs the assignment's acceptance checks in the workspace
   through the runner, mechanically: required files exist, stated commands pass.
   The coordinator records a verdict: `accept`, `rework` (bounded, default 2, with
   the failure fed back into the same conversation), or `no_agreement`
   (terminal, evidence kept). Reuse `runtime::attest`.
4. **Objective check (finding 11).** The review step compares the result to the
   assignment's objective, not only to its acceptance list, and says when they
   diverge.

## WP-C: team manifest (MULTI_AGENT_HACP.md, stage 1)

Each run has a profile: `{agent_id (run id), key, role (assignment key),
agent, model, device, owned_paths, current_task, status, dependencies,
last_seen}`. Hive serves the task's roster at `GET /api/tasks/{id}/team`.
Each agent's prompt gets a compact, relevant view (its peers' roles, owned
paths and status, not their transcripts), and it is refreshed when the roster
changes, the same way peer topology updates are delivered today. The session
page shows the team: who owns what, their status, and their dependencies.

## Order

WP-A needs the `hacp` bump with the `guardian` feature (branch
`chore/hacp-secure-bump`). WP-B and WP-C are independent of it and of each
other, except that WP-C displays WP-B's `owned_paths` once both exist. Each
lands as its own PR against `feat/container-workers`, and each passes every
step of `.github/workflows/ci.yml` before its PR is opened.

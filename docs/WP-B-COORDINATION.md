# Delegation coordination checks

Assignments carry `owned_paths`, prose `acceptance_criteria`, mechanical
`acceptance_checks`, and `max_rework` (default 2, configurable from 0 to 10).
The planner must supply at least one mechanical check for every new assignment.
For example:

```json
{
  "acceptance_checks": [
    {"kind": "file_exists", "path": "repo/report.json"},
    {"kind": "command", "argv": ["cargo", "test", "--locked"], "cwd": "repo", "timeout_seconds": 120}
  ],
  "max_rework": 2
}
```

Checks are fixed in the coordinator's assignment database. Commands use argv
without implicit shell expansion, run in a workspace-relative directory, and
have a 1–120 second timeout. An assignment supports up to 32 checks with at most
600 seconds of command time in total. File and working-directory checks reject
traversal and symlinks outside the workspace. The runner retains the last 8 KiB
of command output and kills the command's process group on timeout. Reviewed
autonomy retains the existing command policy: a check needing additional
authorization fails with that reason; it never silently gains yolo privileges.

Checks must verify artifacts that survive the worker's requested cleanup. They
must not implement or deploy the task. Prose criteria and the assignment's
objective remain part of the coordinator's separate objective-versus-result
review; a mechanical pass alone cannot settle that review.

After a native final turn, the coordinator imports the runner's completion
claim as `verifying` and asks the runner to execute the saved checks. It compares
the returned check list with the saved list and applies `runtime::attest::gate`.
Passing evidence yields `accept` and a `completed` run. A failure queues a
`rework` message in the same conversation, up to `ContractLimits.max_rework`.
The next failure yields terminal `no_agreement` with all measurements retained.
A completed task review requires accepted measurements for every run as well
as the existing nonempty objective-versus-result note.

The last acknowledged native message identifies the measured turn. Repeated
snapshots do not spend another rework round. Runner receipts prevent replaying
checks after a lost response; an interrupted execution fails closed. Evidence
is discarded if the native turn changes during measurement. Pending inbox
messages, provider quota pauses, approvals and peer waits do not consume
acceptance rounds. Verdicts and repair messages commit together in the
coordinator database. Current snapshot/check code can read older journals
without restarting their persistent native conversations.

Legacy assignments deserialize with an empty check list. They cannot obtain a
new mechanical accept from a successful native exit or a worker's claim.
Assignments without executable checks require a new plan with checks; their
missing-check evidence follows the same bounded failure path.

Both participants' session pages show the same stored HACP v2 frozen revision
and digest. A pending proposal or amendment has a separate proposal digest;
until both parties agree, an amendment leaves the previous frozen revision
visible. Agreement contracts remain collaborations using the pinned HACP
constructor; this change introduces no delegation contract or grant bypass.

Ownership validation reserves each glob's literal prefix conservatively,
including prefixes inside filenames. It can reject disjoint wildcard patterns
with the same prefix; use disjoint directory roots to make ownership explicit.
The existing team panel displays each assignment's owned paths; the contract
and acceptance panels sit alongside it.

This is relay-attested HACP Secure degraded mode. The coordinator initiates and
records measurements independently of worker claims, but an agent and its
runner share an OS user. The worker device is not a security boundary.

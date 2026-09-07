# Two-device dynamic collaboration — observed September 5, 2026 (Pacific)

Follow-up: [authorized runtime fixes and remaining limits](collaboration-fixes-2026-09-05.md).

## Outcome

OpenCode on the Mac mini and AGY on the MacBook Air completed a substantive
design → independent tests → implementation → rejection → revision → acceptance
exercise. The final artifact passed **43 tests on each device**, including
350 seeded shuffle/replay trials per device. The initial artifact failed one
independent requirement check on both devices; the agents corrected it through
peer feedback. One semantic repair round was needed.

This was **coordinator-mediated collaboration**, not native Hive distributed
orchestration. The coordinator supplied CLI invocation, SSH/SCP transport,
artifact materialization, test execution, and retry decisions. No application
source changes were made to implement missing Hive orchestration.

## Task and ownership

The scratch task was a pure Python job-log reconciler: reconstruct job ownership
and results from unordered, duplicated grant/completion/revocation events,
respecting cutoff time, epoch fencing, lease expiry, split-brain ownership,
deterministic tie-breaking, nested JSON results, and input immutability.

| Responsibility | Participant |
|---|---|
| Design, implementation, final integration decision | OpenCode on mini |
| Design challenges, independent tests, review and regression tests | AGY on Air |
| Independent requirement tests, transport, execution and output gates | Coordinator |

OpenCode selected `gemini-3.8-flash-high` from the supplied eligible worker-model
choices; AGY was invoked with that selection. Device placement was supplied by
the diagnostic, not dynamically discovered or scheduled by Hive. Both CLI
conversations were resumed across turns. No Claude model was invoked.

The peer was initially given the design but not the implementation. Once rules
were agreed, implementation and independent test generation ran concurrently
on the two devices. Five additional peer-owned regressions were also written
without seeing the revision.

## Observed exchange

1. The mini proposed deterministic rules and asked the peer concrete questions.
2. The Air identified a genuine divergence between two proposed serialization
   tie-breakers (`false` versus `0`), requested one agreed algorithm, broadened
   the conflict report, and requested acknowledgment of cutoff/epoch consequences.
3. The mini explicitly accepted the changes and consequences.
4. The mini delivered implementation; the Air independently delivered 22 tests.
5. Actual execution on both Macs produced 37 passes and one failure out of 38.
   All peer tests passed; the coordinator's post-revocation completion check failed.
6. The Air returned `rework`, acknowledged that its own negotiated wording and
   test coverage were too broad, and supplied a precise correction to the mini.
7. The mini acknowledged the requirement error and revised its implementation.
   The Air added five focused regression tests.
8. Both Macs passed all 43 tests. The Air reviewed the actual evidence and
   returned `accept`; the mini received that verdict and returned `complete`.

The critical failing case was grant at 10, revoke at 12, completion at 15,
snapshot at 17. The initial implementation returned `completed`; the original
requirement that revocation terminates a lease required `expired`. Both agents
had agreed to overly broad completion precedence. The revision rejects a
completion strictly after the earliest effective matching revocation, while
preserving their negotiated rule that exact-time ties favor completion.

This was not an intentionally inserted implementation bug. It arose from their
own proposal and agreement, and was exposed by the separate requirement test.

## Execution evidence

```text
First artifact — python3 -B -m unittest discover -v
  mini: Ran 38 tests in 0.029s; FAILED (failures=1); exit 1
  Air:  Ran 38 tests in 0.040s; FAILED (failures=1); exit 1

Revised artifact — same command
  mini: Ran 43 tests in 0.028s; OK; exit 0
  Air:  Ran 43 tests in 0.040s; OK; exit 0

Final suite composition
  22 original peer tests, including 100 seeded shuffle trials
   5 peer revocation regression tests
  16 coordinator tests, including 250 seeded shuffle/duplicate-replay trials
```

Code and all three test-file SHA-256 digests matched between devices. Final
implementation digest:

```text
ad3cc86510b907b3254ec8dee0f04e46573e84f95a008d9c5698c8016f462d42
```

## What failed or still required intervention

- **Structured-output reliability:** the first OpenCode design response used
  Markdown fences despite a JSON-only request. Its first repair response had
  malformed JSON (extra data at column 5308) and appeared to double-escape the
  source. Both were rejected before artifact use and required formatting retries.
  The coordinator did not repair their JSON or implementation by hand.
- **Timeout recovery:** the first implementation invocation exceeded the
  diagnostic's 240-second timeout. A concise retry delivered code in 8.23 seconds.
  This is an observed invocation timeout, not a diagnosed provider failure.
  The retry was coordinator-supplied, not demonstrated Hive failover or recovery.
- **Agreement did not guarantee correctness:** both agents agreed on the
  overbroad revocation rule, and all 22 original peer tests missed it. Independent
  requirement checking was necessary to expose the defect. The peer subsequently
  acknowledged and corrected its review gap.
- **No autonomous transport/control loop was exercised:** the agents generated
  task content and feedback, but the coordinator relayed messages and executed
  code. Automatic placement, peer discovery, unsolicited remote initiation,
  disconnect recovery, and native distributed contract orchestration remain
  unverified by this exercise. Previous runtime findings are not resolved by it.

AGY also produced extra protocol-like fields and duplicated nested payloads in
some JSON responses. They were retained as application payload, not interpreted
as authoritative envelope metadata. The diagnostic did not enforce a native
contract-body schema on those payloads.

## Exactly what HACP validation covered

An external Rust adapter importing `hacp::v2` was rebuilt before use and copied
to the Air. It wrapped successful model responses in ten HACP/2.0 envelopes
with opaque peer URNs and `x.teamwork.*` application-extension kinds. Each was
validated on both devices: envelope shape, expected session, authorized
participants, and request correlation/peer reversal where applicable.

Three negative controls rejected an unauthorized author, a wrong session, and
a wrong `in_reply_to`. These exercised the diagnostic's library-backed gate.

The adapters minted the outer envelopes; the models supplied the task payloads.
This does **not** certify the standard contract proposal/freeze, grants, artifact
submission, verification binding, escalation, or settlement state machines.
There was no formal HACP contract settlement in this run. Passing the finite
test corpus also does not establish correctness for every possible task input.

## Evidence and preserved state

Mini workspace: `/private/tmp/hacp-teamwork.x6iklr`.
Air workspace: `/private/tmp/hacp-teamwork.z1bgNw`.
The mini retains the brief, adapter driver, raw CLI outputs, request prompts,
model/session metadata, ten envelopes and validation logs, original and revised
source, independent tests, both execution reports, and negative-control evidence.
The first implementation timeout has a separate incident record; partial output
from that attempt was not retained by the initial diagnostic driver.

OpenCode session: `ses_f8bf9029affeU0dpRf7bHmGe9V`.
AGY conversation: `b618f61b-ce03-4b78-9d39-9ed631be1eaf`.

The pre-existing tracked working-tree diff was unchanged throughout:

```text
8d0f94f1a2147ebad6d76ccebfa365134f743e08045ad2919c7177966ba805cd
```

Only audit documentation was added/updated in the repository. Diagnostic code
and task artifacts remained in temporary workspaces. No production database,
service, credentials, SSH trust configuration, or existing agent session was
modified for this test; the new CLI conversations remain in normal CLI history.

# Collaboration fixes — September 5, 2026 (Pacific)

Follow-up to the [distributed audit](distributed-orchestration-2026-09-05.md)
and [two-device collaboration exercise](hacp-two-device-collaboration-2026-09-05.md).
The user authorized code fixes after the observation runs.

## Fixed and verified

| Finding | Change | Verification |
|---|---|---|
| D4: reachable host selected despite missing tmux | Health now checks the actual non-login SSH launch prerequisites: tmux, bash, tee and tail, including tmux/bash version execution. Unready hosts are `Unhealthy`. This does not claim model authentication. | Regression and live Air probe: `Unhealthy; selected = None`. |
| D5: missing CLI inventory | Inventory includes OpenCode and AGY; inferred agent capability covers every CLI the runtime supports. | Regression plus actual probes: OpenCode found on mini; AGY found on Air. |
| D6: wrong Mac page size | Parse the page size reported by `vm_stat`; do not assume 4096 bytes. Missing headers yield no estimate. | Identical page-count fixtures yield 2.00 GiB at 16384 bytes and 0.50 GiB at 4096 bytes. Live estimates were 6.08 GiB on mini and 1.25 GiB on Air at the probe time. |
| D7: load grows after completion | A completion sentinel releases the assignment's shared load reservation exactly once. | Two simultaneous reservations, repeated completion, and lost-supervision tests. |
| D8: duplicate receipts | Exact redelivery of a session/message ID adds no second transcript frame; changed content under the same ID is rejected. | Repository regression and previously failing independent acceptance test. |
| D9: false artifact acceptance | Independently enumerate frozen criteria, output line constraint and manifest digest/size; required words use case-sensitive whole-word measurement, not nonemptiness. A final unterminated line also counts. | Both previously failing independent tests now pass; added omitted-criterion, word boundary/case, unknown-criterion and line-count regressions. |
| Invalid control JSON | Terms, decisions and verdicts must be JSON objects. One retry after an exited invocation; rejected bytes retained beside invocation logs. No silent stripping of fences or manual JSON repair. | Fenced JSON, trailing garbage and array responses recover after one audited retry; persistently invalid output still fails. |
| Unsafe timeout retry | `TimedOut` stops the workflow for session inspection, even if an output file exists. It cannot launch a retry beside a possibly frozen or still-running agent. | Regression asserts one worker invocation and an explicit unfinished session. |

Acceptance arrays no longer silently discard malformed or empty entries, and the
single-output schema rejects a non-boolean `one_line` or multiple outputs. The
run report separately records independently measured frozen obligations.

The acceptance evaluator deliberately supports a small, documented language:
file existence, nonemptiness, one line, required/ending words, and manifest
digest/size matching. Supported conjunctions require every clause to pass.
Unsupported prose blocks acceptance rather than being interpreted as success.
The authoring brief now names the supported forms. This is not a general task
correctness evaluator or proof that a negotiated contract captures the objective.

## Verification evidence

```text
cargo build --workspace --offline
  PASS, zero warnings
cargo test --workspace --offline --no-run
  PASS, zero warnings
cargo test --workspace --offline --quiet
  TOTAL: 544 passed, 0 failed, 5 ignored
interop/run-interop.sh
  test an_independent_peer_interoperates_over_the_file_edge ... ok

Independent functional audit, rebuilt against the changed repository:
  17 passed, 0 failed (previously 14 passed, 3 failed)

Rebuilt library-backed, read-only inventory diagnostic:
  mini: opencode and agy present
  Air: agy present; tmux absent
  Air pool health = Unhealthy; selected = None
```

Live checks contacted only the mini and configured Air. No agent model was billed
for this fix verification. Earlier live collaboration evidence remains valid for
its stated diagnostic scope, not as a fresh run of the changed runtime.

## Not fixed by this patch

- **Native distributed HACP hosting, master-controlled device/model scheduling,
  and peer agent discovery (D1–D3):** still missing product features. The manual
  two-device success does not implement them. They need a separate runtime slice.
- **Durable queue/restart recovery (remaining D8):** receipt deduplication uses the
  current side's in-memory transcript. The latest-file reader is not a durable
  acknowledged inbox. No exactly-once application execution is claimed.
- **Advanced orchestration (D10):** counteroffer/amendment loops, recursive
  delegation, escalation and automatic rework/resumption are still not integrated
  into a distributed host lifecycle.
- **Lost supervision:** an assignment without a confirmed completion retains its
  load reservation and makes its worker unhealthy. Health refresh cannot clear
  this uncertainty within the pool. Operator reconciliation is still required;
  this state is process-local, not durable across a master restart.
- **Air setup:** tmux remains absent. AGY authentication already worked in the
  earlier live exchange; no login fix was needed for that path. Codex login and
  reverse SSH trust were not rechecked or changed. The optional worker HTTP daemon
  is not required for SSH and does not need installing to fix these bugs.
- **Agent reasoning:** the scratch reconciler's revocation bug was already repaired
  by the two agents, passing 43 tests per device. It was not Hive application code.
  Agreement still is not proof of correctness; independent requirements tests remain
  necessary. Provider latency and model-generated malformed JSON cannot be eliminated
  by changing HACP; this patch adds bounded runtime handling, not that guarantee.

No HACP wire schema or standalone library semantics changed in this follow-up.
Existing unrelated working-tree edits were preserved. No credentials, SSH trust,
production database, running service or remote software installation was changed.
Debug workspace binaries were rebuilt; production services were not restarted.

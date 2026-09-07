# Release 1 — native, recoverable two-device collaboration

> Dated acceptance evidence. The later [repository separation](REPOSITORY-SPLIT.md)
> moved HACP's tests to its own repository; combined totals below describe the
> original run and remain unchanged as historical evidence.

Scope: OpenCode supervises on the Mac mini, a worker runs on the Air, and HIVE
coordinates HACP negotiation, execution, independent verification, rework and
settlement without an external coordinator. HACP remains a standalone library;
the frozen 1.1 binding is not silently upgraded.

## Required gates

1. Durable v2 sessions: persist identities, protocol messages, receiver receipts,
   revisions, invocation identities and execution state. Inspect and resume a run.
   Inject restarts at lifecycle boundaries: accepted messages survive and an
   uncertain or existing invocation is never blindly executed again.
2. Native remote hosting: per-role local/SSH host and model selection, isolated
   workspaces, digest-checked transfers, logs and supervision. One HIVE command
   runs the mini–Air pair through negotiation, freeze, execution, verification
   and settlement. Check actual agent authentication, not just installed binaries.
3. Useful coding collaboration: multiple files/patches, executable acceptance
   tests, clarification/counteroffers, bounded rework and conversation continuity.
   Repeat the hard reconciler exercise through native HIVE: independently authored
   tests expose a defect, feedback reaches the worker, and repaired work passes on
   both devices. Do not substitute the one-line artifact smoke test for this gate.
4. Shipping checks: warning-free workspace build and test compilation, workspace
   tests, independent protocol interoperability, fault-injection tests and recorded
   live evidence. Preserve unrelated pending changes. No claim of completion before
   all of these checks have actually run against rebuilt code.

## Implementation decisions

- A private SQLite journal belongs to the HIVE v2 runtime, separate from the legacy
  1.1 bus. Commit outgoing envelopes before exposing them to transport; commit
  validated receiver receipts before acknowledging delivery.
- Recovery replays recorded protocol inputs through the HACP library, retaining
  the original IDs and checking replay divergence. Invocation effects are outside
  the database transaction: record intent before launch, recover the existing host
  handle after interruption, and fail closed when its fate cannot be established.
  This is not an exactly-once shell-execution guarantee.
- Device/model selection in this release is explicit per role. Autonomous fleet
  scheduling and peer discovery remain Release 2, not implied by successful SSH.
- Initial live pairing uses the working Air AGY installation. Claude is excluded;
  Air Codex remains optional pending a fresh successful authenticated request.

## Evidence

Release 1 implementation and acceptance gates are complete as of 2026-09-07.
This is a verified working-tree milestone, not a pushed tag, registry publication
or production deployment. The sections below retain chronological evidence,
including failures and previously outstanding gates; the final audit supersedes
those earlier progress snapshots.

### Durable foundation — verified 2026-09-05

Implemented private WAL journal, stable run/artifact/message identities, ordered
outbox replay, validated receiver receipts, typed HACP state projections,
invocation intents/results and an OS-backed exclusive coordinator lock.
`hive collab inspect <run-dir>` reads the durable state; `hive collab resume
<run-dir>` reconstructs protocol state and recovers unfinished host invocations.
Completed invocation outputs are digest-checked, not silently replaced or rerun.

Commands and observed results:

```text
cargo build --workspace --offline
  Finished dev profile; zero warnings
cargo test --workspace --offline --no-run
  Finished test profile; zero warnings
cargo test --workspace --offline --quiet
  TOTAL: 550 passed, 0 failed, 6 ignored
interop/run-interop.sh
  an_independent_peer_interoperates_over_the_file_edge ... ok
cargo test -p hive-core --offline runtime::lifecycle::tests::restart_at_every_durable_boundary -- --nocapture
  durable restart matrix: 31 recovered, 4 ambiguous launches refused, no repeated effects
  1 passed; 0 failed
cargo test -p hive-core --offline collab::session::tests::live_recovery_reattaches -- --ignored --test-threads=1 --nocapture
  live_recovery_reattaches_after_host_restart_without_relaunch ... ok
  1 passed; 0 failed
git diff --check
  exit 0
```

The six default-suite skips include the new live recovery test, which was run
explicitly above. The restart matrix injects interruption after each SQL commit;
the live test replaces the host object while its real detached tmux command runs,
then checks exactly one start and completion in the log. These are not yet a full
coordinator-process crash matrix across two devices.

Remaining at that checkpoint: native SSH role hosting/model options and artifact transfer; multi-file
coding contracts, executable independent tests, counteroffers, conversation
continuity and automatic bounded rework. Recorded safety pauses remain paused:
the new resume command does not silently send SIGCONT. An explicit audited
operator continuation/reconciliation path is still required. Remote supervision
and recovery must be tested with actual disconnects before Gate 1 is complete.

Air setup: Homebrew installation of tmux completed; login-shell probe reports
`tmux 3.7b` and resolves AGY. Non-login SSH does not resolve tmux. Use a checked
login-shell environment in the new remote host, not the legacy worker's PATH.

### Native hosting and multiple outputs — implemented, live gate pending

`hive collab run` now accepts `--supervisor-host`, `--worker-host`,
`--supervisor-model` and `--worker-model`. Omitted hosts mean local execution;
omitted models retain the CLI default. Explicit model identifiers are passed
unchanged. Both placements are private run configuration/report fields and are
reloaded by `resume`, not placed in HACP messages. Default run-directory names
are neutral UUIDs so the physical paths in briefs do not expose the pairing.

The SSH host uses strict existing SSH trust and a login-shell environment. A
bundled standard-library Python helper performs only process/file plumbing;
the Rust HIVE lifecycle and HACP library retain all protocol decisions. No
external agent coordinator or worker daemon is needed. Each role gets its own
workspace; only submitted artifacts are copied into the verifier's workspace.
Transfers preserve executable bits, compare SHA-256 at both ends and reject
symlinks, traversal, duplicate paths and oversized workspaces. Limits are 32 MiB
and 2,048 files per transfer; `.git` and `__pycache__` are excluded.

Contracts can declare 1–128 text output files, including nested paths. Every
output is required after execution and bound separately into the submission and
verification records. Completion receipts cover all outputs, not just the first.
Case-insensitive collisions and file/directory conflicts are refused for portable
mini/Air workspaces. Global file criteria currently apply to every output;
passing these checks does **not** establish that source code is functionally correct.

```text
cargo build --workspace --offline
  Finished dev profile; zero warnings
cargo test --workspace --offline --no-run
  Finished test profile; zero warnings
cargo test --workspace --offline --quiet
  TOTAL: 557 passed, 0 failed, 9 ignored
cargo test -p hive-core --offline runtime::hosting::tests::local_remote_host -- --ignored --test-threads=1 --nocapture
  local_remote_host_disconnect_does_not_repeat_or_overwrite_live_work ... ok
  local_remote_host_pauses_on_timeout_and_watchdog_without_killing ... ok
  2 passed; 0 failed
interop/run-interop.sh
  an_independent_peer_interoperates_over_the_file_edge ... ok
git diff --check
  exit 0
```

The two explicitly run host tests use real local tmux behind a loopback SSH shim:
one injects a transport failure and verifies that stale local files cannot
overwrite the existing task; the other verifies actual stopped process groups
and SIGCONT after timeout and watchdog detection. They do not prove a networked
Air run. `live_ssh_invocation_recovers_and_transfers_without_relaunch`, invoked
with `HIVE_TEST_SSH_HOST=mac-air`, failed at initial SSH connection timeout,
before any remote task launched. Subsequent direct SSH probes also timed out.
This is currently a reachability failure, not an observed agent login failure.

Still required at that checkpoint: executable independent acceptance tests, clarification and
counteroffers, conversation continuity, bounded automatic rework, audited
operator continuation, complete coordinator-process crash testing, and the
actual hard two-device coding acceptance run once the Air is reachable. No
Release 1 gate is claimed complete on the strength of the local shim tests.

### Executable verification, negotiation and repair — implemented

Contracts may include `verification.files` (independently authored fixture paths
and source) and `verification.commands` (program, argv and bounded timeout).
Fixtures are frozen with the terms and cannot collide with worker outputs.
HIVE copies submissions into separate test workspaces and executes the frozen
commands through both selected role hosts. Command outcomes and log digests are
durable receipts. A failed executable check overrides an agent's unsupported
`accept` with HACP rework; a later attempt must submit fresh artifact identities
against the same frozen revision. Earlier workspaces and logs remain intact.

`--max-rework` defaults to 2 for new CLI runs and is capped at 5. Exhaustion
leaves the HACP contract executing and returns an unfinished result, not success.
Worker counteroffers and clarification questions now reach the supervisor;
revised terms require explicit worker acceptance. The HACP library controls
round exhaustion and agreement before freeze.

OpenCode and AGY calls request structured output and retain explicit conversation
IDs in private receipts. Each invocation binds its original input ID; replay
cannot replace it with the latest conversation. No implicit "continue last
session" flag is used. Other CLIs still use independent calls; their persistent
conversation adapters are not claimed implemented.

An operator can continue a recorded suspension with:

```sh
hive collab resume <run-dir> --continue-session <recorded-session> --reason '<review rationale>'
```

Approval commits before SIGCONT, preserves the prior pause receipt and renews
that invocation's timeout. Only already-reviewed log bytes are exempted from
rescanning; new output remains supervised. Crash recovery can reapply a pending
approval idempotently without launching a replacement process.

```text
cargo build --workspace --offline                         -> zero warnings
cargo test --workspace --offline --no-run                 -> zero warnings
cargo test --workspace --offline --quiet                  -> 563 passed, 0 failed, 10 ignored
interop/run-interop.sh                                   -> independent file-edge peer passed
cargo test -p hive-core --offline runtime::lifecycle::tests::restart_at_every_durable_boundary -- --nocapture
  -> 35 recovered boundaries, 4 ambiguous launch intents refused, no repeated effects
cargo test -p hive-core --offline collab::session::tests::live_approved_cursor -- --ignored --nocapture
  -> 1 passed: real SIGSTOP, approved cursor, another new violation still stopped, SIGCONT
cargo test -p hive-core --offline collab::session::tests::live_exit_code_survives -- --ignored --nocapture
  -> 1 passed: short-lived commands retain their actual exit code
```

The executable-rework regression runs real Python unittest commands against an
incorrect function returning 41, on both injected role hosts. Both fail despite
an agent's accept record; HIVE requests rework, a repair returns 42, and both
commands pass. Replay preserves both attempts and invokes neither models nor
test commands again. Model responses in this regression are scripted; it is not
the hard live two-agent acceptance run.

### Local stock-CLI evidence — not the two-device gate

A rebuilt native HIVE run used two independent OpenCode sessions on the mini,
both explicitly selecting `zai-coding-plan/glm-5.3`. Objective: implement numeric
addition and freeze three independent standard-library unittest cases covering
integers, negatives and floats. Evidence remains in the scratch run
`hive-r1-native.JAF155`; HACP session `s-46a9a4d51b97`.

```text
SETTLED — verdict accept
frames: 10
agent runs: 4
acceptance executions: 2, both exit 0
each acceptance log: Ran 3 tests ... OK
corroboration: 6 backed, 3 unmatched, 0 contradicted
replay after settlement: same result, all six execution-log SHA-256 values unchanged
final tmux check: verifier session no longer exists
```

The supervisor retained `ses_f8ba05f93ffeioSDi6WoMjLYBi` across authoring and
verification; the worker retained `ses_f8b9fcf44ffe3w3XgXKL0nVpl0` across review
and implementation. These are private CLI identities, not HACP agent identities.

This was **not unattended**: the supervisor attempted an unnecessary scratch
cleanup while rechecking tests, twice triggering Tier 1. The first command failed
before execution because its working directory did not exist. The second was
confined to the previously absent disposable verification sandbox. Both pauses
were inspected, recorded with explicit reasons and resumed through the new CLI
path; the same verifier process and conversation completed. Subsequent violations
were not globally waived. The model also initially chose an incorrect test
working directory; HIVE's own frozen-suite executions were already correct.

Remaining acceptance work: strengthen command-result edge cases (including empty
test discovery), run full coordinator-process crash tests, verify AGY continuity
and remote continuation against the reachable Air, and run the hard reconciler
collaboration with live independent agents across both devices. Air SSH still
times out before authentication. No Release 1 completion is claimed.

### Empty-suite refusal and abrupt coordinator death — verified

Direct Python `-m unittest` acceptance commands must now report a complete,
successful nonempty suite with at least one test not skipped. A zero exit with
zero discovered tests, an entirely skipped suite or a missing summary cannot
settle a contract. Actual Python regressions demonstrate the first two cases
on both role hosts despite a scripted verifier's accept; replay repeats neither
models nor commands. The run summary labels exit-zero counts as exit statuses,
not passed acceptance checks. Custom scripts retain exit-status semantics:
this is not a generic test-count detector or a defense against fabricated logs.

The process-crash matrix launches a separate coordinator test executable and
sends SIGKILL to that child immediately after each durable commit, bypassing
Rust destructors and SQLite connection shutdown. It covers six scripted model
calls, four real Python acceptance executions, failed work, automatic rework
and settlement. A fresh process recovers the journal and host evidence; exclusive
launch markers refuse any repeated effect. Every previously accepted envelope
is compared byte-for-byte after recovery. The production binary has no crash
environment switch; this injection is compiled only into tests.

```text
cargo build --workspace --offline                         -> zero warnings
cargo test --workspace --offline --no-run                 -> zero warnings
cargo test --workspace --offline --quiet                  -> 565 passed, 0 failed, 13 ignored
interop/run-interop.sh                                   -> independent peer passed
cargo test -p hive-core --offline sigkill_at_every_commit -- --ignored --nocapture
  -> 50 recovered boundaries, 10 ambiguous launch intents refused
     accepted envelopes unchanged, no repeated effects
cargo test -p hive-core --offline local_remote_host -- --ignored --test-threads=1 --nocapture
  -> 3 passed: approved cursor/new violation, disconnect/stale mirror,
     timeout/watchdog pause and continuation
cargo test -p hive-core --offline real_empty_or_fully_skipped -- --nocapture
  -> 1 passed, exercising empty and entirely skipped real Python suites
hive collab resume <retained-local-native-run>
  -> existing OpenCode/OpenCode run remains SETTLED with its original session
```

The crash matrix's host evidence is file-backed and its model outputs are
scripted: it does not yet kill a coordinator while a detached live agent is
still running. The three remote-host tests use real tmux and the bundled helper
behind a loopback SSH shim, not a reachable second device. Remaining gates are
live-task coordinator-crash recovery, actual Air AGY conversation/continuation
and disconnect checks, and the hard native two-device reconciler acceptance.
A fresh Air SSH probe still timed out before authentication. No production
service was restarted, no remote model task launched, and no release was pushed.

### Running tasks survive coordinator death — verified

The new live-process matrix holds a deterministic task open in a real detached
tmux session at each of ten invocation stages: author, review, initial work,
both initial acceptance checks, initial verification, repair work, both repaired
acceptance checks and final verification. The test controller confirms the task
PID and exact tmux session are alive, SIGKILLs only its own coordinator child,
starts a new coordinator, and verifies that the same PID/session remain alive
after reattachment before releasing the task. Every run settles after the
required repair. Exclusive launch markers count exactly ten launches, and
previously accepted envelopes remain unchanged.

The fixture adapter supplies deterministic model outputs, but process launch,
completion markers, log supervision and reattachment use the production
`LocalSessionHost`. This proves live local process recovery, not provider
authentication or two-device network behavior. No live agent was killed.

```text
cargo build --workspace --offline                         -> zero warnings
cargo test --workspace --offline --no-run                 -> zero warnings
cargo test --workspace --offline --quiet                  -> 565 passed, 0 failed, 14 ignored
interop/run-interop.sh                                   -> independent peer passed
cargo test -p hive-core --offline live_tasks_survive_coordinator -- --ignored --nocapture
  -> all 10 stages passed; original task PID/session survived and were reattached
cargo test -p hive-core --offline sigkill_at_every_commit -- --ignored --nocapture
  -> 50 recovered, 10 ambiguous intents refused; no repeated effects
cargo package --manifest-path hacp/Cargo.toml --allow-dirty --offline
  -> 51 files packaged; isolated package compilation passed
cargo test --manifest-path target/package/hacp-1.1.0/Cargo.toml --offline --quiet
  -> 146 passed, 0 failed, including the independent Python peer
git diff --check                                        -> exit 0
```

The local HACP package is not a published registry release. It retains package
version 1.1.0 and the separate v2 draft API; no wire-version migration is implied.

Current external gate: direct Air SSH still times out. The installed Tailscale
app's status reports the mini online and the Air **offline**. No authentication
failure is inferred. The symlinked `tailscale` CLI crashes on this installation;
invoking the existing application's actual executable provides the read-only
status successfully. No network settings or installed software were changed.

Remaining Release 1 acceptance: actual Air AGY conversation continuity, remote
continuation/disconnect recovery and the hard native mini–Air reconciler run
with independently authored tests, observed defect feedback, bounded repair
and passing checks on both devices. The local matrices do not substitute for
these gates. Release 1 remains in progress.

### Air reachable again — remote recovery verified 2026-09-07

After the Air was switched on, SSH succeeded. Its login-shell environment resolves
AGY and tmux 3.7b with Python 3.9.6. The native remote-host recovery/transfer test
now passes on the actual Air, rather than only through the loopback shim.

A second real-Air test records a watchdog pause, approves its echo-only fixture
output, resumes the same remote process and injects one host-local transport
failure before acknowledging that continuation. A new host replays the pending
approval idempotently. A second newly printed violation still pauses; another
recorded approval completes the original task with exactly one start. Completed
host evidence refuses relaunch. The injected observation failure does not modify
device network settings or disconnect unrelated sessions.

```text
HIVE_TEST_SSH_HOST=mac-air cargo test -p hive-core --offline live_ssh_invocation -- --ignored --nocapture
  -> 1 passed on Air: reattachment, checked artifact transfer, no relaunch
HIVE_TEST_SSH_HOST=mac-air cargo test -p hive-core --offline live_ssh_approved -- --ignored --nocapture
  -> 1 passed on Air: recorded continuation, lost observation, new violation,
     idempotent recovery, original process completion
cargo build --workspace --offline                         -> zero warnings
cargo test --workspace --offline --no-run                 -> zero warnings
cargo test --workspace --offline --quiet                  -> 565 passed, 0 failed, 15 ignored
interop/run-interop.sh                                   -> independent peer passed
```

The hard native collaboration has been launched in `hive-r1-distributed.POnDwS`:
OpenCode supervises locally with `zai-coding-plan/glm-5.3`; AGY is assigned to
`mac-air` with `gemini-3.8-flash-high`. The objective requires two source modules,
at least 25 independently authored tests, 350 deterministic replay/shuffle trials,
and up to two repair rounds. At this checkpoint authoring is still running; no
settlement, AGY authentication or conversation continuity is claimed from merely
listing models or assigning the remote role.

### Hard native run and launch-size fix — 2026-09-07

The first author invocation exhausted its model generation limit without writing
the contract. HIVE retained that receipt and used its one bounded output retry,
in the same conversation. The retry produced a 57,260-byte contract with an
independently authored 61-test suite. That exposed a real host bug: tmux rejected
the full command message as too long before the Air's review could start.

Local and SSH hosts now pass tmux a short path to a private launch script rather
than the entire command. The agent-argv size guard remains; arbitrary-sized
agent prompts are not newly claimed supported. The original failed run and its
unresolved launch intent are preserved, not deleted or treated as permission to
relaunch. A replacement native run reused the exact independently authored
contract, digest `90dbe4280ac98e1258df622e01f3c920893f6c4d7aceb2c4c0ef907b8184e0f8`.

`hive-r1-distributed.OgNSmY`, session `s-5adfd5e19ad4`, settled with accept:
four model calls, ten HACP frames, both source artifacts measured, and 61 tests
passing on each device. The fixture executes 432 deterministic cases: 72 base
worlds and 360 permutation/duplicate/key-order variants. Both devices received
identical artifacts; settled replay left all six execution-log digests unchanged.

Supervisor conversation `ses_f8254d309ffeZAdepB0CEj3eM5` continued through
authoring/verification; worker conversation `6665c2de-69d0-4384-ba98-1086fa6088f5`
continued through review/implementation on the Air. A separate direct AGY
preflight also recalled a random marker across an explicit conversation resume.
These are private CLI handles, not protocol identities.

The settled run required one inspected operator continuation: OpenCode performed
an unnecessary cleanup of its temporary verification directory despite the
task's prohibition. Tier 1 stopped the same verifier process after observing
the completed tool event. The replacement scratch contents matched the submitted
sources and frozen fixture; approval was recorded before SIGCONT. This is not
an unattended run or a claim that log supervision prevents a tool from executing.

### Rework exposed a conversation-directory integration bug

A separate controlled-fault run (`hive-r1-rework.fYjXi4`) deliberately seeded
the historical revocation bug into a copy of the reconciler. The real agents
were explicitly instructed to submit that baseline, observe failures, then
repair through normal HACP rework. Both devices found the same two failures;
OpenCode returned rework and AGY repaired them in its existing conversation.
Both repaired executions passed all 61 tests. The defect was deliberately
injected, not a naturally occurring worker mistake.

However, the final OpenCode client hung with an empty captured log even though
the model had completed and written its verdict. Its session record retained
the original supervisor directory while the client had moved into `attempt-2`.
HIVE correctly withheld settlement while that invocation remained unconfirmed.
New OpenCode invocations now retain the role's original conversation directory;
per-attempt outputs still use separate paths. Existing invocation specifications
retain their recorded directories on replay rather than silently migrating.
The earlier settled run still replays with the corrected code.

At this checkpoint, a fresh controlled rework run (`hive-r1-rework.mXM2Iw`) is
testing that fix. No completion is claimed for the stalled older invocation.

## Final acceptance audit — complete 2026-09-07

The fresh native controlled-rework run `hive-r1-rework.mXM2Iw` completed from
one `hive collab run` invocation with no operator continuation and no external
agent coordinator. OpenCode supervised on the mini; AGY worked on the Air.
Session `s-0f7b3fd7e70e`, contract `c-0f7b3fd77054`:

```text
SETTLED — verdict accept
agent invocations: 6
HACP frames/outbox/receiver receipts: 12 / 12 / 12
initial frozen suite: 61 tests, 2 failures, exit 1 on each device
HACP response: verification.delivered -> rework
repaired frozen suite: 61 tests, 0 failures, exit 0 on each device
HACP response: verification.delivered -> accept; session.close
operator-continue audit rows: 0
settled replay: all ten execution-log SHA-256 values unchanged
```

The baseline defect was deliberately seeded for this diagnostic; the independent
61-test suite was not changed. The separate non-injected hard run described above
passed on its first submission. Together these prove actual implementation,
independent execution and live-agent repair without presenting an injected defect
as an accidental model mistake.

The supervisor retained `ses_f8247a882ffeKCrIO40VVzQLXo` across authoring and both
verifications; the worker retained `30273f54-6469-4536-bfa3-c61bf40bfa26` across
review, baseline submission and repair. Final verification ran in the original
supervisor conversation directory while writing its verdict under `attempt-2`.
Both submissions bind to the same frozen revision
`a7c6c8747bc83849fb750966060ba88e86f8bec0ad5bfdc7d297736a4b94877b`, with four
distinct artifact IDs across the two attempts. Final hashes matched on both Macs:

```text
reconciler.py dc9527475851110740744a698c19c532221314e12a8e13f90e469979e353c40c
canonical.py  a46e8f1b761136efffb0152aa0b6cfc649c6c0311d53f4b1dfe83126b033f9c3
```

| Required gate | Evidence supporting completion |
|---|---|
| 1. Durable state, inspect/resume, no blind reruns | Separate-process SIGKILL matrix: 50 recovered boundaries, 10 ambiguous intents refused; live detached-task recovery at all ten invocation stages; unchanged accepted messages and replay logs; real SSH channel interruption/recovery. |
| 2. Native per-role remote hosting | Actual mini/Air runs above, exact model selection, authenticated AGY calls, checked cross-device hashes, 63 KB prompts on both hosts, audited pause/continuation and actual interrupted SSH observation. |
| 3. Useful coding and collaboration | Two source modules; 61 independently authored tests with 432 deterministic cases; clarification/counteroffer and bounded-budget regressions; native failed checks -> peer feedback -> repair -> both-device acceptance, with stable conversations and preserved attempts. |
| 4. Shipping verification and standalone protocol | Warning-free dev/test builds, 566 workspace tests passing, independent interoperability, explicit live/fault tests, independently verified HACP package and 146 package tests. Evidence and unrelated worktree edits retained. |

Final workspace checks and additional live regressions:

```text
cargo build --workspace --offline                         -> zero warnings
cargo test --workspace --offline --no-run                 -> zero warnings
cargo test --workspace --offline --quiet                  -> 566 passed, 0 failed, 17 ignored
interop/run-interop.sh                                   -> independent peer passed
HIVE_TEST_SSH_HOST=mac-air cargo test -p hive-core --offline live_ssh_and_local_accept -- --ignored --nocapture
  -> passed: 63,000-byte argument delivered intact on both Macs
HIVE_TEST_SSH_HOST=mac-air cargo test -p hive-core --offline live_ssh_channel_disconnect -- --ignored --nocapture
  -> passed: established owned SSH observation client terminated, remote task
     recovered through a new host, one start, original exit 7 and artifact retained
cargo test -p hive-core --offline live_approved_cursor -- --ignored --nocapture
  -> passed with the file-backed local launch path
cargo test -p hive-core --offline live_exit_code_survives -- --ignored --nocapture
  -> passed with the file-backed local launch path
git diff --check                                         -> exit 0
```

The SSH disconnect test signals only its own established SSH observation client,
not the remote task or a process group; unrelated connections and network settings
are untouched. The 17 default-suite skips include explicit live tests and a child
helper; the commands above and earlier sections identify the tests run separately.
No ignored test is represented as passing merely because the default suite is green.

### Retained diagnostic state and scope boundary

The old verifier from `hive-r1-rework.fYjXi4` reached its original timeout and
is deliberately suspended as `hive-a18c668f-04-verify-a2` on the mini. Its paused
receipt and original directory remain intact; the new code does not silently
migrate or relaunch it. It is an obsolete scratch diagnostic, not an outstanding
acceptance run or production workload. No running sessions remain from the final
successful run on the Air. The initial oversized-launch run also remains available
for inspection, with its unresolved launch intent preserved.

Production services/databases, SSH trust and installed software were not changed
during the September 7 acceptance work. No commit, tag, push or registry release
was made. Autonomous scheduling, peer discovery, recursive teams, broader CLI
conversation adapters and full process sandboxing remain outside Release 1.

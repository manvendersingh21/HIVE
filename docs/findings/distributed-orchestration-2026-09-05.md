# Distributed orchestration observation: Mac mini → MacBook Air

Date: 2026-09-05. Observation only; no application code, credentials, SSH trust,
device configuration, or running services were changed during this audit.

Subsequent authorized fixes are recorded in
[Collaboration fixes](collaboration-fixes-2026-09-05.md): D4–D7 and the reproduced
D8/D9 bugs are addressed; the missing distributed features remain open.

## Outcome

**The devices can communicate, but the requested autonomous distributed Hive
workflow does not work end to end today.** Tailscale has a direct bidirectional
path, SSH from the mini to the Air works, and a manually transported HACP envelope
round-trip passed. Hive's v2 CLI cannot place a participant on another device.
Separately, the Air's Codex is unauthenticated and its tmux launcher is unavailable.

OpenCode ran successfully on the mini as an isolated decision-making probe. It
proposed the Air as the requested worker but reported that no worker model was
currently runnable. That was a real model response, **not a Hive scheduling or
remote-delegation event**. The direct Codex launch on the Air failed with HTTP 401
and produced no result. No completed two-AI-agent cross-device contract is claimed.

## Scope and methods

- Rebuilt the entire workspace, then ran its automated tests.
- Inspected the v2 runtime, legacy SSH worker path, planner/router, machine
  inventory, file edge, capability declarations, and reporting code.
- Probed only the requested mini and Air live. Other configured workers were not
  contacted by the audit's live probes. No Claude or AGY model was invoked.
- Invoked OpenCode on the mini for a bounded, no-tools placement decision.
- Invoked the Air's existing Codex installation in an ephemeral scratch session.
- Exercised the actual `SshWorker` and machine-probe APIs through a temporary
  external Rust harness; no replacement transport was added to Hive.
- Sent one library-validated HACP/2.0 heartbeat to a temporary Python echo on the
  Air and validated its correlated reply on the mini. Files traveled over SCP/SSH.
  This proves transportability, not native dispatch, discovery, or AI cooperation.
- Re-ran 17 independent acceptance tests from the earlier audit.

## What was observed

| Capability | Result | Evidence / limitation |
|---|---|---|
| Mini → Air direct peer connectivity | PASS | Tailscale ping reported a direct endpoint, about 20 ms. |
| Air → mini direct peer connectivity | PASS | Reverse Tailscale ping reported a direct endpoint, about 21 ms. |
| Tailnet device discovery | PASS at network layer | Air's Tailscale status listed the mini online/active; MagicDNS enabled. This is Tailscale discovery, not Hive agent discovery. |
| Mini → Air SSH | PASS | Both system SSH and `SshWorker::run("hostname")` returned the Air's hostname. |
| Air → mini SSH | BLOCKED at trust setup | TCP port 22 reachable, but strict checking reported no known ED25519 host key for the mini's MagicDNS name. Authentication was not reached. No key was enrolled. |
| HACP message crossing devices | PASS, diagnostic only | Library-minted heartbeat → Air Python echo → library validation of session, peers, reply ID and challenge. No contract or model execution involved. |
| OpenCode on mini | PASS | Version 1.18.29; completed a placement-response call using its existing `zai-coding-plan/glm-5.3` selection. |
| Codex installed on Air | PASS with environment caveat | Version 0.153.2. Found in a login shell, not bare non-login SSH PATH. |
| Codex executes the worker task on Air | FAIL: environment/authentication | `codex login status`: `Not logged in`; live `codex exec`: WebSocket/HTTPS 401, exit 1, no artifact. |
| Hive supervised SSH task launch on Air | FAIL: environment | Actual `SshWorker::spawn_tmux("…", "true", scratch_log)` returned “remote command could not be executed”; remote shell reported `tmux: command not found`. |
| Hive v2 remote participant placement | NOT IMPLEMENTED in CLI path | `--worker-host` rejected; `codex@mac-air` rejected as an unknown CLI. `LocalSessionHost` is hardcoded. |
| Master chooses participant models through Hive | NOT IMPLEMENTED in v2 run API | `--model` rejected; `RunConfig` has no per-role model fields. CLI defaults are used. |
| Peer agent/service discovery in Hive | NOT IMPLEMENTED in inspected paths | Static `workers.toml` plus probes of known SSH targets; no HACP peer advertisement/discovery/registry exchange. |
| Worker HTTP daemon on Air | NOT LISTENING at default endpoint | Loopback port 9091 refused `/health`. The daemon is optional for the existing SSH path, so this is not itself the SSH blocker. |

### Actual OpenCode decision

The mini's OpenCode was given the measured inventory, the requested placement,
and a tiny task: produce `result.txt` containing `peer-ready` plus a newline.
It was explicitly told not to run tools, repair configuration, install software,
or claim that its response meant Hive had dispatched work.

Its response proposed the Air and Codex but set `runnable_now` to `false`.
For `selected_model`, it said none was currently runnable; `gpt-5.6-terra` with
medium reasoning was configured on the Air but had no authenticated access.
It listed Codex authentication, missing tmux, and the absent Hive remote dispatch
path as blockers. Exporting this audit session identified the master's actual
provider/model as `zai-coding-plan/glm-5.3`.

The Air launch was a separate direct-SSH diagnostic of the configured model. It
failed before producing the requested file. No credentials were copied from the
mini, no login was performed, and no alternative model was silently substituted.

## Product gaps and reproducible failures

### D1 — The HACP v2 run path and the distributed worker path are separate

`hive-cli/src/collab.rs:58` constructs `LocalSessionHost` unconditionally.
`hive-core/src/runtime/lifecycle.rs:44` configures supervisor/worker CLI names,
task, directory and timeout—not device endpoints. Both role invocations receive
that same local host. The file edge assumes locally available inbox directories.

Meanwhile, `hive-core/src/workers/mod.rs` can launch shell assignments over SSH,
but those are `hive_common::TaskAssignment` commands, not the bilateral v2
session/contract exchange. Having both subsystems does not connect them.

Measured CLI refusals:

```text
--worker-host mac-air  → unexpected argument '--worker-host'
--worker codex@mac-air → unknown agent CLI 'codex@mac-air'
--model gpt-5.6-terra  → unexpected argument '--model'
```

### D2 — OpenCode is not wired as Hive's fleet/model scheduler

In the v2 run, the operator chooses both CLI roles up front. The supervisor
authors terms and verifies an artifact; it is not given a machine/model
catalog or a placement-result API that dispatches another role.

The separate `MasterAgent` does have machine-graph capability selection
(`hive-core/src/agent/mod.rs:609`), including explicit-worker handling in the
plan/approve/execute path. It should not be described as having no placement
logic. However, its `LlmRouter` uses the configured Ollama/cloud-provider paths,
not an OpenCode-session scheduler. Neither that placement logic nor the worker
pool is connected to `hive collab run`'s participant hosting.

### D3 — Network discovery is not agent discovery

Tailscale provides node presence, MagicDNS and connectivity; these worked.
Hive's known-worker inventory comes from `config/workers.toml`. Machine probes
can refresh facts for those names, but they do not discover new agents, negotiate
their model availability, authenticate their accounts, or advertise HACP endpoints.
No such discovery implementation was found in the inspected runtime/worker paths.

### D4 — Health says “Online” when the worker cannot host a task

The actual worker pool probe marked the Air `Online` and `select_worker()`
selected it. In the same audit, actual tmux launch failed.

```text
Hive SshWorker::ping = Ok(())
Hive SshWorker::run(hostname) = Ok(<Air hostname>)
Hive SshWorker::spawn_tmux(true) = Err(remote command could not be executed)
Air pool health = Online; selected = Some("mac-air")
```

`refresh_health` (`workers/mod.rs:132`) measures SSH reachability, not execution
readiness, tmux availability, CLI login state or usable model availability.
`select_worker` (`:109`) filters that status and the active-task count. This
explains the discrepancy; it is not a network outage.

### D5 — OpenCode is absent from the machine capability inventory

The mini ran OpenCode successfully, but `PROBED_TOOLS` in
`hive-core/src/memory/machines.rs:69` does not include `opencode` or `agy`.
The inferred `agentic-cli` capability checks only Claude/Codex.

Live library probes identified both Macs and found Codex on the Air, but did not
report the mini's working OpenCode. Thus that inventory cannot faithfully drive
the user's requested OpenCode-aware scheduling. Its login-shell probing sees
tools that a bare SSH command may not see; those environments are not equivalent.

### D6 — Apple Silicon available-memory calculation uses the wrong page size

Both devices' `vm_stat` headers reported **16384-byte pages**. The machine probe
at `memory/machines.rs:145` multiplies free+inactive page counts by **4096**.
Consequently that component of the memory estimate is four times too small.
The live inventory reported approximately 1.54 GiB available for the 16 GiB M4
mini and 0.49 GiB for the 8 GiB M2 Air. These are snapshots, not stable capacity
values; the formula mismatch is the reproducible finding. It matters because
placement ranking consumes available-memory estimates.

### D7 — Least-loaded bookkeeping still accumulates completed assignments

`workers/mod.rs:215` increments `active_tasks`; the inspected implementation has
no corresponding decrement. The basic least-loaded policy therefore drifts with
historical assignments. Source-confirmed; not stress-tested against the Air
because its task launcher is unavailable.

### D8 — File-edge receipts are not idempotent

Independent test `v2_file_edge_receipt_is_idempotent_on_redelivery` failed:
delivering the same envelope twice records **2** events, expected **1**.
`runtime/edge.rs:106` appends every receipt. The newer session-aware receive method
checks the session and author, then calls this same method; it does not add replay
deduplication. This matters on at-least-once network transports.

The file reader also reads the lexically latest file rather than implementing a
durable acknowledged message queue. The built-in synchronous local choreography
does not establish loss/reordering/restart tolerance for a distributed deployment.

### D9 — Runtime acceptance can still misrepresent task-specific checks

Two independent hermetic tests failed against the current Hive runtime:

1. `runtime_required_word_check_checks_the_required_word`: a file containing
   `unfinished` but not `ready` settled under a check claiming the required word.
   `runtime/attest.rs:99` treats names containing `word`/`content` as nonempty-file
   checks, not checks of the named content.
2. `runtime_rechecks_the_frozen_one_line_requirement_even_if_not_claimed`: a
   two-line artifact settled under a frozen one-line contract when the verifier
   only claimed file existence. Corroboration loops over the claimed checks
   (`:91`); it does not independently enumerate all frozen acceptance criteria.

The protocol library now refuses explicitly failed checks and binds verification
records to revisions, but it cannot measure task-specific artifact correctness
for the runtime. These remaining failures are in that runtime boundary.

### D10 — Advanced protocol features are not distributed runtime workflows yet

Core tests cover negotiation, amendments, grant chains, permits, observers and
escalations. That is not proof the two-device adapter executes those workflows.
The current v2 drive path uses fixed declarations and an accept/decline review;
it does not orchestrate counteroffer/amendment loops, recursive spawn/delegation,
cross-branch permit admission, or escalation between hosted remote agents.
Rework is explicitly reported unfinished with a snapshot, not automatically
resumed. No general distributed heartbeat/recovery or failover loop was found
in this run path. These capabilities were **not live-certified** in this audit.

## Tests and limits of the claims

```text
cargo build --workspace --offline
  PASS, zero warnings

cargo test --workspace --offline --quiet
  531 passed, 0 failed, 5 ignored

Independent acceptance suite (outside the repository)
  14 passed, 3 failed
  failures: receipt idempotency; required-word measurement;
            omitted frozen one-line requirement

Live OpenCode decision on mini
  PASS, one completed decision response

Live Codex execution on Air
  FAIL, HTTP 401, exit 1, no result.txt

HACP transport-only round-trip via SCP/SSH
  PASS, validated by standalone hacp::v2 after Air Python echo
```

The workspace tests are valuable but do not cover the three independently
reproduced failures above. A passing unit suite is not a certification of all
distributed features. Remote AI execution, automatic device/model scheduling,
recursive delegation, recovery after disconnect, and a full cross-device
negotiate→freeze→execute→verify→settle flow remain unproven or blocked here.

## Preserved state and artifacts

Application source and pre-existing local changes were preserved. The SHA-256
of the tracked working-tree diff was identical before and after the probes:

```text
8d0f94f1a2147ebad6d76ccebfa365134f743e08045ad2919c7177966ba805cd
```

This audit report is the only new repository file. Temporary diagnostic files
are retained under `/private/tmp/hive-distributed-audit.97LvXH` on the mini and
`/private/tmp/hive-distributed-audit.zwdRIa` on the Air. The remote directory contains
the small echo fixture and challenge/reply files, not a running service. The
mini's completed OpenCode audit session remains in OpenCode's normal history.
Codex was invoked with `--ephemeral`. No new tmux sessions remain; the pre-existing
`hivew2` session was left untouched. No production Hive database was used by the
live probes, and no service restart, installation, login or SSH trust change was made.

One environment quirk was also observed: invoking Tailscale through the
`/usr/local/bin/tailscale` symlink crashed with a bundle-identifier assertion;
calling the installed application's executable directly worked. This was not
changed and did not prevent the network probes.

## Follow-up — AGY authentication resolved; model-backed exchange passes

After the user reported fixing the Air environment, `agy models` succeeded over
SSH. This supersedes the earlier AGY authentication failure; Codex authentication
was not rechecked and must not be considered resolved by this result.

On September 5 at approximately 23:54 UTC, a fresh OpenCode invocation on the
mini selected `mac-air`, `agy`, and `gemini-3.8-flash-low` for a small JSON
challenge-response task. The diagnostic then manually transported a HACP/2.0
heartbeat envelope to an isolated temporary directory on the Air over SCP.
AGY was invoked there with the selected Gemini model and instructed to read
the request file and return a correlated HACP reply. The prompt did not contain
the request ID or challenge value.

AGY returned `SUCCESS` with the exact request ID and challenge read from that
file. Its returned envelope, without repair, passed the standalone HACP
library's envelope validation. Additional assertions verified reversed peers,
matching session and protocol, a distinct reply ID, exact `in_reply_to`,
heartbeat kind, matching challenge, and the requested acknowledgement.
Local and remote request SHA-256 checksums matched.

```text
OpenCode worker/model decision on mini: PASS
AGY/Gemini execution on Air over SSH: PASS
Actual model reply HACP validation and correlation: PASS
Request: m-37750b878598419696a4bbb3152bb383
Reply:   m-c4b91f0a2d8e47b192e5a610f3c87e5b
```

This establishes a real cross-device, model-backed HACP message exchange,
unlike the earlier Python echo fixture. It does not establish native Hive
device/model scheduling, automatic peer discovery, or a full distributed
contract lifecycle. OpenCode supplied the placement decision; this diagnostic
performed the SSH invocation and transport manually.

AGY was passed `--sandbox`; it warned that `--mode plan` has no effect with
slash-command expansion disabled, so plan mode is not claimed as enforced.
No application changes or credential changes were made by this follow-up,
and no Claude model was invoked. The tracked diff checksum remained unchanged.
Evidence is retained under `/private/tmp/hacp-agy-live.9EvmrI` on the mini
(`master.jsonl`, `challenge.json`, `agy-output.txt`, `agy-result.json`, and
`reply.json`) and `/private/tmp/hacp-agy-live.6ogBIB` on the Air (request file).

The subsequent [two-device dynamic collaboration test](hacp-two-device-collaboration-2026-09-05.md)
completed a real design/test/review/revision exchange, with 43 final tests passing
on each device. That report records both the successful peer feedback loop and
the coordinator intervention still required; it does not resolve the native
distributed-runtime gaps above.

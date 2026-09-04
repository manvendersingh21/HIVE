# Hive — Current Status

**Updated 2026-09-02, verified against a live build.** This audit reflects what was actually
compiled, tested, and run — not what a session claimed.

---

## Phase 3: ✅ Core delegation + safety supervision complete and live-verified

This landed as a redesign, not the original plan's pseudocode: instead of a deployed
`hive-worker` HTTP daemon receiving `POST /task`, the master drives `tmux` **directly over
SSH**. A worker only needs `tmux` and an authorized SSH key — no `hive-worker` binary to
build/deploy/keep updated. `hive-worker`'s HTTP routes still exist (Phase 1) but are now
bypassed for this delegation path; see "Superseded" below.

```
$ cargo build --workspace          # clean, zero warnings
$ cargo test --workspace           # 28 passed, 1 ignored (see below), 0 failed
$ cargo run --bin hive -- workers list
NAME              HOST           USER       TAGS
lawfinder-worker  hive-worker-1  azureuser  []

$ cargo run --bin hive -- task -d "Delegate to a remote worker machine: run the shell
  command 'hostname && uptime && echo delegated-test-done' on the remote worker, not locally"
...
Session 'hive-06c431eb-...' on worker 'lawfinder-worker': running

# Verified on the worker directly afterward:
$ ssh hive-worker-1 cat /tmp/hive-06c431eb-....log
lawfinder
 00:44:48 up 1 day, 20:52,  3 users,  load average: 1.00, 1.00, 1.04
delegated-test-done
__HIVE_DONE__0
```

### What Phase 3 built

| Component | Detail |
|:---|:---|
| `hive-core/src/workers/ssh.rs` | `SshWorker` — real SSH via `openssh` (`process-mux`: a real `ControlMaster` per connection, `server_alive_interval`, `connect_timeout`). `spawn_tmux` starts a detached tmux session running the command, piping combined stdout/stderr through both the live pane *and* a remote log file (`> >(tee path) 2>&1`, not a plain pipe, so `$?` after the command is preserved for the completion sentinel). `tail` streams that log file line-by-line over its own SSH channel on the same pooled connection — chosen over polling `tmux capture-pane` specifically because polling can miss short bursts of output between checks. `send_keys`/`capture_pane` round out the toolkit. |
| `hive-core/src/watchdog/rules.rs` | 9 built-in Tier-1 regex rules (`rm -rf /`, disk format/overwrite, `DROP TABLE`/`TRUNCATE`, force-push/hard-reset, `sudo`/`chmod 777`, `curl \| sh`, private-key/API-key patterns, fork bombs), each mapped to the `Severity`/`SafetyCategory` types Phase 1 already defined — those types turned out to already match this design closely. |
| `hive-core/src/watchdog/mod.rs` | `Watchdog::scan_line` — Tier 1, run on every streamed line, zero LLM cost. `Watchdog::review` — Tier 2, periodic (`WatchdogConfig::poll_interval_secs`, backing off to `reduced_poll_interval_secs` after `max_consecutive_safe` clean checks — both config fields Phase 1 had already added), asks **the local model** (never a routed one — see "Who reviews whom" below) "does this still look like it's working toward the objective?" over the JSON-extraction pattern proven in the Phase 2 planner. Soft-fails to "inconclusive, safe" on any LLM/parse error — Tier 1 remains the hard stop so a flaky LLM response can't spuriously pause a session. `extra_rules` from `hive.toml` are compiled in alongside the built-ins. |
| `WorkerPool::delegate` | SSHes in, starts the tmux session, registers a `SessionInfo` (shared `Arc<Mutex<HashMap>>`, safe to read from `active_sessions()` concurrently), and spawns a background supervisor task. Returns as soon as the session is confirmed *started* — does not block on the remote command finishing. |
| `supervise` (in `workers/mod.rs`) | The background task: `tokio::select!` between the next tailed line (Tier 1 + sentinel detection) and a poll-interval tick (Tier 2). On a Tier-1 or Tier-2 hit: sends `tmux send-keys C-c` (pause, not kill — preserves state for review), marks the session `TaskState::PausedByWatchdog`, and logs a handover notification with the exact `ssh ... -t 'tmux attach -t ...'` command to take over. On the `__HIVE_DONE__<code>` sentinel: marks `Completed`/`Failed` by exit code. |
| `WorkerPool::refresh_health` | Real SSH reachability probe replacing "every worker boots `Offline`" — `hive-cli`'s `build_agent` now calls it before every `hive task`/`hive chat`. |
| `hive-cli` wiring | **Fixed a live Phase 1 bug in passing**: `build_agent` called `WorkerPool::new(vec![])` unconditionally — `config/workers.toml` was parsed elsewhere (`hive workers list`) but never actually fed into the pool the agent used, so delegation was structurally impossible even once the router supported `requires_remote`. Now loads `WorkersConfig::from_project_root` for real. |
| `agent/mod.rs` | The `requires_remote` branch now calls `workers.delegate(...)` for real (previously just logged a note) and reports delegated sessions back in `AgentResponse.sessions`. |
| `config/workers.toml` | One real worker configured — `host` points at an SSH config alias (`hive-worker-1`), not a raw IP, specifically so the actual hostname/IP never lands in this **public** GitHub repo. The alias (with `IdentityFile`, `ServerAliveInterval`/`CountMax`, `ControlMaster`/`ControlPersist`) lives only in the local, untracked `~/.ssh/config`. |

### Live-verified, including a real pause

Beyond the `hostname`/`uptime` run above, a `#[tokio::test]` marked `#[ignore]` (depends on a
private host not available in CI or on a fresh checkout — run explicitly with
`cargo test -p hive-core --lib -- --ignored live_delegation_pauses_on_tier1_match`) delegates a
command whose output contains `rm -rf /` (an `echo`, never actually run) to the real worker and
asserts the session reaches `TaskState::PausedByWatchdog` within 10s. **This passed live** in
this session: the Tier-1 scan caught the line from real remote output, and the session was
correctly marked paused. (The pause *signal* itself — `tmux send-keys C-c` — logged a "can't find pane" warning, which
this audit originally recorded as a benign race: the `echo` finishing before the signal arrived.
**That reading was wrong.** `send_keys` passed a bare session name where tmux expects a
target-pane, so the call failed every time and the watchdog could detect a dangerous session but
never actually pause one. Found and fixed while closing the supervision caveat — see
`workers::ssh::pane_target`.)

Tier 2 (LLM review) is unit-tested (JSON extraction, `Watchdog::review`'s fallback path) and
built entirely on the `LlmRouter::local_complete` path already live-verified in Phase 2, but
was not separately exercised live end-to-end this session (Tier-1-only config was used for the
live worker test, to isolate it from needing Ollama running at the same time).

### Known limitation — the supervision window is only as long as the process stays alive

`WorkerPool::delegate` spawns the supervisor as a `tokio::spawn` task inside the current
process. The remote command runs independently on the worker regardless (it's a detached tmux
session — verified above, the `hostname`/`uptime` test's log has full output and the completion
sentinel even though the CLI exited almost immediately after printing the summary). But
**watching** it — Tier-1/Tier-2 checks, the pause — only happens while something is still
running that spawned task:

- `hive task` is one-shot: the process exits right after printing, so anything delegated during
  that call is supervised for a few hundred milliseconds at most before the task is aborted.
- `hive chat` stays alive for the whole interactive session, so supervision continues to work
  for anything delegated during that session — until you exit chat.
- Nothing yet keeps the master running continuously across CLI invocations. `hive serve` today
  just tells you to run `hive-web` directly (Phase 1 stub, unchanged). A persistent master
  daemon is what would give every delegated session supervision for its full lifetime — that's
  not built.

If you use `hive task` to delegate something long-running today, treat it as fire-and-forget
with **no** safety net once the CLI process exits, not as "the watchdog has it."

### Also not yet done (by design — later phases, not a Phase 3 gap)

- Remote agentic-CLI delegation (routing `AiProvider::Claude`/`Codex` to a supervised `claude`/
  `codex` session instead of a plain shell command) — `claude`/`codex` are installed locally on
  this machine but **not** on the configured worker, so this isn't demonstrated yet. The same
  `SshWorker`/`supervise` machinery built here should cover it once a worker has those CLIs
  installed and authenticated; a **local** (no-SSH, local tmux) supervised session for the
  already-authenticated local `claude`/`codex` is the more natural next slice.
- Full Phase 10 watchdog: no `Incident`/`IncidentReviewState` persistence, no `HumanDecision`
  (resume/abort/modify-and-resume) handling, no ntfy.sh/webhook delivery — today's notification
  is a `tracing::warn!` log line. `Severity`/`SafetyCategory`/`SafetyAnalysis`/`Incident` are
  all still the Phase 1 types; only `SafetyAnalysis` is actually produced and used yet.
- `WorkerPool::active_sessions()` isn't surfaced anywhere in the CLI yet (`hive sessions` still
  prints its Phase 1 placeholder) or in `hive-web` (`/api/sessions` still returns `"[]"`).

### Superseded from the original plan

- `hive-worker`'s HTTP daemon (`POST /task`, tmux-via-daemon) — Phase 1 built its routes,
  Phase 3/4 in the original plan meant to use them. This design instead drives tmux directly
  over SSH from the master, no daemon required. The routes aren't removed (harmless, still
  respond over HTTP per Phase 1's own verification), just unused by this delegation path.

---

## Phase 2: ✅ Complete and verified

```
$ cargo build --workspace
   Finished `dev` profile [unoptimized + debuginfo] target(s)   # clean, zero warnings

$ cargo test --workspace
running 18 tests   (hive-common)
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
running 3 tests    (hive-core — planner JSON extraction)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo run --bin hive -- task -d "check disk space"    # before Ollama was installed
Error: Failed to reach Ollama at http://localhost:11434/api/chat: error sending request
for url (http://localhost:11434/api/chat) (is `ollama serve` running?)

$ brew install ollama tmux && brew services start ollama
$ ollama pull qwen2.5:14b-instruct-q4_K_M   # 9.0 GB

$ cargo run --bin hive -- task -d "check disk space on this machine"
Summary: Check the disk space on the local machine

$ df -h
exit_code: 0
stdout:
Filesystem        Size    Used   Avail Capacity ...
/dev/disk3s1s1   228Gi    16Gi    79Gi    17%    ...
...

Provider: local (Ollama)
Complexity: SIMPLE
No sessions created (worker delegation not implemented yet).

$ HIVE_WORKER_ADDR=127.0.0.1:19091 cargo run --bin hive-worker &
$ curl http://127.0.0.1:19091/health
ok

$ HIVE_WEB_ADDR=127.0.0.1:19093 cargo run --bin hive-web &
$ curl http://127.0.0.1:19093/api/health
ok
```

Both paths are now verified live: before Ollama was installed, the router failed honestly
(no fake `Medium` placeholder); after installing Ollama + pulling the model, a real request
was classified `SIMPLE`, planned into a concrete `df -h` command by the local model, executed
through `ShellTool`, and returned with real output in the summary.

### What Phase 2 built

| Component | Detail |
|:---|:---|
| `hive-core/src/llm/local.rs` | `OllamaClient` — `/api/chat` and `/api/embeddings` against a local Ollama server |
| `hive-core/src/llm/gemini.rs`, `claude.rs`, `openai.rs` | Cloud clients for Gemini, Claude, and OpenAI (routed to for `CodeHeavy`/"Codex" tasks). All send API keys via request headers (`x-goog-api-key`, `x-api-key`, `bearer_auth`) — never in a URL — so a key can't leak through an error message or log line that echoes the request URL. |
| `LlmRouter::from_config` | Builds all four clients from `HiveConfig::llm`; a cloud provider with no resolvable API key is logged and left unconfigured rather than failing construction |
| `LlmRouter::classify_complexity` | Real classification prompt against the local model, parsed with the existing `Complexity::from_llm_output` |
| `LlmRouter::route_and_execute` | Dispatches by `Complexity::recommended_provider()`; if the recommended provider isn't configured or the call fails, falls back to the local model with a `tracing::warn!` rather than failing the whole request |
| `hive-core/src/agent/planner.rs` | `Planner::plan` asks the routed LLM for a JSON `TaskPlan`/`SubTask` list; `extract_plan` tolerates prose/markdown-fenced responses; falls back to a single no-command subtask (nothing assumed safe to run) if the response isn't parseable — covered by 3 unit tests |
| `hive-core/src/tools/` | `Tool` trait (`async-trait`, schema via `schemars::schema_for!`) plus `ShellTool` (`sh -c`, timeout, captures stdout/stderr/exit code), `FileOpsTool` (read/write/append), `GitTool` (arbitrary git subcommand) |
| `MasterAgent::handle_request` | Now: loads memory context (still stubbed) → classifies → plans → runs each local subtask's commands through `ToolRegistry` → notes remote subtasks against the (still-offline) worker pool → returns a summary that includes real command output |
| `hive-cli/src/main.rs` | `build_agent` now calls `LlmRouter::from_config(&config.llm)` instead of hand-wiring just the local URL/model |

### Known limitation — no safety gate yet (by design, not an oversight)

Local subtasks now run real shell commands via `ShellTool`, with **no confirmation prompt and
no safety watchdog** — the watchdog is Phase 10. This matches the project's own phased design
(Phase 1's stub-execution model was always meant to be replaced with real execution before the
watchdog exists to guard it), but it's worth being explicit: today, whatever the LLM's planning
prompt returns as `commands` will execute unattended. Treat `hive task`/`hive chat` as you would
any other unattended automation until Phase 10 lands. The planner's fallback path (no parseable
JSON → zero commands) is deliberately conservative for exactly this reason.

### What's still a stub (Phase 3+, not a Phase 2 gap)

- `requires_remote` subtasks are only noted (`"would delegate to worker '...'"`) — no SSH, no
  tmux session, no actual delegation (`WorkerPool::delegate` doesn't exist yet)
- `WorkerPool` still boots every worker `Offline`; no health checks flip it online
- `hive-worker`'s `/task` route is unchanged from Phase 1 — still claims `Running` without
  executing anything
- `hive-web`, `memory`, `skills`, `finetune`, `watchdog` are unchanged from Phase 1

See [`ROADMAP.md`](ROADMAP.md) for Phases 3–10.

---

## Phase 1: ✅ Complete and verified

Previously this document reported Phase 1 as ~70% done and non-compiling — the workspace had
two build-breaking gaps and had never once been built. Both are now fixed, and every claim
below was checked by actually running the command.

```
$ cargo build --workspace
   Finished `dev` profile [unoptimized + debuginfo] target(s)   # clean, zero warnings

$ cargo test --workspace
running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo run --bin hive -- task -d "check disk space"
Summary: Task received: check disk space
Provider: Gemini Flash
Complexity: MEDIUM
No sessions created (worker delegation not implemented yet).

$ HIVE_WORKER_ADDR=127.0.0.1:19091 cargo run --bin hive-worker &
$ curl http://127.0.0.1:19091/health
ok

$ HIVE_WEB_ADDR=127.0.0.1:19093 cargo run --bin hive-web &
$ curl http://127.0.0.1:19093/api/health
ok
```

### What changed to get here

| Fix | Detail |
|:---|:---|
| Rust toolchain | Installed via rustup — `rustc 1.98.0`, `cargo 1.98.0`. None was present before. |
| `hive-common` deps | Added `rusqlite` and `reqwest` (used by `error.rs`'s `From` impls but never declared), `tracing` (used by `protocol.rs`'s `Complexity::from_llm_output`, also never declared), and the `schemars` `"chrono"` feature (needed for `DateTime<Utc>` to derive `JsonSchema` — three structs in `protocol.rs` use it). |
| `hive-cli/src/main.rs` | Was completely absent. Wrote the full `clap` command tree: `chat`, `task`, `sessions`, `attach`, `workers list`, `skills list`, `finetune export`, `serve`. `chat` and `task` build a real `MasterAgent` from `hive-core` and call `handle_request` — they're not print stubs, they exercise the actual (still-stubbed) agent loop. Commands for unbuilt subsystems (sessions, attach, skills, finetune, serve) print an honest "not implemented, see Phase N" message rather than fake output. |
| `config/hive.toml`, `config/workers.toml` | Written from the plan's templates. `workers.toml` ships with an empty `workers = []` list and inline comments rather than fake placeholder hosts — `hive workers list` reports "No workers configured" truthfully until real machines are added. |
| Unused-import warnings | `hive-core/src/agent/mod.rs`, `llm/mod.rs`, `workers/mod.rs` each had imports for types not yet used by their stub logic. Trimmed to a warning-free build; Phase 2/3 will re-add them as the real logic lands. |
| Git | Repo initialized, working baseline committed. `graphify-out/`'s machine-local sidecar files (`.graphify_python`, `.graphify_root`, and the transient extraction JSONs) are gitignored; the durable outputs (`GRAPH_REPORT.md`, `graph.html`, `graph.json`, `manifest.json`) are tracked. |

### What's still a stub (by design — this is Phase 2+, not a Phase 1 gap)

- `LlmRouter::classify_complexity` always returns `Medium`; `local_complete` returns `""`
- `MasterAgent::handle_request` plans nothing and delegates nothing — it classifies, then stops
- `WorkerPool` has real selection logic but no SSH/delegation; every worker boots `Offline`
- `hive-worker`'s `/task` route accepts a `TaskAssignment` and immediately claims `Running`
  without creating a tmux session or executing anything
- `hive-web`'s `/api/sessions` returns a hardcoded `"[]"`; no WebSocket terminal exists;
  `static/` is empty
- `memory`, `skills`, `tools`, `watchdog`, `finetune` are still empty structs with `new()`

None of this blocks calling Phase 1 done — Phase 1's job was a workspace that builds, tests,
and runs, with types the rest of the system can build against. That's now true and verified.
See [`ROADMAP.md`](ROADMAP.md) for what each of those stubs turns into in Phases 2–10.

> **Superseded by Phase 2** (see below): `classify_complexity` and `local_complete` are real
> now, and `handle_request` plans and executes local subtasks. The rest of this list —
> delegation, `hive-worker` execution, `hive-web`, memory/skills/watchdog/finetune — is still
> accurate as of Phase 2.

---

## Full deliverable checklist

| Deliverable | State |
|:---|:---|
| Workspace `Cargo.toml`, 5 members, shared deps | ✅ |
| `hive-common` protocol types | ✅ |
| `hive-common` error types | ✅ |
| `hive-common` config types | ✅ |
| `hive-core` module skeleton | ✅ |
| `hive-worker` entry point | ✅ |
| `hive-web` entry point | ✅ |
| `hive-cli` entry point | ✅ |
| `hive-common` deps match its code | ✅ |
| `config/hive.toml` | ✅ |
| `config/workers.toml` | ✅ |
| `cargo build --workspace` passes | ✅ verified, zero warnings |
| `cargo test --workspace` passes | ✅ verified, 18/18 |
| All three binaries start and respond | ✅ verified over HTTP |
| Git repository with a committed baseline | ✅ |

**13/13. Phase 1 is done.**

---

## Phase 2 deliverable checklist

| Deliverable | State |
|:---|:---|
| `llm/local.rs` — real Ollama `/api/chat` + `/api/embeddings` client | ✅ |
| `llm/gemini.rs`, `llm/claude.rs`, `llm/openai.rs` — cloud clients, header-based auth | ✅ |
| `LlmRouter::from_config` — builds all four from `HiveConfig`, optional cloud providers | ✅ |
| `LlmRouter::classify_complexity` — real prompt, no more hardcoded `Medium` | ✅ |
| `LlmRouter::route_and_execute` — routes by complexity, falls back to local on failure | ✅ |
| `agent/planner.rs` — LLM-driven `TaskPlan`/`SubTask` decomposition, JSON extraction, safe fallback | ✅ |
| `tools/` — `Tool` trait + `ShellTool`, `FileOpsTool`, `GitTool`, schemas via `schemars` | ✅ |
| `MasterAgent::handle_request` — plans, executes local subtasks for real, notes remote ones | ✅ |
| `hive-cli` wired to `LlmRouter::from_config` | ✅ |
| `cargo build --workspace` passes | ✅ verified, zero warnings |
| `cargo test --workspace` passes | ✅ verified, 21/21 (18 + 3 new planner tests) |
| `hive-worker`/`hive-web` unaffected, still respond over HTTP | ✅ verified |

**12/12. Phase 2 is done.**

---

## Next: Phase 3 — Worker Pool & SSH Delegation

See [`ROADMAP.md`](ROADMAP.md#phase-3--worker-pool--ssh-delegation). Starting point is
`hive-core/src/workers/mod.rs`: add `workers/ssh.rs` (`openssh` sessions with ControlMaster
multiplexing), health checks that flip `WorkerStatus` off of `Offline`, and
`WorkerPool::delegate` to actually create a remote tmux session and POST a `TaskAssignment` to
the worker daemon. `MasterAgent::handle_request`'s remote-subtask branch (currently just a
`notes.push(...)` in `hive-core/src/agent/mod.rs`) is where the real call plugs in.


---

## Local model selection (measured, not assumed)

The master runs `qwen3.5:9b`. It was chosen by benchmarking candidates on the
three jobs Hive actually gives the local model — complexity classification, JSON
plan generation, and Tier-2 safety review — on the real hardware (Mac Mini M4,
16 GB).

| Model | Size | Plan latency | Gen | JSON valid |
|:---|---:|---:|---:|---:|
| qwen2.5:14b-q4_K_M (previous) | 9.0 GB | 14.9s | 11.4 tok/s | 3/3 |
| **qwen3.5:9b (current)** | **6.6 GB** | **6.8s** | **17.5 tok/s** | **3/3** |
| gemma4:12b-it-qat | 7.2 GB | 10.2s | 12.6 tok/s | 15/18 |
| qwen3.5:4b | 3.4 GB | 5.9s | 28.0 tok/s | 3/3, weaker commands |

Two findings mattered more than the ranking:

**Thinking must be disabled.** Qwen3.x emits reasoning tokens by default. With
them on, `qwen3.5:9b` scored **0/3** usable responses — the whole token budget
went to reasoning and the answer came back empty — and plan latency was 18.1s.
`OllamaClient` now always sends `think: false`; Ollama ignores it on models that
do not think.

**The prompt mattered more than the model.** The planner never said which OS its
commands would run on, so models guessed and emitted GNU-only flags
(`find -printf`, `ps --sort=`) that fail on macOS. Fixing that is worth more than
any model swap, and *how* it is fixed matters just as much — measured on
qwen3.5:9b, 12 plans per variant:

| Prompt | Broken commands |
|:---|---:|
| No OS information (the old behavior) | 3–4 / 12 |
| Terse constraint at the end | 9 / 12 |
| Constraint at the top of the prompt | 3 / 12 |
| Forbid GNU flags, at the end | 5 / 12 |
| **Forbid GNU flags *and show BSD equivalents*, at the end** | **0 / 12** |

Telling the model what not to write is not enough; it needs the replacement to
reach for. `FleetContext` in `agent/planner.rs` renders both halves from the
machine knowledge graph, so it stays correct as the fleet changes.

With that in place the two models are equivalent on command quality — 6/7 vs
5/6 working commands over the same live queries — so `qwen3.5:9b` wins on being
2.4 GB smaller and ~2x faster. On a 16 GB Mac (~11 GB GPU-wired ceiling) that
headroom is the scarce resource: the previous 9 GB model left the machine
swapping at 14% free.

**Bigger model, heavier quantization does not work here.** A 27B needs ~16 GB at
Q4_K_M and ~12 GB at IQ3 — both over the ceiling — and only fits around IQ2,
where degradation is 15–25% versus 3–5% at Q4_K_M. That lands worst on
structured output, and when JSON breaks `Planner::plan` falls back to a no-op
subtask. 16 GB is the binding constraint, not the model.


---

## The two Phase 3 caveats, closed

Both stemmed from `hive task` doing everything in-process and then exiting.

### 1. Delegated work is now supervised for its whole life

`WorkerPool::delegate` spawns its watchdog with `tokio::spawn` — on the *caller's*
runtime. `hive task` exited moments later, aborting that task and leaving the remote
tmux session running unwatched.

`hive task` and `hive chat` now submit to the running master (`hive-web`, under
launchd/systemd) whenever one is reachable, falling back to in-process with an
explicit warning when it is not. The supervisor then lives in a daemon, not a
short-lived CLI.

Proven live, not asserted:

```
CLI exited: 21:42:06
master log: 21:42:31  Session 'hive-20f2e4df…' finished with exit code 0
```

and, with a session engineered to trip Tier-1 twenty seconds after the CLI was gone:

```
CLI exited: 21:45:04
master log: 21:45:24  WATCHDOG INCIDENT [CRITICAL] … 'rm-rf-root-or-wide' matched: rm -rf /
```

### 2. The CLI has the same safety gate as the web UI

`hive task` ran whatever the planner produced. It now plans first, holds anything the
watchdog's Tier-1 rules flag, and asks. `--yes` approves flagged commands (for
non-interactive use), `--deny-flagged` refuses them, `--local` forces in-process.

**No tty means deny.** A piped or scripted invocation cannot answer, and defaulting to
"run it" would silently execute exactly the commands the watchdog objected to. Verified:
`echo "" | hive task -d "…rm -rf …"` skipped the command and the canary file survived;
`--yes` on the same request deleted it.

### Two real bugs surfaced by testing this

**The watchdog could never pause anything.** `SshWorker::send_keys` passed a bare session
name where tmux expects a target-pane, so every pause attempt failed with
`can't find pane`. This audit had previously recorded that warning as a benign race. It
was not. Fixed with `workers::ssh::pane_target` (`=name:`).

**`C-c` was a kill, not a pause.** With the target fixed, the interrupt landed — and ended
the session, because the shell `spawn_tmux` starts has only that one command to run. A
`sleep 300` child orphaned. That destroys the very state the operator is told to attach to.
Pausing now sends SIGSTOP to the pane's foreground process group, so the session stays
attachable and the process tree is intact. The remote snippet requires the pgid to be
non-empty, all digits, and `> 1` — passing `-1` to `kill` means *every process the user can
signal*, which is exactly how the Phase 4 incident happened.

**And SIGSTOP did not hold either — found 2026-09-04, while building the HACP session host.**
The two fixes above are both real, and the pause still did not work on the SSH path. Without
job control, `spawn_tmux`'s `bash -c '{ ( cmd ); … } | tee log'` runs the whole pipeline in
bash's own process group, which is also the pane's group — so `pause_session`, aiming at the
foreground pgid, was stopping tmux's own pane child. tmux reaps that child with `WUNTRACED`
(`server_child_stopped`) and answers a stop with SIGCONT. Measured on tmux 3.7c: `kill`
returned 0, `pause_session` returned `Suspended`, and every process was back in `S` a moment
later. A flagged session kept running while the operator was told it was frozen.

Fixed by adding `set -m` to the launched shell, which puts the pipeline in a group of its own
so tmux is never told anything stopped. Verified as a before/after on one machine — old line:
`pane_pid == tpgid`, states `Ss+ S+ S+ S+` after the stop; new line: `pane_pid 52436 != pgid
52438`, states `T+ T+ T+`. `pause_session` now also *verifies* the group reached state `T` and
returns an error if it did not, because the through-line of all three of these bugs is that
each fix was believed on the strength of a command exiting 0. `kill` exiting 0 means the
signal was delivered, not that it stuck.

Note what this cost: this section previously read as a closed story, and the entry above it
says the earlier warning "had previously recorded that warning as a benign race. It was not."
The same mistake was then made one layer down. The lesson that generalises is not about tmux —
it is that *a safety mechanism is unverified until something observes the state it is supposed
to produce*, and every one of these three fixes shipped without that observation.

### Also fixed: workers were permanently offline

Making `hive-web`'s startup non-blocking dropped its `refresh_health()` call, so every
worker sat at `Offline` forever and the master would never place remote work — it reported
"no worker is online" with the worker sitting there perfectly reachable. `refresh_health`
now takes `&self` (status is an `AtomicU8`, like `active_tasks`) and runs on a 60s timer
alongside the machine-graph refresh, bounded so one wedged host cannot stall the fleet.


---

## Azure retired

The `lawfinder` Azure VM is no longer part of the fleet. `workers.toml` lists only
`archlinux-worker` (`hive-worker-2`, Arch, 4 cores, 11.6 GB), and the master prunes
machines that are no longer configured, so the knowledge graph dropped
`lawfinder-worker` on the next refresh.

Historical results recorded in this document that were verified on that machine —
Phase 3's SSH delegation, Phase 4's worker daemon, Phase 5's web terminal — still
stand as records of what was tested and when. They are not claims about what is
running today. What runs today:

| | host | state |
|:---|:---|:---|
| Master (`hive-web`: agent UI, terminal, machine graph) | Mac Mini, launchd | live |
| Worker | `archlinux-worker` via SSH+tmux | live |
| `hive-worker` daemon | — | not deployed anywhere |

The `hive-worker` daemon has no host at present: the Arch box has no Rust toolchain,
so nothing has been built there. Direct SSH+tmux delegation (the default path) works
without it.


---

## Who reviews whom: the Tier-2 model asymmetry

Surfaced by a knowledge-graph trace of `LlmRouter`, which is the second-highest
betweenness node in the codebase. Four subsystems depend on it — the agent loop,
the planner, the supervisor, and the watchdog — and they do **not** all use it
the same way.

| Consumer | Method | Model that actually runs |
|:---|:---|:---|
| `Planner::plan` (`planner.rs:196`) | `route_and_execute` | routed by complexity; **falls back to local** if the provider is unconfigured or fails |
| `MasterAgent` (`agent/mod.rs:129`) | `classify_complexity` | routed |
| `Watchdog::review` (`watchdog/mod.rs:101`) | **`local_complete`** | **always local — never routed** |

So when a `CODE_HEAVY` task is planned by Codex or Claude, the safety review of
that plan is still performed by the local 9B. The weakest model in the fleet
judges the output of the strongest.

**Why it is this way.** Tier 2 fires every `poll_interval_secs` for the whole
life of every supervised session. Routing it to a cloud provider would bill
continuously and add seconds of latency to a loop meant to be cheap, and Tier 1
(regex) is the hard stop precisely because Tier 2 is the weaker, advisory signal.
That is a defensible trade — but it was never written down, and `local_complete`
at the call site looks indistinguishable from an oversight. It is now documented
in `Watchdog::review`'s doc comment.

**What it costs.** This is the likeliest source of Tier-2 false positives. On a
successful run, review saw `4` from `nproc` during a 15-second sleep and reported
the agent as "stuck". Harmless while verdicts are advisory; a watchdog that cries
wolf on healthy work trains you to ignore it.

**The second-order effect.** With no cloud API keys configured — the current
state — planning *also* degrades to local. Both sides of the safety check then
collapse onto the same 9B model: it plans the commands and it reviews its own
output. Nothing distinguishes that from the healthy case except the "fell back
from claude" badge in the agent UI.

**The decision this leaves open.** If Tier-2 verdicts are ever promoted from
advisory to blocking, route `review` through `route_and_execute` first. An
advisory signal is allowed to be cheap and noisy; a blocking one is not.

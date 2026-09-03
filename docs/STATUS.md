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
| `hive-core/src/watchdog/mod.rs` | `Watchdog::scan_line` — Tier 1, run on every streamed line, zero LLM cost. `Watchdog::review` — Tier 2, periodic (`WatchdogConfig::poll_interval_secs`, backing off to `reduced_poll_interval_secs` after `max_consecutive_safe` clean checks — both config fields Phase 1 had already added), asks the LLM "does this still look like it's working toward the objective?" over the JSON-extraction pattern proven in the Phase 2 planner. Soft-fails to "inconclusive, safe" on any LLM/parse error — Tier 1 remains the hard stop so a flaky LLM response can't spuriously pause a session. `extra_rules` from `hive.toml` are compiled in alongside the built-ins. |
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
correctly marked paused. (The pause *signal* itself — `tmux send-keys C-c` — logged a benign
"can't find pane" warning in this specific run, because the `echo` finished and closed its tmux
session before the signal arrived; there was nothing left to interrupt. State tracking and
detection are what the test asserts, and both worked.)

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

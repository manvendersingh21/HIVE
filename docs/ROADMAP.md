# Hive — Roadmap

All 10 phases, in dependency order. This ordering is canonical and comes from the
**Implementation Order** table in [`implementation-plan.md`](implementation-plan.md).

> **Note on numbering:** the narrative sections of `implementation-plan.md` use a slightly
> different numbering than its own Implementation Order table (the prose has no "Phase 8",
> and puts Skills at 5 / Finetune at 6 / CLI at 7). This roadmap uses the **table's**
> dependency-ordered numbering, and each phase below cites the plan section that documents it.

| # | Phase | Effort | Depends on | Status |
|:---:|:---|:---:|:---|:---|
| 1 | Scaffold, `hive-common` types, workspace | 1 day | — | ✅ done, build+test verified |
| 2 | LLM router + agent loop | 2–3 days | 1 | ✅ done, build+test verified |
| 3 | Worker pool, SSH delegation, tmux creation | 2 days | 2 | ✅ done, live-verified against a real worker (redesigned: direct SSH+tmux, no daemon — see below) |
| 4 | `hive-worker` daemon — real task execution | 1–2 days | 1 | ⬜ routes only, superseded by Phase 3's direct-SSH design (see note) |
| 5 | `hive-web` — web terminal | 2–3 days | 3 | ⬜ health route only |
| 6 | `hive-cli` — CLI subcommands | 1 day | 2–4 | 🟡 command tree wired, most subcommands are stubs |
| 7 | Skill system | 1–2 days | 2 | ⬜ empty struct |
| 8 | Fine-tuning pipeline | 1–2 days | 2 | ⬜ empty struct |
| 9 | Memory — projects, KG, RAG | 2–3 days | 2 | ⬜ empty struct |
| 10 | Safety watchdog | 2–3 days | 3, 7 | 🟡 Tier-1/Tier-2 detection + pause done (pulled into Phase 3), no incident log/notifier/UI |
| | **Total** | **~16–22 days** | | |

Legend: ✅ done · 🟡 partial · 🔴 broken · ⬜ not started

---

## Phase 1 — Project Scaffold & Core Types
*Plan section: "Phase 1: Project Scaffold & Core Agent Loop"* · **Status: ✅ done**

The foundation every other phase compiles against.

- [x] Workspace `Cargo.toml` with 5 members and centralized `[workspace.dependencies]`
- [x] `hive-common/src/protocol.rs` — the master↔worker wire protocol
- [x] `hive-common/src/error.rs` — unified `HiveError` + `HiveResult`
- [x] `hive-common/src/config.rs` — `HiveConfig`, `WorkersConfig`, env-var secret resolution
- [x] `hive-core` module tree (`agent`, `llm`, `workers`, `tools`, `skills`, `memory`, `watchdog`, `finetune`)
- [x] `hive-worker` and `hive-web` binary entry points with axum routes registered
- [x] **`hive-cli/src/main.rs`** — full `clap` command tree, `chat`/`task` build a real `MasterAgent`
- [x] **`hive-common` dependency fix** — added `rusqlite`, `reqwest`, `tracing`, and the `schemars` `"chrono"` feature
- [x] **`config/hive.toml`** and **`config/workers.toml`** — written from the plan's templates
- [x] **`cargo build --workspace` and `cargo test --workspace` pass** — verified: clean build, 18/18 tests, all 3 binaries run and respond over HTTP

**Definition of done:** `cargo test --workspace` is green and `hive --help` prints.

---

## Phase 2 — LLM Router & Agent Loop
*Plan section: "Phase 2: Master Agent — LLM Integration & Complexity Router"* · **Status: ✅ done**

The brain. Local model classifies, then routes.

- [x] `llm/local.rs` — Ollama client (`/api/chat`, `/api/embeddings`)
- [x] `llm/gemini.rs`, `llm/claude.rs`, `llm/openai.rs` — cloud clients (header-based auth, never a URL query key)
- [x] `LlmRouter::classify_complexity` — real classification prompt replacing the old hardcoded `Medium`
- [x] `LlmRouter::route_and_execute` — dispatch by `Complexity` → `AiProvider`, with fallback to the local model if a cloud provider isn't configured or fails
- [x] `MasterAgent::handle_request` — full loop: plan → decompose → execute locally / note remote → summarize
- [x] `agent/planner.rs` — LLM-driven decomposition producing `TaskPlan` / `SubTask`, with a safe no-op fallback when the LLM doesn't return parseable JSON
- [x] `tools/` — `shell.rs`, `file_ops.rs`, `git.rs` behind a `Tool` trait, schemas via `schemars`

Routing table: `Simple → Local` · `Medium → Gemini Flash` · `Complex → Claude` · `CodeHeavy → Codex`.

**Known limitation carried forward:** local subtasks now execute real shell commands via
`ToolRegistry`, with no confirmation gate and no safety watchdog yet (that's Phase 10). This
is the project's intended design, not an oversight — see `docs/STATUS.md` for the full note.
Remote (`requires_remote`) subtasks are only *noted*, not delegated — that's still Phase 3.

---

## Phase 3 — Worker Pool & SSH Delegation
*Plan section: "Phase 3: Worker Management & SSH Delegation"* · **Status: ✅ done, live-verified**

**Redesigned from the plan's pseudocode**: no `hive-worker` HTTP daemon in the loop. The master
drives `tmux` directly over SSH (`openssh`, `process-mux` — a real `ControlMaster` per
connection) and streams a remote log file rather than polling `tmux capture-pane`, specifically
because polling can miss short-lived output between checks. Every delegated session is watched
by a real (if partial — see Phase 10 note) safety layer, pulled forward because shipping
unattended remote command execution with zero safety net isn't acceptable even as an interim
state.

- [x] `WorkerPool::select_worker` — least-loaded-online selection
- [x] `WorkerPool::online_count`
- [x] `workers/ssh.rs` — `openssh` sessions, `ControlMaster` pooling + `server_alive_interval`
      keepalive, tmux session creation with dual-transport (live pane + piped log file) output,
      remote log tailing, `send-keys`, `capture-pane`
- [x] Health checks that actually flip `WorkerStatus` — `WorkerPool::refresh_health`, an SSH
      reachability probe, called from `hive-cli` before every `task`/`chat`
- [x] `WorkerPool::delegate` — creates the remote tmux session directly (no daemon to POST to)
- [x] `active_sessions()` — aggregates live sessions (shared `Arc<Mutex<HashMap>>`, updated by
      the background supervisor as sessions complete/fail/pause)
- [x] Load `workers.toml` into a live pool at startup — **this was silently broken**:
      `hive-cli/src/main.rs` called `WorkerPool::new(vec![])` unconditionally even though
      `WorkersConfig` parsing already existed for `hive workers list`. Fixed.
- [x] Tier-1 (regex) + Tier-2 (periodic LLM review) safety supervision, pulled forward from
      Phase 10 — see `docs/STATUS.md` for what's real vs. still-stub in that pull-forward.

**Known limitation, not yet solved:** the background supervisor is a `tokio::spawn` task tied to
the current process. `hive task` exits right after delegation confirms started, so anything
delegated through it is supervised for well under a second before the task is aborted — the
remote command keeps running regardless (verified: full output + exit code land in the log even
after the CLI exits), but nothing is watching it. `hive chat` supervises for its whole session.
Continuous supervision across the master's full uptime needs a persistent daemon — that's not
built (`hive serve` is still the Phase 1 stub). See `docs/STATUS.md` for detail.

**Not yet done**: remote agentic-CLI delegation (routing `AiProvider::Claude`/`Codex` to a
supervised `claude`/`codex` CLI session instead of a plain shell command) — the same machinery
should cover it once a worker has those CLIs installed; a **local** (no SSH) supervised session
for the already-authenticated local `claude`/`codex` is the more natural next slice than
installing them on every worker.

---

## Phase 4 — Worker Daemon
*Plan section: "Phase 3", `hive-worker/src/main.rs`* · **Status: ⬜ — superseded by Phase 3's design**

Phase 3 shipped direct SSH+tmux delegation instead of POSTing to this daemon (see its note
above). The routes below still exist and still respond (verified in Phase 1), but nothing calls
them anymore. Revisit this phase only if a use case actually needs a persistent per-worker agent
(e.g. accepting tasks without an open SSH session, or running as an unprivileged service) —
otherwise it may not be worth building out.

- [x] axum server with `/health`, `POST /task`, `GET /status/{task_id}`
- [ ] `executor.rs` — create the tmux session, `send-keys` each command, honor `working_dir`,
      `env_vars`, `timeout_secs`, `wait_for_completion`
- [ ] Real task registry so `/status/{id}` reports truth instead of a hardcoded `Running`
- [ ] `reporter.rs` — push `TaskStatus` back to the master
- [ ] Exit-code capture and output tailing via `tmux capture-pane`
- [ ] `pause` / `resume` / `kill` endpoints (the watchdog in Phase 10 depends on these)

---

## Phase 5 — Web Terminal
*Plan section: "Phase 4: Web Terminal (Phone/Laptop Access)"* · **Status: ⬜**

- [x] axum server with `/api/health`
- [ ] `GET /api/sessions` — real aggregation (returns a hardcoded `"[]"` today)
- [ ] `terminal.rs` — `WebSocket ↔ SSH ↔ tmux attach` bidirectional bridge
- [ ] `auth.rs` — basic auth middleware using `HIVE_WEB_PASSWORD`
- [ ] `static/index.html` — session picker dashboard
- [ ] `static/terminal.html` + bundled `xterm.js` — the terminal page (`static/` is empty today)
- [ ] Incident review UI (shared with Phase 10)

---

## Phase 6 — CLI
*Plan section: "Phase 7: CLI Interface"* · **Status: 🔴 no source file**

`clap`-based, binary named `hive`:

- [ ] `hive chat` — interactive session with the master agent
- [ ] `hive task -d "<description>"` — one-shot task submission
- [ ] `hive sessions` — table of active sessions across workers
- [ ] `hive attach <session_id>` — attach from a local terminal
- [ ] `hive workers <add|list|remove|health>`
- [ ] `hive skills <list|add|remove>`
- [ ] `hive finetune <export|stats>`
- [ ] `hive serve --bind` — start the web terminal server
- [ ] `hive project <new|list|switch>` and memory/search commands (needs Phase 9)

---

## Phase 7 — Skill System
*Plan section: "Phase 5: Skill & Plugin System"* · **Status: ⬜**

TOML-defined skills in `~/.hive/skills/<name>/`, each with `skill.toml`,
`system_prompt.md`, and optional `scripts/`.

- [ ] `skills/loader.rs` — parse `skill.toml` (metadata, trigger patterns, parameters, execution)
- [ ] `SkillRegistry::load_from_dir`
- [ ] `SkillRegistry::match_skill` — pattern matching plus LLM disambiguation
- [ ] `SkillRegistry::to_tool_definitions` — expose skills as LLM tools
- [ ] `require_confirmation` gate before execution
- [ ] Per-skill `ai_provider` override

---

## Phase 8 — Fine-Tuning Pipeline
*Plan section: "Phase 6: Fine-Tuning Pipeline"* · **Status: ⬜**

- [ ] `finetune/collector.rs` — log successful interactions (input, reasoning, tool calls, output)
- [ ] SQLite training-example schema
- [ ] `finetune/exporter.rs` — Alpaca / ShareGPT / ChatML export
- [ ] `hive finetune export --format sharegpt --output …`
- [ ] QLoRA training runbook via Unsloth or MLX-LM (4-bit, fits in 16GB)
- [ ] Load the resulting LoRA adapter back into Ollama

---

## Phase 9 — Memory: Projects, Knowledge Graph, RAG
*Plan section: "Phase 9: Conversation Memory, Knowledge Graph & Project Scoping"* · **Status: ⬜**

The reason the agent still knows what you decided three weeks ago.

- [ ] SQLite schema: `projects`, `conversations`, `messages`, `kg_nodes`, `kg_edges`, `rag_chunks`
- [ ] `memory/projects.rs` — project registry and conversation scoping
- [ ] `memory/extractor.rs` — local LLM extracts entities + relationships as JSON after each conversation
- [ ] `memory/knowledge_graph.rs` — upsert, dedup by cosine similarity (`entity_dedup_threshold = 0.85`), traversal
- [ ] `memory/rag.rs` — chunk (512 tokens / 64 overlap), embed via `nomic-embed-text`, vector search
- [ ] `MemorySystem::retrieve_context` — real KG + RAG + recent-message retrieval (returns empty vectors today)
- [ ] Context injection capped at `max_context_tokens`
- [ ] `hive search` / `hive memory` CLI surface

---

## Phase 10 — Safety Watchdog
*Plan section: "Phase 10: Safety Watchdog — Continuous Monitoring & Human-in-the-Loop"* · **Status: 🟡 partial — core detection pulled forward into Phase 3**

Polls every active session; pauses first, asks you second.

**Pulled forward and done** (see `docs/STATUS.md` Phase 3 for the live-verified detail):
- [x] `watchdog/rules.rs` — Tier-1 regex rules (`rm -rf`, disk format, `DROP TABLE`, force-push,
      `sudo`/`chmod 777`, `curl | sh`, credential patterns, fork bombs), plus `extra_rules` from
      config
- [x] `watchdog/mod.rs` — `Watchdog::review`, Tier-2 periodic LLM safety analysis producing
      `SafetyAnalysis`, checked against the task's `expected_behavior`, with `poll_interval_secs`
      backing off to `reduced_poll_interval_secs` after `max_consecutive_safe` clean checks
- [x] Pause path: `send-keys C-c` → mark `TaskState::PausedByWatchdog` → log a handover
      notification with the reattach command (not a full `Incident` log — see below)
- [x] Tail-based monitoring (not `tmux capture-pane -p` polling — a dedicated log-file tail, to
      avoid missing short-lived output between polls)

**Still not built:**
- [ ] `watchdog/mod.rs` as a `ractor` actor supervising *all* sessions in one place (today: one
      ad hoc `tokio::spawn` task per delegated session, no central supervisor)
- [ ] Persisted `Incident` log / `IncidentReviewState` tracking (today: a `tracing::warn!` line,
      nothing recorded)
- [ ] `watchdog/notifier.rs` — ntfy.sh push, webhook (Slack/Discord), web dashboard
- [ ] Incident review UI: resume / abort / resume-with-note / modify-and-resume
      (`HumanDecision` variants already exist in `protocol.rs`, nothing consumes them yet)
- [ ] Continuous supervision independent of CLI process lifetime (needs the persistent master
      daemon noted in Phase 3 — today, exiting `hive task`/`hive chat` ends supervision even
      though the delegated remote command keeps running)

---

## Open questions carried over from planning

1. **Worker details** — hostnames/IPs and SSH usernames for the 4 machines are still placeholders.
2. **Cloud spend** — no daily cost cap or confirmation threshold has been decided for
   Claude/Gemini/Codex calls.
3. **Web exposure** — basic auth is LAN-only-adequate. Anything beyond the LAN (e.g. Tailscale)
   should get TOTP or client certs.
4. **Fine-tuning corpus** — build up from usage, or seed from existing logs?

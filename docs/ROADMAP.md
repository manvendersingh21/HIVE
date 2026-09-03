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
| 3 | Worker pool, SSH delegation, tmux creation | 2 days | 2 | ⬜ selection logic only |
| 4 | `hive-worker` daemon — real task execution | 1–2 days | 1 | ⬜ routes only |
| 5 | `hive-web` — web terminal | 2–3 days | 3 | ⬜ health route only |
| 6 | `hive-cli` — CLI subcommands | 1 day | 2–4 | 🟡 command tree wired, most subcommands are stubs |
| 7 | Skill system | 1–2 days | 2 | ⬜ empty struct |
| 8 | Fine-tuning pipeline | 1–2 days | 2 | ⬜ empty struct |
| 9 | Memory — projects, KG, RAG | 2–3 days | 2 | ⬜ empty struct |
| 10 | Safety watchdog | 2–3 days | 3, 7 | ⬜ empty struct |
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
*Plan section: "Phase 3: Worker Management & SSH Delegation"* · **Status: ⬜**

- [x] `WorkerPool::select_worker` — least-loaded-online selection
- [x] `WorkerPool::online_count`
- [ ] `workers/ssh.rs` — `openssh` sessions with ControlMaster multiplexing
- [ ] Health checks that actually flip `WorkerStatus` (everything boots as `Offline` today)
- [ ] `WorkerPool::delegate` — create remote tmux session, POST the `TaskAssignment` to the daemon
- [ ] `active_sessions()` — aggregate live sessions across all workers
- [ ] Load `workers.toml` into a live pool at startup

---

## Phase 4 — Worker Daemon
*Plan section: "Phase 3", `hive-worker/src/main.rs`* · **Status: ⬜**

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
*Plan section: "Phase 10: Safety Watchdog — Continuous Monitoring & Human-in-the-Loop"* · **Status: ⬜**

Polls every active session; kills first, asks you second.

- [ ] `watchdog/mod.rs` — `ractor` actor supervising all monitored sessions
- [ ] Poll loop: `tmux capture-pane -p` over SSH every `poll_interval_secs` (5s, backing off to 15s
      after `max_consecutive_safe` clean checks)
- [ ] `watchdog/rules.rs` — fast regex rules (`rm -rf`, `DROP TABLE`, credential echo, `chmod 777`, …)
      plus user-defined `extra_rules` from config
- [ ] `watchdog/analyzer.rs` — LLM safety analysis producing `SafetyAnalysis`, checked against
      the task's `expected_behavior`
- [ ] Kill path: `send-keys C-c` → pause task → log `Incident` → notify
- [ ] `watchdog/notifier.rs` — ntfy.sh push, webhook (Slack/Discord), web dashboard
- [ ] Incident review UI: resume / abort / resume-with-note / modify-and-resume
      (`HumanDecision` variants already exist in `protocol.rs`)

---

## Open questions carried over from planning

1. **Worker details** — hostnames/IPs and SSH usernames for the 4 machines are still placeholders.
2. **Cloud spend** — no daily cost cap or confirmation threshold has been decided for
   Claude/Gemini/Codex calls.
3. **Web exposure** — basic auth is LAN-only-adequate. Anything beyond the LAN (e.g. Tailscale)
   should get TOTP or client certs.
4. **Fine-tuning corpus** — build up from usage, or seed from existing logs?

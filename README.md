# 🐝 Hive

A **self-hosted, distributed agentic system**. A master agent runs on a Mac Mini M4 (16GB)
with a local LLM, plans your tasks, judges how hard they are, and either handles them itself
or delegates them over SSH to worker machines on your LAN. Delegated work runs inside `tmux`
sessions you can attach to from your phone through a web terminal.

Every conversation is scoped to a **project** and indexed into a **knowledge graph + RAG**
index by the local model, so the agent remembers past decisions, commands, and errors when
you come back to a project weeks later.

A **safety watchdog** watches every running session continuously. If it sees something
dangerous, it pauses the task immediately (not kills — session state is preserved for
review) and escalates to you for a human decision.

---

## Why it's built this way

| Decision | Reason |
|:---|:---|
| Local model does planning + routing | Free, private, and fast enough on an M4; cloud is only paid for when the task actually needs it |
| Complexity router (local → Gemini Flash → Claude/Codex) | Most tasks are simple; don't pay Claude prices for `df -h` |
| tmux as the execution surface | Work survives disconnects, and any session is attachable from anywhere |
| SSH for delegation | No agents to install beyond a small daemon; inherits your `~/.ssh/config` |
| Rust workspace | One toolchain, shared types between master/worker/web, single static binaries to ship to workers |

---

## Architecture

```
┌──────────────────────── Mac Mini M4 (Master) ────────────────────────┐
│                                                                      │
│   hive-cli ──► MasterAgent ──► LlmRouter ──┬── local  (Ollama/Qwen)   │
│                    │                       ├── medium (Gemini Flash)  │
│                    │                       └── hard   (Claude/Codex)  │
│                    │                                                  │
│                    ├──► MemorySystem ──► Knowledge Graph + RAG + SQLite│
│                    ├──► Watchdog ──────► safety rules + LLM analysis   │
│                    ├──► SkillRegistry ─► TOML-defined custom skills    │
│                    └──► WorkerPool ────► least-loaded worker selection │
│                                                                       │
│   hive-web (axum + xterm.js)  ◄── phone/laptop browser over HTTPS      │
└───────────────────────────────┬───────────────────────────────────────┘
                                │ SSH + JSON over HTTP
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
   ┌─────────┐            ┌─────────┐            ┌─────────┐
   │ worker-1│            │ worker-2│    ...     │ worker-4│
   │hive-work│            │hive-work│            │hive-work│
   │  tmux   │            │  tmux   │            │  tmux   │
   └─────────┘            └─────────┘            └─────────┘
```

Full architecture diagrams, data model, and code sketches live in
[`docs/implementation-plan.md`](docs/implementation-plan.md).

---

## Crate map

| Crate | Binary | Role |
|:---|:---|:---|
| `hive-common` | — | Shared protocol types (`TaskAssignment`, `TaskStatus`, …), config schema, `HiveError` |
| `hive-core` | — | Master agent brain: agent loop, LLM router, worker pool, tools, skills, memory, watchdog, finetune |
| `hive-worker` | `hive-worker` | Daemon on each worker machine: receives tasks, runs them in tmux, reports status |
| `hive-web` | `hive-web` | axum server: session dashboard + WebSocket↔SSH↔tmux terminal bridge |
| `hive-cli` | `hive` | Your interface: `hive chat`, `hive task`, `hive sessions`, `hive workers`, … |

---

## Status

**Phases 1–3 are complete and live-verified** — `cargo build --workspace` and
`cargo test --workspace` both pass cleanly (28 passed, 1 opt-in live test), and `hive task`/
`hive chat` classify, plan, and execute real commands — locally, or delegated over real SSH to a
worker running inside a supervised tmux session. See [`docs/STATUS.md`](docs/STATUS.md) for the
full audit and [`docs/ROADMAP.md`](docs/ROADMAP.md) for all 10 phases.

Short version:

- ✅ Workspace + all 5 crate manifests, `cargo build --workspace` clean with zero warnings
- ✅ `hive-common` protocol, error, and config types — fully written, 18/18 unit tests passing
- ✅ `hive-core`: real `LlmRouter` (Ollama + Gemini/Claude/OpenAI clients, complexity routing
  with local fallback), an LLM-driven `Planner`, and a `Tool` registry (shell/file/git) that
  `MasterAgent::handle_request` actually calls for local subtasks
- ✅ Remote subtasks delegate for real: SSH (`openssh`, connection-pooled) into a worker,
  start a detached tmux session, stream its output live, and watch it with a Tier-1 regex +
  Tier-2 LLM-review safety layer (pulled forward from Phase 10) that pauses — not kills — a
  session that looks dangerous or off-track, logging the exact command to reattach and inspect
- ✅ `hive-worker` and `hive-web` axum servers — routes wired, verified live over HTTP (now
  bypassed by the direct-SSH delegation path above — see `docs/ROADMAP.md`'s Phase 4 note)
- ✅ `hive-cli/src/main.rs` — full command tree; `chat`/`task` drive a real `MasterAgent`;
  `workers list` and worker health checks reflect real config/real SSH reachability
- ✅ `config/hive.toml` and `config/workers.toml` in place — one real worker configured
- ⚠️ Watchdog supervision only lasts as long as the CLI process does — `hive task` exits right
  after delegating, so anything delegated through it runs unsupervised almost immediately
  after; `hive chat` supervises for its session. No persistent daemon yet. See `docs/STATUS.md`.
- ⚠️ No safety watchdog for **local** commands — only delegated/remote ones are supervised
- ⬜ Web terminal, skills, memory, fine-tuning, full incident review UI — see the roadmap

---

## Prerequisites

```bash
# Master (Mac Mini)
brew install ollama tmux
ollama pull qwen2.5:14b-instruct-q4_K_M   # planning + routing model
ollama pull nomic-embed-text              # embeddings for RAG (274MB)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Each worker
# - tmux installed
# - master's SSH key in ~/.ssh/authorized_keys
# - hive-worker binary deployed
```

## Build

```bash
cargo build --workspace
cargo test  --workspace
```

## Configuration

Two files, both read from `config/` at the project root:

- `config/hive.toml` — master settings, LLM providers, web auth, database, memory, watchdog
- `config/workers.toml` — the list of worker machines (name, host, user, tags)

API keys and the web password are **never** stored in config; they come from environment
variables (`GEMINI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `HIVE_WEB_PASSWORD`).

---

## Knowledge graph of this repo

`graphify-out/` holds a generated knowledge graph of the codebase itself:

- `graphify-out/GRAPH_REPORT.md` — plain-language architecture audit
- `graphify-out/graph.html` — interactive graph, open in any browser
- `graphify-out/graph.json` — structured graph for agent queries

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
│                    └──► WorkerPool ────► capability-based placement    │
│                                                                       │
│   hive-web (axum + xterm.js)  ◄── phone/laptop browser over the tailnet│
└───────────────────────────────┬───────────────────────────────────────┘
                                │ SSH (direct: tmux driven over the connection)
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
   ┌──────────────┐    ┌──────────────┐    ┌──────────────┐
   │archlinux-wkr │    │  cis-a6000   │    │  cis-linux2  │
   │ 4c / 11.6 GB │    │2× RTX A6000  │    │ shared login │
   │     tmux     │    │ 32c / 251 GB │    │  (bastion)   │
   └──────────────┘    └──────────────┘    └──────────────┘
```

Full architecture diagrams, data model, and code sketches live in
[`docs/implementation-plan.md`](docs/implementation-plan.md).

---

## Crate map

| Crate | Binary | Role |
|:---|:---|:---|
| `hive-common` | — | Shared protocol types (`TaskAssignment`, `TaskStatus`, …), config schema, `HiveError` |
| `hive-core` | — | Master agent brain: agent loop, LLM router, worker pool + placement, tools, skills, memory, watchdog, finetune |
| `hive-worker` | `hive-worker` | Daemon on each worker machine: receives tasks, runs them in tmux, reports status |
| `hive-web` | `hive-web` | axum server: session dashboard + WebSocket↔SSH↔tmux terminal bridge |
| `hive-cli` | `hive` | Your interface: `hive chat`, `hive task`, `hive sessions`, `hive workers`, … |

---

## Status

**The HACP/2.0 agent-collaboration protocol is designed, specified, implemented, and
proven** — a bilateral contract protocol with an independent second implementation
and live cross-vendor runs (claude×codex, agy×opencode) settling real lifecycles.
The full story, commit by commit, is in
[`docs/PROJECT-RECORD.md`](docs/PROJECT-RECORD.md); the protocol's own map is
[`docs/HACP-HIVE.md`](docs/HACP-HIVE.md) and its testing method is
[`docs/TESTING-YOUR-PROTOCOL.md`](docs/TESTING-YOUR-PROTOCOL.md).

**Phases 1–5 are complete and live-verified** — `cargo build --workspace` and
`cargo test --workspace` both pass cleanly (95 passed, 1 opt-in live test), and `hive task`/
`hive chat` classify, plan, and execute real commands — locally, or delegated over real SSH to
a machine the knowledge graph chose, running inside a tmux session the master supervises for
its whole life. See [`docs/STATUS.md`](docs/STATUS.md) for the
full audit, [`docs/ROADMAP.md`](docs/ROADMAP.md) for all 10 phases,
[`docs/PLACEMENT.md`](docs/PLACEMENT.md) for how a request becomes a machine, and
[`docs/DEPLOY-WEB.md`](docs/DEPLOY-WEB.md) for what runs where. Picking the work up from
here starts at [`docs/HANDOFF.md`](docs/HANDOFF.md) — what is done, what is next, and the
traps this repo has already paid for.

Short version:

- ✅ Workspace + all 5 crate manifests, `cargo build --workspace` clean with zero warnings
- ✅ `hive-common` protocol, error, and config types — fully written, 18/18 unit tests passing
- ✅ `hive-core`: real `LlmRouter` (Ollama + Gemini/Claude/OpenAI clients, complexity routing
  with local fallback), an LLM-driven `Planner`, and a `Tool` registry (shell/file/git) that
  `MasterAgent::handle_request` actually calls for local subtasks
- ✅ Remote subtasks delegate for real: SSH (`openssh`, connection-pooled) into a worker,
  start a detached tmux session, stream its output live, and watch it with a Tier-1 regex +
  Tier-2 LLM-review safety layer (pulled forward from Phase 10; Tier 2 always runs on the
  local model, never a routed one — see `docs/STATUS.md`) that pauses — not kills — a
  session that looks dangerous or off-track, logging the exact command to reattach and inspect
- ✅ **Agent chat UI** at `/` — talk to the master agent from a browser. It classifies the
  request, shows which model it routed to (and says so when a missing cloud key made it fall
  back to the local model), plans, and runs. Local commands the watchdog's Tier-1 rules flag
  stop and wait for an explicit approval in the UI instead of executing
- ✅ **Machine knowledge graph** at `/machines` — every machine is probed (OS, arch, cores,
  RAM, disk, GPU, installed tools) and projected into a SQLite entity/relation graph as
  `machine ──runs_os/has_arch/has_tool/has_capability──►`. Placement is a graph query rather
  than a hardcoded branch, and the same graph renders into the planner's prompt
- ✅ `hive-web` is a real web terminal: tmux session dashboard, create/kill sessions running
  `claude`, `codex` or a plain shell, and a `WebSocket ↔ PTY ↔ tmux attach` bridge rendering
  into xterm.js — password-gated, mobile key bar, auto-reconnect. Deployed on the master and reachable from any device on the tailnet
  (see `docs/DEPLOY-WEB.md`)
- ✅ `hive-worker` is a real daemon: accepts a `TaskAssignment`, runs its commands in a tmux
  session honoring `working_dir`/`env_vars`/`timeout_secs`/`wait_for_completion`, tracks true
  per-task state, captures exit codes, pushes status back to the master, and exposes
  `pause`/`resume`/`kill` (a real SIGSTOP/SIGCONT, so paused work can actually resume).
  Bearer-token authenticated — it refuses to start without one
- ✅ `hive-cli/src/main.rs` — full command tree; `chat`/`task` drive a real `MasterAgent`;
  `workers list` and worker health checks reflect real config/real SSH reachability
- ✅ `config/hive.toml` and `config/workers.toml` in place — three workers configured
  (`archlinux-worker`, `cis-a6000`, `cis-linux2`); the Azure `lawfinder` worker was retired
- ✅ **Capability-based placement** — machines are probed into a knowledge graph, the planner
  states what each subtask needs (`gpu-compute`, `containers`, …), and the graph picks the
  machine. A named machine is a decision, not a hint: Hive will not silently run a CUDA job
  somewhere without a GPU. See [`docs/PLACEMENT.md`](docs/PLACEMENT.md)
- ✅ **Durable supervision** — `hive task`/`hive chat` submit to the running master, so a
  delegated session is watched by a daemon for its whole life rather than for the moment the
  CLI happens to stay alive
- ✅ **Safety gate on local commands, in the CLI as well as the web UI** — anything the
  watchdog's Tier-1 rules flag stops and asks. With no tty (piped, scripted) the answer is
  *deny*, never *run*
- ⚠️ Nothing routes GPU work through SLURM yet — `cis-a6000` is a shared university node, and
  heavy jobs belong in `sbatch` rather than a tmux session. The graph records the capability;
  no policy acts on it. See `docs/PLACEMENT.md` §5
- ⚠️ Watchdog incidents are `tracing` warnings only — no incident log, queue, or notifier
- ⬜ Skills, RAG/projects/history, fine-tuning, incident review UI — see the roadmap

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

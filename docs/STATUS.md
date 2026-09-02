# Hive — Current Status

**As of 2026-09-01.** This is a verified audit of what exists on disk, not a summary of what
was claimed. Every item below was checked against the actual files.

---

## Answer to "is Phase 1 completed?"

**No.** The previous build session reported Phase 1 complete and the workspace "fully
scaffolded", then the session was interrupted before that claim could be checked. It doesn't
hold up: **the workspace does not compile**, for two independent reasons, and it was never
built or tested even once.

The type layer — which is genuinely the bulk of Phase 1's value — *is* done and is good work.
What's missing is the last mile that makes it a running program.

---

## What is actually done ✅

### `hive-common` — the shared foundation (1,424 lines, fully written)

| File | Lines | Contents |
|:---|---:|:---|
| `protocol.rs` | 681 | `TaskAssignment` + builder, `TaskCommand` + builder, `AiProvider`, `AiContext`, `TaskPriority`, `TaskStatus` + `running`/`completed`/`failed` constructors, `TaskState` (7 variants incl. `WaitingForDecision`, `PausedByWatchdog`), `Complexity` + `from_llm_output` + `recommended_provider`, `WorkerInfo` + `ssh_target`, `WorkerStatus`, `Severity`, `SafetyCategory` (8 variants), `SafetyAnalysis`, `Incident`, `HumanDecision`, `IncidentReviewState`, `AgentResponse`, `SessionInfo`. **8 unit tests.** All types derive `JsonSchema` for LLM tool calling. |
| `config.rs` | 576 | `HiveConfig` and every sub-config (`master`, `llm`, `web`, `database`, `skills`, `finetune`, `memory`, `watchdog`), `WorkersConfig`, TOML loading, `~` expansion for the DB path, and secret resolution from env vars with sensible fallbacks. **7 unit tests.** |
| `error.rs` | 167 | `HiveError` with 28 variants across config / LLM / SSH / task / tmux / DB / memory / skill / safety domains, `HiveResult<T>`, `From` impls for `io`, `serde_json`, `rusqlite`, `reqwest`, `toml`. **3 unit tests.** |

### `hive-core` — skeleton in place, one piece real

- All 8 modules declared and present: `agent`, `llm`, `workers`, `tools`, `skills`, `memory`, `watchdog`, `finetune`
- `workers/mod.rs` — `WorkerPool::new`, `select_worker` (least-loaded online), `online_count`. **Real logic.**
- `agent/mod.rs` — `MasterAgent` struct wiring all four subsystems, `handle_request` skeleton that
  retrieves context → classifies complexity → picks a provider, then stops at a `TODO`
- `agent/planner.rs` — `TaskPlan` and `SubTask` types defined
- `llm/mod.rs` — `LlmRouter` struct; `classify_complexity` returns a hardcoded `Medium`,
  `local_complete` returns an empty string
- `memory`, `skills`, `tools`, `watchdog`, `finetune` — empty structs with `new()` and a `// TODO` comment

### `hive-worker`

- axum server, `HIVE_WORKER_ADDR` (default `0.0.0.0:9091`), tracing configured
- Routes registered: `GET /health`, `POST /task`, `GET /status/{task_id}`
- `receive_task` deserializes a real `TaskAssignment` and returns a `TaskStatus::running` —
  but **creates no tmux session and runs no commands**
- `task_status` returns a hardcoded `Running` for any id

### `hive-web`

- axum server, `HIVE_WEB_ADDR` (default `0.0.0.0:8080`), tracing configured
- `GET /api/health` → `"ok"`; `GET /api/sessions` → hardcoded `"[]"`
- `ServeDir` fallback pointed at `static/` — the WebSocket route is commented out

### Workspace

- Root `Cargo.toml` with 5 members and every dependency centralized in `[workspace.dependencies]`
  (tokio, serde, axum, tower-http, rusqlite, openssh, tmux_interface, ractor, clap, schemars, …)

### Knowledge graph

- `graphify-out/` — 209 nodes, 412 edges, 19 communities, with `GRAPH_REPORT.md`,
  interactive `graph.html`, and `graph.json`

---

## What is broken or missing ❌

### 1. `hive-cli` has no source file — build-breaking

`hive-cli/Cargo.toml` declares:

```toml
[[bin]]
name = "hive"
path = "src/main.rs"
```

`hive-cli/src/` exists but is **empty**. `main.rs` was never written. Because `hive-cli` is a
workspace member, `cargo build --workspace` fails outright.

### 2. `hive-common` is missing two of its own dependencies — build-breaking

`hive-common/src/error.rs` contains:

```rust
impl From<rusqlite::Error> for HiveError { … }
impl From<reqwest::Error>  for HiveError { … }
```

but `hive-common/Cargo.toml` declares only `serde`, `serde_json`, `thiserror`, `chrono`,
`uuid`, `schemars`, and `toml`. Neither `rusqlite` nor `reqwest` is there, so both `impl`
blocks reference crates that don't exist in scope — an unresolved-crate error, not a warning.
**`hive-common` is the base of every other crate, so nothing in the workspace compiles.**

### 3. No configuration files

`config/` is an empty directory. Both `config/hive.toml` and `config/workers.toml` are
specified in the plan, and `HiveConfig::from_project_root()` / `WorkersConfig::from_project_root()`
are written to read exactly those paths — but neither file was ever created. Nothing can be
configured or started.

### 4. Nothing has ever been compiled or run

There is **no Rust toolchain on this machine** — `rustc`, `cargo`, and `rustup` are all absent,
and `~/.cargo` does not exist. So `cargo build` and `cargo test --workspace` have never been
executed. The 18 unit tests in `hive-common` are written but **unverified** — they have never
passed, because they have never run.

### 5. Minor — unused imports

`hive-core/src/agent/mod.rs` imports `TaskAssignment`, `TaskCommand`, and `warn`;
`hive-core/src/workers/mod.rs` imports `TaskAssignment` and `TaskStatus`. None are used yet.
Warnings only, but they'll be noisy on the first successful build.

### 6. Not blocking Phase 1, but worth noting

- `hive-web/static/` is empty — no `index.html`, no `terminal.html`, no `xterm.js` (Phase 5)
- The repo is **not a git repository** — no version control, no history, no way to roll back

---

## Phase 1 completion: item by item

| Deliverable | State |
|:---|:---|
| Workspace `Cargo.toml`, 5 members, shared deps | ✅ |
| `hive-common` protocol types | ✅ |
| `hive-common` error types | ✅ |
| `hive-common` config types | ✅ |
| `hive-core` module skeleton | ✅ |
| `hive-worker` entry point | ✅ |
| `hive-web` entry point | ✅ |
| `hive-cli` entry point | ❌ file absent |
| `hive-common` deps match its code | ❌ two missing |
| `config/hive.toml` | ❌ absent |
| `config/workers.toml` | ❌ absent |
| `cargo build --workspace` passes | ❌ never run, would fail |
| `cargo test --workspace` passes | ❌ never run |

**~70% complete. 7 of 13 deliverables done, and the remaining 6 are the ones that turn it
from a pile of types into something that runs.**

---

## To close out Phase 1

1. Install the Rust toolchain (`rustup`) — nothing can be verified without it
2. Add `rusqlite` and `reqwest` to `hive-common/Cargo.toml`
3. Write `hive-cli/src/main.rs` with the clap command tree (stub handlers are fine for now)
4. Write `config/hive.toml` and `config/workers.toml` from the plan's templates
5. Run `cargo build --workspace`, fix whatever the first real compile surfaces
6. Run `cargo test --workspace` and confirm all 18 tests pass
7. `git init` and commit the working baseline

Then Phase 2 (LLM router + agent loop) can start against a foundation that's known-good
rather than assumed-good.

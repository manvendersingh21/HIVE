# Hive — Current Status

**Updated 2026-09-01, verified against a live build.** This audit reflects what was actually
compiled, tested, and run — not what a session claimed.

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

## Next: Phase 2 — LLM Router & Agent Loop

See [`ROADMAP.md`](ROADMAP.md#phase-2--llm-router--agent-loop). Starting point is
`hive-core/src/llm/mod.rs` (`LlmRouter::classify_complexity`, `local_complete`) and wiring a
real Ollama client so `MasterAgent::handle_request` does more than classify and stop.

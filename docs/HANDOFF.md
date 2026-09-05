# Handoff — what is done, what is next, and how to work here

Written 2026-09-05, immediately after **M3** (`06269f2`) landed. This document exists so
that a model or engineer arriving with no prior context can pick the work up and finish it
without re-deriving the project's history, its rules, or the traps it has already fallen
into three times.

Read this file, then [`ROADMAP.md`](ROADMAP.md) (per-phase checklists) and
[`STATUS.md`](STATUS.md) (what was proved, and how). The approved milestone plan lives
outside the repo at `~/.claude/plans/working-status-1-memoized-crayon.md`.

---

## 0. The laws of this repo — non-negotiable

These are not style preferences. Each one exists because it was violated once and cost
real time or real safety.

1. **A status row moves to ✅ only after the command that proves it has been run and its
   output pasted into the doc.** This is the project's oldest law and the reason
   `STATUS.md` exists at all. "It should work" is not a status. Neither is "the tests
   compile."
2. **The repo is public.** No absolute home paths (`/Users/<you>/...`, `/home/<you>/...`) may ever be
   committed. `host` in `config/workers.toml` is an SSH config alias, never a raw IP —
   that is deliberate, do not "fix" it.
3. **Secrets never touch disk, argv, or the wire.** The collab auth token comes from the
   `HIVE_COLLAB_TOKEN` env var, never a command-line argument (argv is world-readable in
   the process table) and never written by `init_run`. Secrets must not appear in briefs,
   contracts, or logs.
4. **Vendor / CLI identity must never reach the bus, a brief, or a contract** (HACP §3).
   The role→tool mapping lives only in the run record. An agent must not be able to learn
   that its counterpart is Codex rather than Claude.
5. **Zero warnings is the standing bar — in *both* profiles.** `cargo build --workspace`
   does not surface test-profile warnings. `cargo test --workspace --no-run` does. Run
   both. This exact trap has been hit twice (unused test helpers in `review.rs`).
6. **Never trust a binary you did not just rebuild.** `cargo test -p hive-web` builds the
   *test harness*, not `target/debug/hive-web`. A live test against a stale binary once
   produced a green `200` that proved nothing. `cargo build` before any live check.

---

## 1. Verified baseline as of this handoff

```
$ cargo build --workspace                → zero warnings
$ cargo test  --workspace --no-run       → zero warnings (test profile too)
$ cargo test  --workspace
TOTAL: 449 passed, 0 failed, 5 ignored
$ interop/run-interop.sh
test an_independent_peer_interoperates_over_the_file_edge ... ok
```

| # | Phase | State |
|:---:|:---|:---|
| 1–5 | Scaffold, LLM router, worker pool, worker daemon, web UI | ✅ |
| 6 | CLI | ✅ except `skills` / `finetune`, which wait on 7–8 |
| 7 | Skill system | ⬜ **empty struct** — `hive-core/src/skills/mod.rs` is 13 lines |
| 8 | Fine-tuning pipeline | ⬜ **empty struct** — `hive-core/src/finetune/mod.rs` is 13 lines |
| 9 | Memory: projects, KG, RAG | 🟡 KG substrate live; projects / conversations / RAG **not started** |
| 10 | Safety watchdog | ✅ complete, live-verified (M3) |

HACP/2.0 Core is complete (§1–§14, schemas, refusal vectors, independent Python peer).
What is unbuilt there is the **HIVE runtime layer** — see §5.

---

## 2. The gate — run this before claiming anything

```bash
cargo build --workspace                  # zero warnings
cargo test  --workspace --no-run         # zero warnings in the TEST profile too
cargo test  --workspace                  # 449 passing at this handoff; must not regress
interop/run-interop.sh                   # must stay green; do not touch hacp/ to make it so
```

Count the totals with:

```bash
cargo test --workspace 2>&1 | grep -E '^test result' \
  | awk '{p+=$4; f+=$6; i+=$8} END {print "TOTAL:", p, "passed,", f, "failed,", i, "ignored"}'
```

---

## 3. M4 — Phase 9 memory (the next milestone)

**The one-line statement of the gap:** `MemorySystem::retrieve_context` returns three
empty vectors, and its only two call sites throw the result away. The agent has no memory
of any conversation it has ever had. The knowledge graph underneath it is real and proven
— it holds the machine fleet — so this is a build-on-a-foundation job, not a from-scratch
one.

### 3.1 What exists and works

| File | Lines | What it is |
|:---|---:|:---|
| `hive-core/src/memory/graph.rs` | 371 | `KnowledgeGraph` — SQLite, WAL, `Arc<Mutex<Connection>>`, caller-chosen ids, idempotent upsert. Tables `entities(id, kind, name, attrs, updated_at)` and `edges(from_id, relation, to_id)`. API: `open`, `in_memory`, `upsert_entity`, `add_edge`, `clear_relation`, `remove_entity`, `entity`, `entities_of_kind`, `neighbors`, `sources_of`, `edges_from`, `snapshot`. |
| `hive-core/src/memory/machines.rs` | 785 | The proof that the substrate works — the machine fleet projected into the graph, with capability placement. See [`PLACEMENT.md`](PLACEMENT.md). |
| `hive-core/src/llm/local.rs:116` | — | `OllamaClient::embed(&str) -> Vec<f32>` already exists. Embeddings are a solved problem here; do not add a new HTTP client. |

### 3.2 What is missing, in dependency order

1. **Project registry — `memory/projects.rs`.** `projects` and `conversations` and
   `messages` tables. Everything else is scoped by `project_id`, so this is first.
2. **Conversation persistence.** Nothing writes a transcript today. `handle_request`
   (`hive-core/src/agent/mod.rs:121`) and `plan_run` (`:269`) are the two places a turn
   passes through.
3. **RAG — `memory/rag.rs`.** Chunk at `memory.chunk_size` (512) with
   `memory.chunk_overlap` (64), embed via `OllamaClient::embed` using
   `memory.embedding_model` (`nomic-embed-text`), store the `f32` vector as a BLOB,
   search by cosine. A linear scan is acceptable at this scale — say so in the module doc
   rather than pretending it is an ANN index.
4. **Extraction — `memory/extractor.rs`.** Local LLM extracts entities + relationships as
   JSON after a conversation ends. Cap at
   `memory.knowledge_graph.max_entities_per_conversation` (20). Dedup by cosine
   similarity at `memory.knowledge_graph.entity_dedup_threshold` (0.85).
   **Route this through `LlmRouter::local_complete`, never `route_and_execute`** — same
   reasoning as the Tier-2 watchdog review (see `watchdog/mod.rs::review`'s doc comment):
   it fires continuously and must not bill a cloud provider per turn.
5. **`retrieve_context` for real** — KG + RAG + recent messages, capped at
   `memory.max_context_tokens` (2048).
6. **Actually inject it.** `hive-core/src/agent/mod.rs:129` binds `let _context = …` and
   discards it; `:277` does the same. Retrieval that is never injected is a more
   expensive way of doing nothing.
7. **CLI surface** — `hive search` / `hive memory`, promised by the roadmap, absent from
   `hive-cli/src/main.rs`.

All config knobs already exist and are parsed — `MemoryConfig` and `KnowledgeGraphConfig`
in `hive-common/src/config.rs:253–305`, defaults in `config/hive.toml:38–45`. Nothing new
needs to be added to config.

### 3.3 The one real design decision — resolve it before writing code

`docs/implementation-plan.md:681+` prescribes `kg_nodes` / `kg_edges` tables scoped by
`project_id`. `memory/graph.rs` already ships an **unscoped** `entities` / `edges` schema
that holds the machine fleet and is live. These are two different schemas for the same
idea.

Pick one and record why in `STATUS.md`:

- **(a) Extend the existing graph** with an optional `project_id` attribute or a
  `scope` column. Keeps one graph, one API, one set of traversals; the machine fleet
  becomes the `null`-project scope. **This is the recommended path** — the substrate is
  proven and `machines.rs` already depends on its exact API.
- **(b) Add separate project-scoped tables** as the plan literally says. Truer to the
  plan, but you then have two graph implementations and `machines.rs` sits on the older
  one forever.

Whichever you choose, do not silently diverge from the plan without writing down that you
did. That is what `STATUS.md` is for.

### 3.4 A trap specific to M4

`hive-cli/src/main.rs:186` builds `MemorySystem::new()` — the **in-memory** constructor —
while `hive-web/src/main.rs:114` uses `MemorySystem::open(config.database.resolved_path())`.
So today the CLI has no persistence at all and the web UI does. If you implement Phase 9
and test it only through `hive chat`, every memory will vanish between invocations and
you will debug the wrong layer. Fix the CLI to `open` the configured path as part of M4.

### 3.5 Acceptance for M4

Not "the tests pass". The proving run is a real one, pasted into `STATUS.md`:

```bash
hive chat --project <name>       # say something specific and factual
# exit, start a new process
hive chat --project <name>       # ask about it; the answer must come from memory
hive search "<term>"             # must find the earlier conversation
```

Plus: the DB file must be `chmod 0600` like the incident log (`watchdog/incidents.rs`
does this and asserts it in `the_database_is_not_world_readable`) — conversation
transcripts are at least as sensitive as flagged output, and they share the same file.

---

## 4. M5 — Phases 7 and 8

Both are 13-line empty structs. Smaller and much better specified than M4.

### Phase 7 — skill system (`hive-core/src/skills/mod.rs`)

Plan section: `implementation-plan.md:461` ("Phase 5: Skill & Plugin System").
Skills live in `skills.directory` (`~/.hive/skills`, already in `SkillsConfig`), each a
directory with `skill.toml`, `system_prompt.md`, optional `scripts/`.

- `skills/loader.rs` — parse `skill.toml`: metadata, trigger patterns, parameters, execution
- `SkillRegistry::load_from_dir`
- `SkillRegistry::match_skill` — pattern match, then LLM disambiguation
- `SkillRegistry::to_tool_definitions` — expose skills as LLM tools (mirror the existing
  `Tool` trait + `schemars` schemas in `hive-core/src/tools/`)
- `require_confirmation` gate before execution — **this is a safety surface, not a nicety**
- Per-skill `ai_provider` override
- Unstub `hive-cli/src/main.rs:148`, which currently prints
  *"the skill loader is not implemented (Phase 7)"*

Note the roadmap records Phase 10 as depending on 3 **and 7**. Phase 10 shipped without
7, so if the skill system introduces a new execution path, check that it goes through the
watchdog's Tier-1 gate rather than around it.

### Phase 8 — fine-tuning pipeline (`hive-core/src/finetune/mod.rs`)

Plan section: `implementation-plan.md:529`. `FinetuneConfig::auto_collect` already
defaults to `true` and is parsed, so config claims collection is happening when nothing
collects.

- `finetune/collector.rs` — log successful interactions (input, reasoning, tool calls, output)
- SQLite training-example schema (same `hive.db`; same 0600 rule)
- `finetune/exporter.rs` — Alpaca / ShareGPT / ChatML
- Unstub `hive-cli/src/main.rs:153–155` — `hive finetune export --format sharegpt --output …`
- QLoRA runbook (Unsloth or MLX-LM, 4-bit, fits 16GB) and loading the adapter back into Ollama

**Privacy note that must be handled, not deferred:** training data is verbatim
conversation content, and the watchdog's own `SafetyCategory::CredentialExposure` exists
because credentials *do* appear in captured output. Either exclude flagged interactions
from collection or document loudly that the export may carry secrets. Do not let
`auto_collect = true` quietly build a corpus of the user's credentials.

---

## 5. Runtime follow-ons (HACP) — ⬜ in `HACP-HIVE.md` §6

Independent of M4/M5; sequence them when you get there.

- **Recursive spawn / delegate lifecycles** (§8, §13) under the
  `hive-recursive-pairwise/1` profile: arity ≤ 2, capability grants, LCA routing. The
  profile module exists and is fully tested (`hacp/src/v2/profile.rs`) but **no runtime
  exercises it**. Today `hive collab run` is two agents, one contract, one machine.
- **Org bootstrapping.**
- **Distribution over SSH workers** — every agent currently runs in a local tmux session,
  even though the SSH + tmux delegation path has been live since Phase 3.
- **Finding 11 — the objective-vs-contract gap.** In a live `claude × codex` run the
  artifact was six bytes (`ready\n`) and satisfied every acceptance criterion the
  supervising agent had written. The worker performed the contract; the verifier verified
  the contract; the contract was not the objective. §9.4 catches an agent lying about what
  it did and says nothing about whether it was worth doing. Deliberately not patched, and
  it belongs to **whichever milestone next touches formation** — i.e. whoever writes the
  code that drafts contracts. See [`findings/adapter-edge.md`](findings/adapter-edge.md) and `STATUS.md` → "What these runs do not show".

---

## 6. Carried-forward debts — small, precise, safe to pick up any time

| Where | What |
|:---|:---|
| `hive-web/src/incidents.rs:104` and `hive-core/src/watchdog/supervisor.rs:133` | Both resolve `~/.hive/hive.db` independently because `DatabaseConfig` has no `Default`. One resolver belongs in `hive-common`. Two copies of a path default is exactly how they drift. |
| `hive-core/src/watchdog/supervisor.rs:242,254` | `stop_session` and `shutdown` exist so supervision is addressable, but no CLI or web surface calls either. |
| `hive-core/src/watchdog/supervisor.rs` registry | Terminal rows are retained, because `delegate`'s contract says `active_sessions()` is how completion is observed. A long-lived master accumulates one row per delegated session — the same unbounded growth the Phase 3 map had, carried forward **deliberately** rather than changed silently. Changing it means changing that contract; do it openly. |
| `hive-core/src/workers/mod.rs:215` | `worker.active_tasks` is incremented on delegate and never decremented, so `select_worker`'s least-loaded choice degrades over the life of the process. Pre-existing, pre-dates M3. |
| `hive-core/src/agent/mod.rs:118–120` | Stale doc comment: claims *"There is still no incident queue or push notification (Phase 10)"*. M3 built both. |
| `hive-core/src/agent/planner.rs:122` | Stale doc comment: *"for the watchdog, once Phase 10 lands"*. It landed. |
| `hive workers add\|remove` | **Deliberately not built.** A TOML serializer would drop `config/workers.toml`'s load-bearing comments. Do not build it without solving comment preservation first. |

---

## 7. Traps this repo has already paid for

Each of these cost time at least once. They are recorded so they cost it exactly once.

- **Test-profile warnings are invisible to `cargo build`.** Run `cargo test --workspace
  --no-run`. Hit twice.
- **A stale binary lies.** `cargo test -p X` does not rebuild `target/debug/X`. Hit twice
  (M2: `hive workers list` widths unchanged after the patch; M3: a `200` from the old
  handler).
- **Python heredocs eat Rust `\`-newline continuations.** Use a *quoted* bash heredoc
  (`<<'RUST'`) when writing Rust from a script, or the continuation vanishes and you get a
  one-line string with twenty embedded spaces — which shipped once, visible in a live 502
  body.
- **`grep -c ... | ... && ...`** — `grep -c` exits 1 when the count is 0, breaking the
  `&&` chain and making a *clean* result look like a failed command.
- **`axum 0.8 Router::layer` wraps only what was registered before it.** `.fallback_service`
  after `.layer(require_auth)` serves every static page unauthenticated. That was a real
  hole, found and fixed in M3; do not reintroduce it by appending a route at the bottom.
- **`ractor`'s default `handle_supervisor_evt` is `myself.stop(None)` on *any* child
  exit** — including a normal one. Without the override in `watchdog/supervisor.rs`, the
  first session to finish successfully tears down supervision of every other running
  session. Verified empirically: deleting the override fails 7 of 9 supervisor tests.
- **SIGSTOP, not kill.** `pause_session` SIGSTOPs the pane's foreground process group and
  verifies state `T` held; `resume_session` finds stopped pgids on the pane's tty. This
  was gotten wrong in three separate instalments — all recorded in `STATUS.md`. Do not
  "simplify" it to `send-keys C-c`.
- **Ordering when carrying out a human decision** (`watchdog/review.rs`):
  `ResumeWithNote` = `send_line` **then** `resume` (the tty buffers while stopped;
  resuming first races the command finishing). `ModifyAndResume` = `interrupt`, `resume`,
  `send_line`. These orderings are tested; if a test fails, the ordering is right and the
  change is wrong.
- **Record before acting.** `IncidentStore::resolve` is a compare-and-swap
  (`UPDATE … WHERE id = ?1 AND review_state = 'pending_review'`) so exactly one operator
  can act. Acting first would let two operators both reach the session, and resume racing
  abort on one suspended process is precisely the state a human was asked to prevent. The
  narrow window where the row commits but SSH fails is surfaced as
  `DecisionError::RecordedButNotApplied`, not hidden.

---

## 8. How to work here

- **Parallelize by dependency graph, not by file count.** M3's shape worked: build the
  thing everything else reads (the incident store) yourself first, fix its API, then hand
  agents precise contracts for the independent pieces (notifier delivery, web UI,
  supervisor).
- **Assign file boundaries so no two workers touch the same file.** Pre-declare new
  modules in the parent `mod.rs` yourself and stub the new files, so no agent ever needs
  to edit a shared file.
- **Review what agents produce.** In M3 an agent's ntfy error path formatted the raw
  `reqwest::Error`, which echoes the URL — and on the public ntfy server the topic *is*
  the entire access control. It needed redaction like the webhook path. Agents produce
  good code; they do not always know which string is a secret.
- **Doc comments carry the "why", not the "what".** This codebase's convention is that
  every non-obvious decision has its rationale in the module or function doc — see
  `watchdog/mod.rs::review` (why Tier 2 is always local) or `incidents.rs` (why the DB is
  0600). Match it.
- Commit trailers required on every commit:

  ```
  Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
  Claude-Session: <session url>
  ```

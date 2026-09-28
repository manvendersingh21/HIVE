# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

No versioned release has been tagged yet. Everything below is unreleased work on
the development branch, and the public interfaces described in the documentation
may still change without a deprecation period.

## [Unreleased]

### Added

- Fleet delegation: native agent sessions per assignment, each with its own tmux
  session, durable journal, and preserved native conversation. Opt-in behind
  `HIVE_DELEGATION=1`. See [docs/FLEET-DELEGATION.md](docs/FLEET-DELEGATION.md).
- Runner adapters for four native agent interfaces — Claude (Agent SDK, Python
  and Node paths), Codex (app-server), OpenCode (server API), and AGY (JSON
  stream with hooks).
- Approval workflow with single-use grants: a decision is bound to the
  fingerprint of the action it approved, so a changed action requires a new
  decision.
- Coordinator recovery. A run whose connection was lost is reconciled from its
  remote journal; deliveries with an uncertain outcome are acknowledged rather
  than replayed.
- Per-device inventory: executables, versions, authentication state, runtime
  prerequisites, and discovered models, refreshed in the background.
- Saved chats — search, reopen, and continue web conversations stored in SQLite.
  See [docs/CHAT-HISTORY.md](docs/CHAT-HISTORY.md).
- Project-scoped memory: knowledge graph extraction plus a RAG pipeline over
  conversation history.
- NVIDIA provider support for master reasoning and embeddings, alongside the
  existing local (Ollama) provider.
- Skill registry loading at startup, so configured skills activate at request
  time.
- [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md) describing how a prompt becomes
  device and agent assignments, and what is validated before anything launches.

### Changed

- HACP is pinned to `3d008d4719e0dcf0ba4057650831ab4696495923`, with
  guardian-backed secure envelopes enabled in `hive-core`. This pin adopts
  HACP's V3/V4/V7 fixes: bilateral delegation contracts are formed under a
  per-run capability grant (the deployment charters the supervisor, which
  grants the worker) with a matching escalation path, and contracts set
  `max_rework = 2`. A rework verdict beyond that bound ends the run as
  `Rejected` with a "rework budget exhausted" reason, even when
  `--max-rework` allows more attempts.
- HACP is consumed as a commit-pinned Git dependency from its own repository
  rather than vendored into this tree.
- Placement restrictions are enforced during plan validation: assignments
  requesting `gpu-compute` or `heavy-compute` are refused on machines tagged
  `light`, `login-node`, or `slurm`.
- Documentation reorganized around a single [docs index](docs/README.md);
  working notes from development sessions were removed from the repository root.

### Fixed

- Run journals no longer store one event per streamed token. The Cursor adapter
  joins contiguous `thinking` deltas and contiguous assistant text into one
  event per block, flushed by any other event (tool call, tool result, result)
  and at turn end; tool calls and results stay individual events. AGY response
  `text_delta` step updates are joined the same way. OpenCode no longer
  re-journals the whole message history every turn, nor a streaming message on
  every poll: an unfinished message is journaled again only when a part starts
  or changes status. A recorded Cursor run shrinks from 5,443 to 403 native
  events. The native conversation id is written only when it changes, not on
  every stream line.
- Cursor runs record the `--model` value actually passed (or `auto`) as the
  actual model and invocation evidence, never the init event's display name
  (e.g. "GPT-5.2 Medium"), which `--model` rejects. A rejected model's
  "Available models: ..." list is saved as the run's available models, and
  display names stored by earlier runs are dropped when the run loads.
- The master-agent provider selection and any Z.ai key entered in the
  settings UI are now persisted robustly: `~/.hive/master-agent.json`
  (relocatable via `HIVE_MASTER_AGENT_FILE`) is written atomically with
  `0600` permissions, re-applied at startup below env/`hive.toml`
  precedence, and never echoed by the API or logs. A corrupt state file is
  ignored instead of affecting startup. See
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).
- Peer dependency deadlocks: a queued verifier can start when its implementer
  is waiting for a peer with a pending message addressed to that verifier.
  Other working or failed prerequisites still block launch. Plans can declare
  `peer_dependencies` for required replies or agreements; validation rejects
  peers queued directly or transitively behind their asker. Waiting runs explain
  when a requested peer is queued behind them, across repeated sync cycles.
- `config/workers.toml` is no longer tracked. It describes a specific fleet,
  including real hostnames and SSH account names; copy
  `config/workers.example.toml` instead.
- The mock planning response in `scripts/check-nvidia.py` omitted the
  `target_machine` field that plan validation requires, failing CI on every run.
- Local machine probe in `machines.rs`, delegation launch in `hive-core/src/delegation/transport.rs local_shell`,
  and session launch in `hive-web/src/sessions.rs` now resolve and inherit the user's login-shell PATH
  (with timeout and caching) with close-on-exec fd isolation, so tools added via shell profiles
  (e.g. `~/.cargo/bin`, `~/.opencode/bin`, `~/.local/bin`) are discovered and executable without hardcoding or fd leakage.

### Known limitations

- AGY and OpenCode adapters have not been validated against live
  provider-backed execution; presence in inventory is not proof of working
  authentication.
- A completed, independently verified two-way 1 GiB transfer proof between two
  worker machines has not been recorded.
- Autonomous device and model scheduling is not implemented; the supervisor
  authors and verifies tasks.
- Fine-tuning data collection and export are not implemented.

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

- HACP is consumed as a commit-pinned Git dependency from its own repository
  rather than vendored into this tree.
- Placement restrictions are enforced during plan validation: assignments
  requesting `gpu-compute` or `heavy-compute` are refused on machines tagged
  `light`, `login-node`, or `slurm`.
- Documentation reorganized around a single [docs index](docs/README.md);
  working notes from development sessions were removed from the repository root.

### Fixed

- `config/workers.toml` is no longer tracked. It describes a specific fleet,
  including real hostnames and SSH account names; copy
  `config/workers.example.toml` instead.
- The mock planning response in `scripts/check-nvidia.py` omitted the
  `target_machine` field that plan validation requires, failing CI on every run.

### Known limitations

- AGY and OpenCode adapters have not been validated against live
  provider-backed execution; presence in inventory is not proof of working
  authentication.
- A completed, independently verified two-way 1 GiB transfer proof between two
  worker machines has not been recorded.
- Autonomous device and model scheduling is not implemented; the supervisor
  authors and verifies tasks.
- Fine-tuning data collection and export are not implemented.

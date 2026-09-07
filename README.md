# HIVE

HIVE is a self-hosted runtime for coordinating AI agents across local and
SSH-connected machines. It negotiates tasks through HACP contracts, runs agent
CLIs in supervised tmux sessions, checks artifacts against frozen acceptance
criteria, and preserves execution evidence for inspection and recovery.

**HIVE uses [HACP](https://github.com/manvendersingh21/hcap); HACP does not depend
on HIVE.** The protocol is maintained in its own Apache-2.0 repository. HIVE
consumes the `hacp` Rust library through a commit-pinned Git dependency.

This is an early-stage project. Distributed bilateral collaboration has been
tested between two Macs, but autonomous fleet scheduling, peer discovery, and
recursive agent teams are not implemented end to end.

## What works

- A supervisor CLI authors a contract; a worker reviews it and produces artifacts.
  Independent acceptance commands execute on both participating hosts.
- Either role can run locally or on an SSH host, with explicit per-role model
  selection. OpenCode on a Mac mini and AGY on a MacBook Air have completed
  both initial acceptance and a controlled feedback-and-repair exercise.
- A SQLite journal records protocol messages, delivery receipts, invocation
  identities, and results. Inspection and resume reuse recorded work and refuse
  ambiguous launches instead of silently running a second task.
- Bounded negotiation and rework preserve frozen criteria and previous attempts.
  Conversation continuity is implemented for OpenCode and AGY.
- A watchdog can suspend tasks for review. Incidents are persisted and exposed
  through an authenticated web review interface.
- A separate master-agent path provides local-model planning, SSH workers,
  project-scoped memory APIs/CLI commands, and a browser interface.

See [Release 1 evidence](docs/RELEASE-1.md#final-acceptance-audit--complete-2026-09-07)
for exact test scope and limitations. Recorded historical counts include HACP's
tests before the repositories were separated.

## Requirements

Build with stable Rust/Cargo. Python 3 is used by acceptance tests and remote
hosting. Live collaboration requires a Unix-like host with bash and tmux, plus
the selected agent CLI installed and authenticated on each participating device.
The SSH path also needs SSH access and an already trusted host key.

HIVE knows adapters for `opencode`, `agy`, `codex`, and `claude`; CLI support does
not imply a working provider account or equal conversation-resume support.
Agent calls may consume your provider credits. Default tests use fixtures and
do not require model accounts; live tests are opt-in.

## Build

```sh
git clone https://github.com/manvendersingh21/HIVE.git
cd HIVE
cargo build --workspace --locked
cargo test --workspace --locked
```

Cargo fetches HACP from its pinned Git revision. No sibling HACP checkout,
submodule, or personal filesystem path is needed. The first build needs network
access to obtain dependencies; subsequent cached builds can use `--offline`.
The CLI binary is `target/debug/hive`.

## Run a collaboration

Authenticate each agent CLI using its own setup instructions first. Start with
a small task in a disposable workspace: agents execute commands using your
account's permissions. The watchdog is not a process sandbox.

```sh
./target/debug/hive collab run \
  --supervisor opencode \
  --worker agy \
  --task "Implement a Python function that deduplicates strings in input order. Require independent unittest coverage of empty input and duplicates." \
  --run-dir /tmp/hive-collab-example
```

The example uses each CLI's configured model. To place the worker on another
device, configure an SSH alias such as `worker-laptop` in `~/.ssh/config`, verify
access yourself, and add `--worker-host worker-laptop`. Use `--supervisor-model`
and `--worker-model` with model IDs supported by your installations. HIVE does
not automatically discover devices or choose these models for you.

```sh
./target/debug/hive collab inspect /tmp/hive-collab-example
./target/debug/hive collab show /tmp/hive-collab-example
./target/debug/hive collab resume /tmp/hive-collab-example
```

Use a fresh run directory for a new task; use `resume` for an existing run.
See [distributed collaboration](docs/DISTRIBUTED-COLLABORATION.md) for SSH
prerequisites, pause approval, recovery, and failure handling.

## Master agent and web interface

This is a separate workflow from `hive collab`. It uses `config/hive.toml` and
`config/workers.toml` in the chosen project root. The checked-in worker file
records the maintainer's fleet, not a ready-to-use public deployment. For your
own setup, start from [workers.example.toml](config/workers.example.toml) and
configure only machines you own or are authorized to use.

The current local-model configuration names `qwen3.5:9b` and the embedding model
`nomic-embed-text`. If using Ollama, pull the same models you configure:

```sh
ollama pull qwen3.5:9b
ollama pull nomic-embed-text
```

Set `HIVE_WEB_PASSWORD` privately in your environment (at least eight characters),
then start the server from the checkout root:

```sh
HIVE_WEB_ADDR=127.0.0.1:8080 ./target/debug/hive-web
```

Open `http://127.0.0.1:8080`. **The actual bind setting is `HIVE_WEB_ADDR`, not
`web.listen_addr` in TOML.** The default is loopback. This interface exposes
terminal functionality; keep it on a trusted network and do not deploy it as
an unaudited public Internet service. Cloud provider keys are optional and come
from environment variables, never committed configuration.

## Current limitations

- The supervisor authors and verifies the task; it is not yet an autonomous
  device/model scheduler. SSH aliases provide connectivity, not HACP discovery.
- The recursive protocol profile and escalation objects exist in HACP, but
  HIVE does not implement a complete recursive multi-team runtime.
- General chat still follows a command-plan interface rather than a complete
  conversational-answer interface. Browser chat does not currently select a
  project for memory. Do not assume all web chats are remembered.
- Skills loading is not wired into the main application entry points. Some
  parsed configuration keys are not effective runtime settings; do not treat
  their parsing tests as proof that an operational setting takes effect.
- Agent output may be malformed or semantically wrong. Independent checks reduce
  false acceptance but cannot prove arbitrary objectives or prevent every unsafe
  command. Suspended or uncertain tasks may require operator review.

## Repository map

| Component | Purpose |
|---|---|
| `hive-core` | Collaboration runtime, journal, local/SSH hosting, planning, memory, watchdog |
| `hive-cli` | `hive` commands, collaboration inspection and recovery |
| `hive-web` | Authenticated browser UI, terminal bridge, incident review |
| `hive-worker` | Optional authenticated HTTP worker daemon; not required for direct SSH collaboration |
| `hive-adapter` | Legacy HACP/1.1 transport adapter |
| `hive-common` | HIVE configuration and task types |
| `rust_api` | Experimental Rust API scaffold |
| [HACP repository](https://github.com/manvendersingh21/hcap) | External protocol library, specifications, schemas, conformance tests |

## Documentation and contributing

Start with the [documentation index](docs/README.md) and
[CONTRIBUTING.md](CONTRIBUTING.md). Report reproducible bugs or propose features
through [GitHub issues](https://github.com/manvendersingh21/HIVE/issues).
See [SECURITY.md](SECURITY.md) before reporting a vulnerability.

## License

HIVE is licensed under [Apache License 2.0](LICENSE). HACP has its own
[Apache-2.0 license](https://github.com/manvendersingh21/hcap/blob/main/LICENSE).

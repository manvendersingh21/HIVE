# HIVE

[![CI](https://github.com/manvendersingh21/HIVE/actions/workflows/ci.yml/badge.svg)](https://github.com/manvendersingh21/HIVE/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![HACP Protocol](https://img.shields.io/badge/protocol-HACP-blueviolet.svg)](https://github.com/manvendersingh21/hacp)

HIVE is a self-hosted runtime for coordinating AI agents across local and SSH-connected machines. It negotiates tasks through HACP contracts, runs agent CLIs in supervised tmux sessions, checks artifacts against frozen acceptance criteria, and preserves execution evidence for inspection and recovery.

**HIVE uses [HACP](https://github.com/manvendersingh21/hacp); HACP does not depend on HIVE.** The protocol is maintained in its own Apache-2.0 repository. HIVE consumes the `hacp` Rust library through a commit-pinned Git dependency.

> **Project status:** pre-release. No version has been tagged, interfaces may
> change without a deprecation period, and several subsystems are documented as
> unverified — see [Current Limitations](#current-limitations) and the
> [changelog](CHANGELOG.md).

## Features

- **Distributed Collaboration**: Coordinate AI agents across local and SSH-connected machines using HACP contracts
- **Fleet Delegation**: Assign work to a named agent on a named machine, each keeping its own tmux session and native conversation (opt-in via `HIVE_DELEGATION=1`)
- **Supervised Execution**: Run agent CLIs in monitored tmux sessions with safety controls
- **Evidence Preservation**: SQLite journal records all protocol messages, results, and execution history
- **Watchdog Protection**: Safety monitoring with incident review interface
- **Memory System**: Project-scoped knowledge graph and RAG pipeline
- **Saved Chats**: Search, reopen, and continue web conversations stored in SQLite
- **Flexible Placement**: Automatic worker selection based on capabilities and availability

## Prerequisites

- Stable Rust/Cargo
- Python 3 (for acceptance tests and remote hosting)
- Unix-like system with bash and tmux
- Selected agent CLIs installed and authenticated on participating devices
- SSH access for remote collaboration (with trusted host keys)

## Installation

```bash
git clone https://github.com/manvendersingh21/HIVE.git
cd HIVE
cargo build --workspace --locked
```

## Quick Start

### Build and Test
```bash
# Build the entire workspace
cargo build --workspace --locked

# Run all tests
cargo test --workspace --locked

# The CLI binary is available at target/debug/hive
```

### Run a Collaboration
First, authenticate each agent CLI using its own setup instructions. Then run:

```bash
# Start with a small task in a disposable workspace
./target/debug/hive collab run \
  --supervisor opencode \
  --worker agy \
  --task "Implement a Python function that deduplicates strings in input order. Require independent unittest coverage of empty input and duplicates." \
  --run-dir /tmp/hive-collab-example
```

### Master Agent and Web Interface
Configure your project root with `config/hive.toml` and `config/workers.toml`. Start with the example configuration:

```bash
cp config/workers.example.toml config/workers.toml
# Edit config/workers.toml to add your machines
```

This checkout uses Ollama with `qwen3.5:9b` for all master reasoning and
`nomic-embed-text` for memory embeddings. No cloud API keys are required.
Worker agent CLIs still use their own authentication. Keep Ollama running
before starting the CLI or web server, and install both models:
```bash
ollama pull qwen3.5:9b
ollama pull nomic-embed-text
```

Set a secure password and start the web server:
```bash
export HIVE_WEB_PASSWORD="your_secure_password_here"
HIVE_WEB_ADDR=127.0.0.1:8080 ./target/debug/hive-web
```

Visit `http://127.0.0.1:8080` to access the web interface. Use **Chats** to reopen
saved conversations or **New chat** to start another. See [chat history](docs/CHAT-HISTORY.md).

### Available Commands
```bash
# List all available commands
./target/debug/hive --help

# Inspect collaboration results
./target/debug/hive collab inspect /tmp/hive-collab-example

# Show collaboration details
./target/debug/hive collab show /tmp/hive-collab-example

# Resume an existing collaboration
./target/debug/hive collab resume /tmp/hive-collab-example

# Manage skills
./target/debug/hive skills list

# Run local tasks with the master agent
./target/debug/hive task --description "Your task here"
```

## Configuration

Two files in your project root:

- `config/hive.toml` — LLM providers, web settings, database path, skills directory, and memory configuration.
- `config/workers.toml` — your SSH worker machines. This file names real hosts and accounts, so it is **gitignored**; copy the template and edit your copy:

```bash
cp config/workers.example.toml config/workers.toml
```

```toml
# One [[workers]] block per machine
[[workers]]
name = "worker-1"          # how plans and logs refer to this machine
host = "worker-1"          # SSH alias from ~/.ssh/config, or a hostname/IP
user = "your-username"
tags = ["linux", "docker"]

[[workers]]
name = "laptop"
host = "laptop"
user = "your-username"
tags = ["macos", "arm64", "light"]
```

Most tags are free-form context for the planner, but three are enforced:
`light`, `login-node`, and `slurm` mark a machine as an invalid target for
heavy or GPU work, and naming it explicitly does not override that. See
[Worker Placement](docs/PLACEMENT.md).

## Documentation

Full index: [docs/README.md](docs/README.md).

- [Web Agent Workflow](docs/AGENT-WORKFLOW.md): The chat loop — planning, execution, correction, verification
- [Orchestration](docs/ORCHESTRATION.md): Turning a prompt into device and agent assignments
- [Fleet Delegation](docs/FLEET-DELEGATION.md): Native agent sessions, peer messages, approvals, recovery
- [Distributed Collaboration](docs/DISTRIBUTED-COLLABORATION.md): Roles, SSH setup, verification, recovery
- [Worker Placement](docs/PLACEMENT.md): How Hive decides which machine runs what
- [Deployment Guide](docs/DEPLOYMENT.md): Production deployment and security considerations
- [HACP Integration](docs/HACP-HIVE.md): Protocol library and dependency workflow
- [Contributing](CONTRIBUTING.md): Development guidelines
- [Code of Conduct](CODE_OF_CONDUCT.md): Community standards
- [Security](SECURITY.md): Security model and reporting
- [Changelog](CHANGELOG.md): Notable changes on the development branch

## Current Limitations

- The supervisor authors and verifies tasks; autonomous device/model scheduling is not implemented
- SSH aliases provide connectivity, not HACP discovery
- Recursive multi-team runtime is not fully implemented
- General chat follows command-plan interface rather than conversational
- Skills activate at request time after being loaded from `skills.directory` at startup
- Fine-tuning data collection and export are not implemented
- Agent output may be malformed; independent checks reduce false acceptance
- Fleet delegation is opt-in (`HIVE_DELEGATION=1`) and under live validation
- The AGY and OpenCode adapters have not been validated against live provider-backed execution

## Repository Structure

| Component | Purpose |
|-----------|---------|
| `hive-core` | Collaboration runtime, journal, local/SSH hosting, planning, memory, watchdog |
| `hive-cli` | `hive` commands, collaboration inspection and recovery |
| `hive-web` | Authenticated browser UI, terminal bridge, incident review |
| `hive-worker` | Optional authenticated HTTP worker daemon |
| `hive-adapter` | Legacy HACP/1.1 transport adapter |
| `hive-common` | HIVE configuration and task types |
| `rust_api` | Experimental Rust API scaffold |

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the
development checks, the HACP dependency workflow, the policy on live and
destructive tests, and what a pull request should explain. Participation is
governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

Two things worth knowing before you open a PR:

- Protocol semantics, wire schemas, and conformance fixtures belong in the
  [HACP repository](https://github.com/manvendersingh21/hacp), not here.
- A successful build is not validation. Describe the checks you actually ran.

Open an issue to discuss substantial changes before implementing them.

## License

HIVE is licensed under [Apache License 2.0](LICENSE). HACP has its own [Apache-2.0 license](https://github.com/manvendersingh21/hacp/blob/main/LICENSE).

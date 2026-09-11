# HIVE

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![HACP Protocol](https://img.shields.io/badge/protocol-HACP-blueviolet.svg)](https://github.com/manvendersingh21/hacp)

HIVE is a self-hosted runtime for coordinating AI agents across local and SSH-connected machines. It negotiates tasks through HACP contracts, runs agent CLIs in supervised tmux sessions, checks artifacts against frozen acceptance criteria, and preserves execution evidence for inspection and recovery.

**HIVE uses [HACP](https://github.com/manvendersingh21/hacp); HACP does not depend on HIVE.** The protocol is maintained in its own Apache-2.0 repository. HIVE consumes the `hacp` Rust library through a commit-pinned Git dependency.

## Features

- **Distributed Collaboration**: Coordinate AI agents across local and SSH-connected machines using HACP contracts
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

Create `config/hive.toml` and `config/workers.toml` in your project root:

- `config/hive.toml`: Contains LLM providers, web settings, database path, skills directory, and memory configuration
- `config/workers.toml`: Defines SSH worker machines with host, user, and tags for placement decisions

Example worker configuration:
```toml
# Add one [[workers]] block per machine
[[workers]]
name = "worker-1"
host = "your-worker-host"
user = "your-username"
tags = ["gpu", "powerful"]

# Multiple workers can be defined
[[workers]]
name = "worker-2"
host = "another-worker"
user = "another-user"
tags = ["cpu", "light"]
```

## Documentation

- [Distributed Collaboration](docs/DISTRIBUTED-COLLABORATION.md): Roles, SSH setup, verification, recovery
- [HACP Integration](docs/HACP-HIVE.md): Protocol library and dependency workflow
- [Deployment Guide](docs/DEPLOYMENT.md): Production deployment and security considerations
- [Worker Placement](docs/PLACEMENT.md): How Hive decides which machine runs what
- [Roadmap](docs/ROADMAP.md): Current status and future plans
- [Contributing](CONTRIBUTING.md): Development guidelines
- [Security](SECURITY.md): Security model and reporting

## Current Limitations

- The supervisor authors and verifies tasks; autonomous device/model scheduling is not implemented
- SSH aliases provide connectivity, not HACP discovery
- Recursive multi-team runtime is not fully implemented
- General chat follows command-plan interface rather than conversational
- Skills activate at request time after being loaded from `skills.directory` at startup
- Fine-tuning data collection and export are not implemented
- Agent output may be malformed; independent checks reduce false acceptance

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for development guidelines and contribution process.

## License

HIVE is licensed under [Apache License 2.0](LICENSE). HACP has its own [Apache-2.0 license](https://github.com/manvendersingh21/hacp/blob/main/LICENSE).

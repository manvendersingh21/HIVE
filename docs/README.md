# HIVE documentation

Start with the [project README](../README.md) for what HIVE is, how to build it,
and how to run something small.

## Using HIVE

| Guide | What it covers |
|---|---|
| [Web Agent Workflow](AGENT-WORKFLOW.md) | The web chat loop: planning, execution feedback, correction, verification |
| [Saved Chats](CHAT-HISTORY.md) | Searching, reopening, and continuing stored conversations |
| [Orchestration](ORCHESTRATION.md) | Turning a prompt into device and agent assignments with a local model |
| [Fleet Delegation](FLEET-DELEGATION.md) | Native agent sessions, peer messages, approvals, and recovery |
| [Distributed Collaboration](DISTRIBUTED-COLLABORATION.md) | The `hive collab` two-role HACP workflow |
| [Worker Placement](PLACEMENT.md) | How Hive chooses a machine when a plan does not name one |

## Operating HIVE

| Guide | What it covers |
|---|---|
| [Deployment](DEPLOYMENT.md) | Running HIVE across machines, and the security considerations that come with it |
| [Security Model](../SECURITY.md) | Trust boundaries and vulnerability reporting |
| [NVIDIA Provider](NVIDIA.md) | The optional NVIDIA provider, and the migration history behind the current default |

## Architecture and protocol

| Guide | What it covers |
|---|---|
| [HACP Integration](HACP-HIVE.md) | The external protocol library and the dependency workflow |
| [Decision Records](adr/) | Design decisions and the reasoning behind them |

## Contributing

| Guide | What it covers |
|---|---|
| [Contributing](../CONTRIBUTING.md) | Development checks, dependency workflow, live-test policy, PR expectations |
| [Code of Conduct](../CODE_OF_CONDUCT.md) | Community standards and enforcement |
| [Changelog](../CHANGELOG.md) | Notable changes on the development branch |

## Project history

| Record | What it covers |
|---|---|
| [Roadmap](ROADMAP.md) | The original ten-phase plan and where each phase stands |
| [Findings](findings/) | The adapter-edge findings cited by the runtime's own doc comments |

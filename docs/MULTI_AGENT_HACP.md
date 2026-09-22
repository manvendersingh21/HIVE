# Multi-agent HACP workspaces

## Vision

HACP can grow from a two-agent bilateral protocol into a persistent multi-agent
workspace. A user could start four or five agents for one project, see what each
agent is responsible for, and let them collaborate dynamically when their work
requires it.

Each agent would have a project-scoped profile describing its role, model,
capabilities, current task, owned paths, dependencies, and status. Agents would
know the relevant profiles of their teammates and could negotiate task
distribution, interfaces, handoffs, and reviews through HACP.

The profile should be durable project metadata rather than context that the
model is expected to remember indefinitely. The coordinator can provide each
agent with a compact, relevant view of the team state while retaining the full
record in the workspace.

## Agent profiles

An agent profile could have this shape:

```json
{
  "agent_id": "frontend-a",
  "role": "frontend",
  "model": "codex",
  "capabilities": ["typescript", "react", "playwright"],
  "owned_paths": ["hive-web/frontend/**"],
  "current_task": "Add master-agent settings UI",
  "status": "working",
  "dependencies": ["backend-b"],
  "last_seen": "2026-09-19T04:00:00Z"
}
```

The profile should not contain API keys or unnecessary private context. It
should expose enough information for other agents to understand ownership,
capabilities, dependencies, and availability.

## Team session model

A team session would contain:

- A roster of agent profiles and heartbeats.
- A task graph with owners, dependencies, file paths, and acceptance checks.
- File and interface leases that prevent conflicting edits.
- Direct messages, broadcasts, questions, proposals, and review requests.
- A shared event log for task changes, completions, blockers, and handoffs.
- Compact summaries of completed work for context recovery.

The existing bilateral HACP contract should remain the safety boundary for a
concrete implementation task. The team layer coordinates many agents, while
each task still declares explicit inputs, outputs, ownership, and acceptance
commands.

```text
Team session
├── Agent profile: frontend
│   └── Contract: settings UI
├── Agent profile: backend
│   └── Contract: provider API
└── Agent profile: tester
    └── Contract: live integration verification
```

## Communication policy

Agents should communicate when an event requires another agent’s attention:

- A dependency becomes available.
- An interface or file ownership conflict appears.
- An agent is blocked.
- A proposed change affects another agent’s task.
- A review or verification is needed.
- The user explicitly requests coordination.

Agents should not send conversational updates merely to prove that they are
active. Structured heartbeats and status events can provide that information
without adding noise.

## Example workflow

1. The coordinator decomposes the user’s request into frontend, backend, and
   verification tasks.
2. Agents publish profiles and claim tasks according to their capabilities.
3. Each task receives an explicit HACP contract with paths and acceptance
   checks.
4. The frontend agent requests the backend endpoint shape if it is a
   dependency.
5. The backend agent publishes the schema and implements its contract.
6. Frontend and backend agents work independently while receiving relevant
   completion and interface events.
7. A tester verifies the integrated behavior against the shared contract.
8. The coordinator reports the combined result and any unresolved follow-up.

## Risks and safeguards

The design needs mechanical safeguards for profile drift, file overlap,
excessive communication, and conflicting decisions:

- Profiles need explicit updates and timestamps.
- Ownership must be enforced by contracts and leases.
- Messages should be scoped to relevant agents and triggered by events.
- Frozen outputs must not be silently overwritten.
- The coordinator should resolve conflicts using task priority and dependency
  order.
- Every agent should see the team manifest, while receiving only the context
  relevant to its work.

## Suggested implementation stages

### Stage 1: team roster and task map

Support one team session with three to five agents, persistent profiles, task
assignments, file ownership, direct messages, dependency notifications, and a
dashboard showing each agent’s status.

### Stage 2: coordination and recovery

Add event-triggered communication, task handoffs, compact context summaries,
heartbeats, reconnect handling, and a user-facing view of the task graph.

### Stage 3: negotiation and reassignment

Allow agents to propose task splits, negotiate interfaces and dependencies,
request replacements when blocked, and have the coordinator reassign work
without losing contract history.

The result would be a persistent multi-agent workspace: HACP sessions provide
coordination units, agent profiles provide durable team awareness, and HACP
contracts protect actual code changes.

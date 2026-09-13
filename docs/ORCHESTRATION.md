# Orchestration with a local model

Hive's coordinator reasons with whichever provider `config/hive.toml` selects.
With `single_provider = "local"` that provider is Ollama, so the model deciding
*which machine runs what* is one you host yourself. This page describes how a
prompt becomes assignments on specific devices, what Hive checks before anything
runs, and what is still under validation.

Delegation is opt-in. Start `hive-web` with `HIVE_DELEGATION=1`; without it,
requests follow the ordinary single-machine agent workflow described in
[Web Agent Workflow](AGENT-WORKFLOW.md).

## The path from prompt to assignment

1. A request arrives through the web chat (`POST /api/chat`). There is no
   separate delegation endpoint — delegation is a mode of the chat interface.
2. The coordinator refreshes the machine graph and the per-device agent records,
   then hands the planner a description of the fleet: each machine's name, OS,
   tags, and the agents discovered on it with their authentication state and
   available models.
3. The model returns a `DelegationPlan` — a summary plus a list of assignments.
4. Hive validates the plan (see below) and rejects it as a whole if any
   assignment fails. Nothing launches from a partially valid plan.
5. Accepted assignments launch over SSH, each in its own tmux session with its
   own durable journal.

## Assignment schema

Each assignment is one agent, on one device, with one objective:

```json
{
  "summary": "Preprocess the dataset on the GPU node, then serve it from the API host",
  "assignments": [
    {
      "key": "preprocess",
      "device": "gpu-node",
      "agent": "claude",
      "model": null,
      "workspace": "~/hive-workspaces/pipeline/preprocess",
      "objective": "Write and run a preprocessing script over data/raw, emitting validated CSV",
      "dependencies": [],
      "acceptance_criteria": [
        "data/clean.csv exists and parses as CSV",
        "row count is reported and non-zero"
      ],
      "required_capabilities": ["gpu-compute"]
    },
    {
      "key": "serve",
      "device": "worker-1",
      "agent": "codex",
      "model": null,
      "workspace": "~/hive-workspaces/pipeline/api",
      "objective": "Expose the preprocessed data through the existing HTTP service",
      "dependencies": ["preprocess"],
      "acceptance_criteria": ["GET /pipeline returns 200 with a non-empty body"],
      "required_capabilities": []
    }
  ]
}
```

`device` must name a machine from `config/workers.toml`. `agent` must be one of
`claude`, `codex`, `agy`, `opencode`. `model` is optional; `null` lets the agent
use its own default. `dependencies` reference other assignments by `key`, and an
assignment waits until the ones it names have finished.

## What Hive checks before launching

Validation is deterministic and runs before any side effect:

- At most 16 assignments per plan, with unique `key` values.
- `device` resolves to a configured worker; `agent` is one of the four supported
  native interfaces.
- `workspace` is a child of `~/hive-workspaces/`, with no `.`, `..`, empty
  segments, or control characters.
- `objective` is non-empty and at least one acceptance criterion is present — an
  assignment with nothing to verify against is rejected.
- Placement restrictions hold. An assignment requesting `gpu-compute` or
  `heavy-compute` is refused on any machine tagged `light`, `login-node`, or
  `slurm`. Laptops someone may be using, bastion hosts, and scheduler-managed
  nodes are not valid targets for sustained work, and naming one explicitly does
  not override this.

A device that is configured but missing the requested executable, runtime, or
authentication becomes `needs-setup` rather than silently rerouting elsewhere.
Hive never substitutes a different machine for the one the plan named.

## Coordinating between assignments

Assignments exchange task-scoped peer messages — questions, answers, and
agreements — each with a stable delivery ID and an acknowledgment. This is how
two agents settle a shared interface (a schema, a file format, a port) without
the coordinator relaying every detail by hand. Guidance sent from the web UI
joins the same native conversation; a busy adapter delivers it at the next turn
boundary.

The coordinator can review runs that have finished or are waiting and send
follow-up work into those same conversations. Native completion on its own does
not mean the acceptance criteria passed — that judgment stays with the
verification step.

## Observing and steering a run

| Endpoint | Purpose |
|---|---|
| `GET /api/runs` | Every run with its device, agent, model, and state |
| `GET /api/runs/{id}/events?after={seq}` | Incremental event stream for one run |
| `POST /api/runs/{id}/messages` | Send guidance into the native conversation |
| `POST /api/runs/{id}/decisions` | Approve or refuse a pending action |
| `POST /api/runs/{id}/replace` | Replace a run's assignment |
| `POST /api/runs/{id}/retry-setup` | Re-probe a device stuck in `needs-setup` |
| `GET /api/runs/{id}/recovery` | Fetch the recovery snapshot for a broken run |
| `POST /api/runs/{id}/recovery` | Acknowledge uncertain deliveries and resume |

All of these require an authenticated session; see
[Deployment](DEPLOYMENT.md).

Example — follow one run's events:

```bash
curl -s http://127.0.0.1:8080/api/runs \
  | jq '.[] | {id, device, agent, state}'

curl -s "http://127.0.0.1:8080/api/runs/$RUN_ID/events?after=0" \
  | jq '.[] | {seq, kind}'
```

## Running it

```bash
# 1. Ollama must be reachable at the base_url in config/hive.toml
ollama serve &
ollama pull qwen3.5:9b
ollama pull nomic-embed-text

# 2. Describe your machines
cp config/workers.example.toml config/workers.toml
$EDITOR config/workers.toml

# 3. Start the web server with delegation enabled
export HIVE_WEB_PASSWORD="choose-a-real-password"
HIVE_DELEGATION=1 HIVE_WEB_ADDR=127.0.0.1:8080 ./target/debug/hive-web
```

Then open `http://127.0.0.1:8080` and describe the work. Naming a device and
agent explicitly — "Claude on gpu-node" — is the form the planner handles most
reliably; see the caveat below.

## Current limitations

These are real constraints, not planned work that happens to be undone:

- **Explicit pairs are the reliable form.** Canonical phrases such as
  `Claude on gpu-node` are enforced against the fleet. Phrasing outside that
  shape still depends on how the planner interprets it; there is no general
  natural-language constraint parser.
- **AGY and OpenCode need live validation.** Both adapters are implemented and
  appear in inventory, but provider-backed execution through them has not been
  verified end to end. Presence in inventory is not proof of working
  authentication or model availability.
- **Model quality bounds plan quality.** A small local model will produce weaker
  assignments than a large one. Validation catches malformed and unsafe plans,
  not merely unwise ones.
- **Cross-device networking is your responsibility.** Hive reaches every machine
  over SSH from the coordinator. Two workers that cannot route to each other
  cannot open a direct connection between themselves just because both appear in
  the same plan.

## See also

- [Fleet Delegation](FLEET-DELEGATION.md) — sessions, approvals, and recovery in
  detail
- [Worker Placement](PLACEMENT.md) — how Hive chooses a machine when the plan
  does not name one
- [Distributed Collaboration](DISTRIBUTED-COLLABORATION.md) — the HACP-negotiated
  two-device workflow

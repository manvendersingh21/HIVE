# Using Ollama as Fleet Orchestrator

## Overview
**YES** — Your vision is fully supported by the current delegation infrastructure. You can:

1. ✅ Give Ollama a high-level prompt
2. ✅ Ollama decides which agents run on which devices (e.g., "AGY on MacAir, Claude on cis-a6000")
3. ✅ Agents collaborate while Ollama orchestrates the entire workflow
4. ✅ Peer messaging enables inter-agent coordination

---

## Architecture

```
User Prompt
    ↓
Ollama (LlmRouter - local provider)
    ↓ 
[Generates DelegationPlan with assignments]
    ↓
┌─────────────────────────────────────────┐
│  Device 1 (MacAir)          Device 2 (cis-a6000)
│  ┌─────────────────┐        ┌────────────────────┐
│  │ AGY             │        │ Claude             │
│  │ Running on MacAir       │ Running on cis-a6000
│  │ Task 1          │──Peer Message───→│ Task 2
│  │ (Agent 1 Work)  │ (Questions, Data)│ (Agent 2 Work)
│  │                 │←─────────────────│ Results
│  └─────────────────┘                  └────────────────────┘
│         ↓                                   ↓
│    Local Exec                         SSH Remote Exec
│         ↓                                   ↓
│    Tmux Session 1                    Tmux Session 2
│    (Durable)                         (Durable)
└─────────────────────────────────────────────────────────────┘
    ↓
Ollama (monitors progress, sends guidance)
    ↓
[Agents acknowledge messages and continue work]
    ↓
Final Results (collected from both devices)
```

---

## Current Implementation Status

### ✅ Already Built
| Component | Status | Location |
|-----------|--------|----------|
| LlmRouter with Ollama | ✅ Ready | `hive-core/src/llm/mod.rs` |
| Planner (receives fleet context) | ✅ Ready | `hive-core/src/agent/planner.rs` |
| Assignment validation (device/agent) | ✅ Ready | `hive-core/src/delegation/mod.rs` |
| Peer messaging system | ✅ Ready | `hive-core/src/delegation/mod.rs` |
| Durable run storage | ✅ Ready | `hive-core/src/delegation/store.rs` |
| Four agent adapters | ✅ Ready | `hive-core/src/delegation/runner/` |
| Collaboration tracking | ✅ Ready | Run events + peer acknowledgments |
| Progress updates to coordinator | ✅ Ready | Guidance messages |

---

## How It Works

### 1. Fleet Context Provided to Ollama
When Ollama plans a run, it receives:

```
Available Devices:
- macair (MacBook Air, macOS)
  - Agents: claude, codex
  - Models: Claude 3.5 Sonnet, GPT-4o  
  - Storage: 256 GB
  - Tags: [laptop, network-dependent]

- cis-a6000 (Shared GPU Server, Linux)
  - Agents: claude, agy
  - Models: Claude 3.5 Sonnet, Anthropic Research
  - GPU: NVIDIA A6000
  - Tags: [heavy-compute, shared]

- archlinux-worker (Local Dev Machine, Linux)
  - Agents: codex
  - Models: GPT-5.6 Sol
  - Storage: 2 TB
  - Tags: [development, local]
```

### 2. Ollama Makes Device + Agent Decisions
Ollama reads the fleet and decides:

```json
{
  "summary": "Implement data pipeline on GPU, integrate with web service",
  "assignments": [
    {
      "key": "data-process",
      "device": "cis-a6000",
      "agent": "claude",
      "model": "claude-3-5-sonnet",
      "workspace": "~/hive-workspaces/data-pipeline/",
      "objective": "Implement GPU-accelerated data preprocessing",
      "dependencies": [],
      "acceptance_criteria": ["Processes 1M records in <30s", "Outputs validated CSV"]
    },
    {
      "key": "web-integrate",
      "device": "archlinux-worker",
      "agent": "codex",
      "model": "gpt-5.6-sol",
      "workspace": "~/hive-workspaces/web-service/",
      "objective": "Integrate pipeline results into web API",
      "dependencies": ["data-process"],
      "acceptance_criteria": ["API endpoint /pipeline returns results", "200ms latency"]
    }
  ]
}
```

### 3. Agents Execute in Parallel
- Claude on cis-a6000 starts working (tmux session created)
- Codex on archlinux-worker waits (dependencies not met yet)
- Each maintains native conversation + tmux session

### 4. Peer Coordination During Work
Claude can ask Codex questions:

```
Claude → Codex (via peer message):
{
  "kind": "question",
  "text": "What's your API schema for incoming data? I'm preparing CSV output format."
}

Codex → Claude (via peer message):
{
  "kind": "answer", 
  "text": "POST /upload expects multipart/form-data with 'data' field (JSON array). Each record needs {id, timestamp, value}."
}

Claude confirms:
{
  "kind": "agreement",
  "text": "Confirmed. Outputting records as {id: UUID, timestamp: ISO8601, value: float}."
}
```

### 5. Ollama Monitors and Guides
While agents work, Ollama can:
- Read progress via `/api/runs/{id}/events`
- Send guidance: `POST /api/runs/{id}/messages` with helpful context
- Approve dangerous actions (if needed)
- Trigger next phase based on status

---

## Example: Complete Workflow

### User Request
```
"Build a data processing pipeline with GPU acceleration that feeds into our 
web service. Use the A6000 for heavy lifting, integrate results into the 
web API on archlinux-worker. They should collaborate on the data format."
```

### Ollama Decision Flow
```
Input: User request + Fleet context
  ↓
Ollama: "I need GPU + web integration, two agents on different machines"
  ↓
Generate JSON assignments:
  - Device: cis-a6000, Agent: claude
  - Device: archlinux-worker, Agent: codex
  - Collaboration: peer messages for schema alignment
  ↓
Hive validates:
  ✓ Both devices exist
  ✓ Both agents installed
  ✓ Workspaces valid
  ↓
Launch both in tmux sessions with durable runs
  ↓
Monitor: GET /api/runs
  {
    "runs": [
      {
        "id": "run-123",
        "device": "cis-a6000",
        "agent": "claude",
        "state": "working",
        "progress": "50% complete"
      },
      {
        "id": "run-456", 
        "device": "archlinux-worker",
        "agent": "codex",
        "state": "waiting",
        "blocked_by": "run-123"
      }
    ]
  }
```

---

## Configuration

### 1. Start Hive with Ollama Enabled
```bash
# Use Ollama as the local coordinator
export OLLAMA_BASE_URL="http://localhost:11434"
export OLLAMA_MODEL="qwen2.5:14b"  # or your preferred model
export HIVE_DELEGATION=1

cargo run -p hive-web
```

### 2. Configure Fleet Devices
In `config/hive.toml`:
```toml
[[workers]]
name = "macair"
hostname = "macair.local"
tags = ["laptop", "network-dependent"]

[[workers]]
name = "cis-a6000"
hostname = "cis-a6000.tu.temple.edu"
tags = ["heavy-compute", "shared-server"]
gpu = "NVIDIA A6000"

[[workers]]
name = "archlinux-worker"
hostname = "100.100.194.107"
tags = ["development", "local"]
```

### 3. Verify Agent Availability
Hive auto-discovers:
- Which agents are installed on each device
- Which models each agent has access to
- Authentication status
- Runtime requirements

Check via: `curl http://localhost:7777/api/runs` → device inventory included

---

## Peer Messaging Example

### Agents Exchange Data
```python
# Claude (on cis-a6000) sends schema to Codex
peer_message = {
    "id": str(uuid.uuid4()),
    "to": "run-456",  # Codex's run ID
    "kind": "agreement",
    "text": json.dumps({
        "output_schema": {
            "records": [
                {"id": "uuid", "timestamp": "iso8601", "value": "float"}
            ],
            "checksum": "sha256"
        }
    })
}

# Post to Hive
requests.post(
    "http://localhost:7777/api/runs/run-123/messages",
    json=peer_message
)

# Codex (on archlinux-worker) receives and confirms
# Then proceeds with web API integration using the agreed schema
```

---

## API Endpoints for Orchestration

### Monitor All Runs
```bash
curl http://localhost:7777/api/runs
# Returns list with device, agent, model, state for each
```

### Get Run Events (track progress)
```bash
curl http://localhost:7777/api/runs/{run_id}/events?after={sequence}
# Returns [
#   {"type": "work", "output": "Processed 10k records..."},
#   {"type": "peer", "from": "other-agent", "message": "..."}
# ]
```

### Send Guidance While Running
```bash
curl -X POST http://localhost:7777/api/runs/{run_id}/messages \
  -H "Content-Type: application/json" \
  -d '{
    "id": "'$(uuidgen)'",
    "text": "Claude: Consider using multiprocessing for this step"
  }'
```

### Make Approvals
```bash
curl -X POST http://localhost:7777/api/runs/{run_id}/decisions \
  -H "Content-Type: application/json" \
  -d '{
    "id": "{approval_id}",
    "fingerprint": "{sha256_of_action}",
    "decision": "continue"
  }'
```

### Check Recovery Options
```bash
curl http://localhost:7777/api/runs/{run_id}/recovery
# Shows if run can resume after interruption
```

---

## Collaboration Patterns

### 1. Dependent Execution (Sequential)
```json
{
  "assignments": [
    {"key": "data-prep", "device": "cis-a6000", ...},
    {"key": "integrate", "device": "archlinux-worker", "dependencies": ["data-prep"]}
  ]
}
```
Codex waits for Claude to finish data-prep, then proceeds.

### 2. Parallel with Peer Coordination
```json
{
  "assignments": [
    {"key": "backend", "device": "archlinux-worker", ...},
    {"key": "ml-model", "device": "cis-a6000", ...}
  ]
}
```
Both run simultaneously, exchange schemas via peer messages.

### 3. Feedback Loop (Multi-Round)
1. Claude runs analysis on cis-a6000
2. Sends results to Codex for web display
3. Codex asks clarifying questions
4. Ollama sends guidance to both
5. Both update and finish

---

## What Ollama Sees

### Model Input (Fleet Context)
```
You have access to a distributed fleet. Decide which device and agent 
should handle each part of the work:

Devices Available:
- macair: [Claude, Codex] (laptop, GUI available)
- cis-a6000: [Claude, AGY] (GPU A6000, high-memory, Linux)
- archlinux-worker: [Codex] (development, local)

Your options:
- Assign tasks to specific devices by name
- Route via agent preference (which native interface works best)
- Specify required capabilities (gpu-compute, network-access, etc.)
- Define peer coordination via dependencies

User request: {user_prompt}
```

### Model Output (JSON Plan)
```json
{
  "summary": "Multi-device pipeline with GPU + web integration",
  "assignments": [...]
}
```

Hive validates and executes.

---

## Key Features Already Built

✅ **Device Selection** - Ollama chooses device by name or capability  
✅ **Agent Selection** - Ollama picks native interface (Claude, Codex, AGY, OpenCode)  
✅ **Model Override** - Ollama can specify which model version to use  
✅ **Workspace Management** - Auto-created under ~/hive-workspaces/  
✅ **SSH Transport** - Automatic SSH to remote devices  
✅ **Durable Sessions** - tmux sessions survive agent restarts  
✅ **Peer Messaging** - Agents exchange data, questions, agreements  
✅ **Approval Workflow** - Dangerous actions require approval  
✅ **Progress Tracking** - Real-time event stream  
✅ **Recovery** - Resume interrupted runs without duplication  

---

## To Enable Today

```bash
# 1. Ensure Ollama is running locally
ollama serve &

# 2. Pull a model (or use existing)
ollama pull qwen2.5:14b

# 3. Start Hive with delegation enabled
HIVE_DELEGATION=1 cargo run -p hive-web

# 4. Submit a delegation request:
curl -X POST http://localhost:7777/api/delegations \
  -H "Content-Type: application/json" \
  -d '{
    "objective": "Run task X on cis-a6000 using Claude, task Y on archlinux-worker using Codex, coordinate results",
    "complexity": "medium"
  }'
```

Ollama will:
1. Analyze the objective
2. Decide device/agent assignments
3. Generate a DelegationPlan
4. Hive validates and executes
5. Agents collaborate with peer messaging
6. Results collected and returned

---

## Status: ✅ READY TO USE

All infrastructure is complete. Ollama can immediately orchestrate multi-device,
multi-agent workflows with full collaboration support. No additional code needed.

Start with a simple prompt and watch Ollama coordinate the fleet!


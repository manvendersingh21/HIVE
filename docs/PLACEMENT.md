# Placement — how Hive decides which machine runs what

Hive's headline claim is that a knowledge graph decides where work runs. This
is the mechanism behind that claim: what the graph stores, how a request turns
into a machine, and where the decision can still go wrong.

With one worker any of this is overkill. The point is that the second and third
machine cost nothing — and the moment a fleet is heterogeneous (a GPU node, a
shared login box, a laptop), "which machine?" stops having an obvious answer.

---

## 1. Probing: what the graph knows

Every machine — the master included — is probed by one portable shell script
(`memory::machines::probe_script`) that runs on both Linux and macOS and emits
`key=value` lines. It is deliberately tolerant: every lookup falls back to empty
rather than failing, because a machine that answers half the questions is still
worth having in the graph.

Probes run on a **60-second timer** in `hive-web`
(`WORKER_REFRESH_INTERVAL`), not at startup. Startup used to await them, and one
wedged SSH connection could hang the master before it bound its listener. Each
remote probe is capped at 20s (`machines::PROBE_TIMEOUT`) and each health check
at 15s (`workers::HEALTH_PROBE_TIMEOUT`), so a single unreachable host cannot
stall the fleet.

Facts collected: OS and version, kernel, arch, cores, total **and available**
memory, free disk, GPUs (model, memory, count), batch scheduler, and which of
`PROBED_TOOLS` are on the login-shell `PATH`.

> The probe runs through `bash -lc`. On several machines `~/.local/bin` and
> `~/.cargo/bin` are only on the login-shell path, so a plain `ssh host command`
> sees a strictly smaller toolset than Hive does.

### Projection into the graph

```
machine:cis-a6000 ──runs_os─────────► os:ubuntu-22.04
                  ──has_arch────────► arch:x86_64
                  ──has_tool────────► tool:nvcc, tool:sbatch, tool:claude, …
                  ──has_capability──► capability:gpu-compute, capability:batch-scheduler, …
```

Entity ids are `kind:name`, so re-probing updates in place rather than
duplicating. A successful probe replaces a machine's relations wholesale, which
is how uninstalled tools disappear.

**A failed probe does not erase what we knew.** An unreachable machine still has
an OS, a core count and a toolset; a probe that could not connect learned
nothing about them. Overwriting them with zeros turns "temporarily offline" into
"unknown machine" — exactly backwards for a graph meant to answer *which
computer should run this*, where the useful fact is that the box currently down
is the one with `claude` on it. An unreachable probe updates only reachability
and the timestamp.

Machines that leave `workers.toml` are pruned on the next refresh, so a
decommissioned host does not linger as a permanently-offline option.

---

## 2. Capabilities: the unit of placement

Tools are facts; **capabilities are what a planner can reason about**. Asking
"which machine can run an agentic CLI?" is more useful than "which has `codex`?"

| Capability | Inferred from |
|:---|:---|
| `agentic-cli` | `claude` or `codex` |
| `local-inference` | `ollama` |
| `gpu-compute` | `nvidia-smi` |
| `batch-scheduler` | `sbatch` or `srun` |
| `containers` | `docker` |
| `build` | `cargo`, `node` or `python3` |
| `database` | `psql` |
| `supervised-sessions` | `tmux` |

`supervised-sessions` is the **baseline** (`agent::BASELINE_CAPABILITY`): every
supervised remote subtask needs it, whatever the work is. Anything a caller asks
for *beyond* the baseline came from the planner because the task genuinely needs
it — and is therefore never substituted away (§4).

---

## 3. From request to machine

```
user request
   → planner emits subtasks, each with required_capabilities
   → choose_worker(baseline + required)         ── graph query
   → machines_with_capabilities()  filter, then rank
   → plan names one machine
   → execute_run delegates to that machine, or explains why it cannot
```

### The planner has to ask

`required_capabilities` is what connects the graph to a real decision. Without
it every remote subtask requested the same baseline, so a CUDA job and a
`wc -l` routed identically — the graph knew which box had the A6000s and was
never asked.

Getting the local model to populate it reliably needed a **worked table**, not a
rule. Measured on `qwen3.5:9b` over the same cases:

| Prompt style | Correct |
|:---|---:|
| Prose rule ("set it only when the work genuinely needs it…") | 11/15 |
| Worked `command → capability` table | **14/14** |

This is the same lesson as the OS trailer in [`STATUS.md`](STATUS.md): with a 9B
planner, examples beat general instructions, consistently and by a lot.

### Ranking

Candidates are filtered to machines holding **every** requested capability, then
sorted by:

1. **Dedicated before shared.** A machine tagged `shared` or `login-node` in
   `workers.toml` sorts last regardless of size. Otherwise adding one big shared
   host silently redirects *all* default work onto it — a 251 GB university node
   with 30 other users would outrank a quiet 11 GB box we own. Shared
   infrastructure should be chosen when its capabilities are asked for, not
   because it is the largest thing available.
2. **Free memory**, falling back to total where a probe did not report it.
   Total is the wrong key on shared hosts: a login node can advertise 15 GB
   while 11 GB belongs to other people, and ranking on the advertised figure
   sends work to the busiest machine precisely because it is the largest.
3. **Cores**, to break ties.

Unreachable machines are excluded entirely.

---

## 4. Never substitute a machine silently

Both selection points used to fall back to "any least-loaded worker" when they
could not satisfy a request. That is right for an ordinary command — the graph
may simply be unseeded — and wrong the moment the caller asked for something
specific. Running a CUDA job on a box with no GPU fails in a far more confusing
way than being told no machine has `gpu-compute`.

- `choose_worker` returns `None` if anything beyond the baseline was requested
  and nothing matches.
- `execute_run` treats the planned machine's name as a **decision, not a hint**:
  it delegates there or reports why it cannot. Only an unnamed target — the
  graph had no opinion — falls back to least-loaded.

Verified end to end: three consecutive GPU requests all routed to `cis-a6000`,
while a generic remote request went to `archlinux-worker`.

---

## 5. What this does not do yet

- **Nothing enforces the scheduler.** `cis-a6000` is a shared university node
  with SLURM and ~30 concurrent users. The graph records `batch-scheduler`, but
  Hive still delegates over direct SSH+tmux. Heavy or long GPU work belongs in
  an `sbatch` job rather than a session started behind the scheduler's back;
  routing `gpu-compute` work through the scheduler is unbuilt.
- **Ranking ignores current load.** Free memory and cores are static-ish facts
  refreshed every 60s; `active_tasks` is tracked per worker but not part of the
  sort.
- **No affinity or data locality.** A task needing a dataset that lives on one
  machine has no way to say so.
- **Capabilities are coarse.** `gpu-compute` does not distinguish an A6000 from
  an integrated Intel GPU, and says nothing about free VRAM.

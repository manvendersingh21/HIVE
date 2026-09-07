# Distributed collaboration

`hive collab` hosts one HACP/2.0 bilateral collaboration. The operator selects
two CLI roles and their locations. The supervisor authors terms and checks work;
the worker reviews terms, implements, and repairs after feedback. The host uses
the external HACP library for protocol transitions.

```text
operator chooses roles, hosts, models
  -> propose / review / freeze contract
  -> worker produces artifacts
  -> frozen acceptance commands execute on both hosts
  -> verification: accept, bounded rework, or terminal failure
```

## Prepare the devices

On each participating device, install the chosen agent CLI, authenticate it using
its own instructions, and confirm it can execute a small task. Both devices need
bash and tmux; remote hosting also needs Python 3. Model inventory alone does
not prove the account can execute a task.

Use an SSH alias from your own `~/.ssh/config`. For example, with an alias named
`worker-laptop`, check the already trusted connection and login-shell environment:

```sh
ssh -o BatchMode=yes -o StrictHostKeyChecking=yes worker-laptop \
  'bash -lc "command -v bash; command -v tmux; command -v python3; command -v agy"'
```

Each prerequisite must be present. Do not disable host-key checking to bypass
an SSH failure. Configure trust yourself before running HIVE. The remote worker
does not need a HIVE checkout or `hive-worker` daemon for this SSH hosting path.

## Run

```sh
cargo build --workspace --locked
./target/debug/hive collab run \
  --supervisor opencode \
  --worker agy \
  --worker-host worker-laptop \
  --max-rework 2 \
  --timeout-secs 600 \
  --run-dir /tmp/hive-distributed-example \
  --task "Implement a small Python parser with independent unittest coverage of valid input, empty input, and malformed input."
```

Omit a role's `--*-host` option to run it locally. Use `--supervisor-host` to
place the supervisor remotely. Optional `--supervisor-model` and `--worker-model`
pass explicit selections to the relevant adapter; use IDs accepted by that CLI.
This is explicit placement, not autonomous discovery or scheduling.

Remote work is hosted in an isolated run directory under `/tmp/hive-collab-*`.
Artifact transfers are digest-checked; that directory separation is not a process
sandbox. Agent CLIs may use non-interactive permission modes and retain provider
history outside the run directory. Protect both the task workspace and account.

## Inspect and recover

```sh
./target/debug/hive collab inspect /tmp/hive-distributed-example
./target/debug/hive collab show /tmp/hive-distributed-example
./target/debug/hive collab resume /tmp/hive-distributed-example
```

The journal records intents before launch, protocol deliveries, receipts, and
results. Completed work is replayed; uncertain existing work is reattached when
host evidence supports it. An unresolved launch is refused rather than assumed
safe to duplicate. A disconnected observation channel is not proof of task exit.

If an invocation is suspended, inspect its logs and tmux session first. Approval
requires the exact recorded invocation name and an explicit audit reason:

```sh
./target/debug/hive collab resume /tmp/hive-distributed-example \
  --continue-session RECORDED_INVOCATION_NAME \
  --reason "Reviewed the recorded output and approved continuing this invocation"
```

This authorizes the reviewed output for that invocation, not all future output
or all tasks. Do not use continuation merely to dismiss an unexplained warning.

## Verification and limits

Frozen verification fixtures are supplied before implementation; required checks
run on both devices. Missing artifacts, changed digests, and failing acceptance
checks prevent acceptance despite a successful CLI exit or positive model prose.
Rework is bounded (`--max-rework` accepts 0–5) and retains previous attempts.

OpenCode and AGY preserve conversations across stages. The other adapters do not
have equivalent continuity guarantees. A finite suite cannot establish arbitrary
task correctness, and the author may omit a requirement from its contract.

The [Release 1 audit](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07)
records real mini/Air acceptance, controlled repair, and explicit recovery tests.
These are dated tests, not a guarantee that every provider, OS, or CLI version
has been exercised. No automatic peer discovery or recursive team scheduler is
implemented by this workflow.

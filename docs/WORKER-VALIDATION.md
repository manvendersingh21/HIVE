# Worker checks — 2026-09-10

The named-machine capability rejection was a placement bug. A named target now
selects that exact configured worker; model-generated capability hints cannot
reject it because the graph is stale, empty, or the requirement is irrelevant.
Automatic placement (`target_machine: "auto"`) still matches required capabilities.
Unknown worker names fail before execution; offline workers are never replaced.
Worker readiness checks and watchdog interception remain in place.

Local plan generation now sends a JSON schema to Ollama, including required
machine destinations. This addresses invalid JSON escapes in model-generated
shell scripts. Classification remains plain text. Structured syntax does not
establish that generated commands implement the requested application. A dry run of
the original request passed placement for the Air and Arch but still proposed an
HTTP server instead of the requested WebSocket service and included faulty shell
quoting. Those commands were not executed. The earlier file-sharing service below
was implemented separately and is **not evidence that Hive can implement the
request itself**.
See [Ollama structured outputs](https://docs.ollama.com/capabilities/structured-outputs).

The subsequent [web agent loop](AGENT-WORKFLOW.md) now passes actual results back
to Ollama for correction and continued implementation. A real Ollama web-API
test created `hive-ollama-tmux-proof-v2` on the Air and Arch, recovered from a
failed verification, and confirmed both remote hostnames. Destination checks
were then tightened to prevent continuation onto unrelated machines. The
automated suite also covers failed commands, dependent-step suppression,
structured file writes, premature completion, and verification on every target.

The live website also created `hive-ollama-ws-proof` on both requested workers;
the production Sessions API confirmed both. The larger application attempt
did not produce a verified WebSocket service: Qwen repeated directory checks,
generated incorrect network probes, and hit the original planning deadline.
The run was stopped with its observations preserved. Follow-up fixes add a
bounded planning retry, rejection of identical successful rounds, and shell
pipeline failure propagation. This remains a limitation of the larger live
test, not a passed application implementation.

The focused live website test subsequently generated and ran `agent-proof.py`
on both workers, recovering from an invented initial home path. It exposed and
fixed a remote-wrapper heredoc bug and a finalization loop. The final website
verification completed in two rounds and reported the actual Air and Arch
hostnames. The production Sessions API independently confirmed that both
`hive-ollama-ws-proof` sessions remained available. No workload code in this test
was installed by the test harness; Hive generated and executed it.

Final validation: workspace build and tests (454 passed, 16 ignored plus one
ignored doc test), web history/browser regression, and `git diff --check` passed.

The Sessions dashboard reuses SSH connections with a private control socket under
`~/.ssh`, avoiding repeated authentication on every poll. During checks, the CIS
bastion reset some fresh SSH connections; the complete browser check passed after
connection reuse was enabled.

| Worker | SSH/tmux command + completion | Browser terminal + reattach |
| --- | --- | --- |
| mac-air | Passed; tmux 3.7b | Passed |
| archlinux-worker | Passed; tmux 3.6a | Passed |
| cis-linux2 | Passed; tmux 3.4 | Passed |
| cis-a6000 | Passed; tmux 3.2a | Passed |

The browser check creates the same session name on the master and all workers,
verifies hostnames through the actual WebSocket terminals, and verifies that
removing the Air session preserves the other four sessions. Generated test
sessions are removed afterwards.

On `cis-a6000`, `nvidia-smi` reported two NVIDIA RTX A6000 GPUs (49,140 MiB each),
and `sinfo` reported `LocalQ` up. No compute job was submitted; model credentials,
agent CLI sign-ins, GPU workload correctness, and scheduler allocation were not tested.

Reproduce the short execution and browser checks:

```sh
cargo run -p hive-core --example check-worker-execution --offline
NODE_PATH=/path/to/node_modules python3 scripts/check-remote-sessions.py
```

`check-worker-plan` takes a request and uses the real planner without executing
commands. Set `HIVE_PLAN_CHECK_DB` to a disposable snapshot of the machine graph.

The requested file-sharing service is installed on the Air and Arch only, with
`ws-share` tmux sessions and identical private configurations. Both directions of
upload and download passed SHA-256 verification with a generated 1 MiB binary
file whose name contains spaces. See [file-sharing usage](../tools/ws-share/README.md).

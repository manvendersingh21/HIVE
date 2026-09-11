# Web chat execution

Web chat runs a feedback loop with the configured model, including local Ollama.
The model plans a small round; Hive writes its files and executes its commands
on the named destinations, records actual results, and asks the model for the
next round. This supports inspection, implementation, correction, and verification
across machines without requiring a separately signed-in agent CLI on each one.

The initial plan fixes the task's destinations. Later rounds cannot execute on
another machine. Named placement does not require invented capabilities such as
`websocket`; automatic placement still uses the capability graph. Each remote
command runs under Hive's tmux supervisor. Persistent services need their own
named detached tmux sessions, which appear in the Sessions dashboard.

Hive waits for remote completion and supplies stdout, stderr, and failure status
to the next planning round. Failed commands stop dependent actions. Structured
file writes preserve multiline source text without shell interpolation. The
model must request verification, with successful checks on every destination,
before Hive accepts completion. This checks execution evidence; it does not
guarantee that the model chose sufficiently thorough tests for arbitrary software.

The browser submits work in the background and polls saved progress. Reloading
or disconnecting does not discard the task. Approval resumes the saved round
without replaying completed actions; newly generated actions receive their own
checks. After a server restart, interrupted work stays recorded and is not
automatically replayed. Unconfirmed remote results stop for review. The loop is
bounded to 24 rounds, 150 seconds for initial planning, 300 seconds per
continuation attempt, and 120 seconds per command. A failed continuation gets
one retry with its error and a request for a smaller action; completed commands
are not replayed.
Identical successful rounds are rejected before execution and returned to the
model as a lack of progress. Shell pipelines propagate failures to prevent a
successful trailing filter from hiding a failed command.
The remote wrapper preserves heredoc terminators, trailing comments, and output
without a final newline, so multiline generated code retains its shell syntax.

This loop applies to web chat. Other callers of the single-round core APIs do
not automatically gain multi-round orchestration. Independent worker LLM agents
and peer-to-peer agent collaboration are not implemented by this change: the
Hive model coordinates the worker executions and their shared observations.

Validation:

```sh
cargo test -p hive-core -p hive-web --offline
cargo check -p hive-cli --offline
cargo build -p hive-web --offline
NODE_PATH=/path/to/node_modules python3 scripts/check-chat-history.py --browser
python3 scripts/check-live-web-workflow.py /path/to/request.txt
```

The last command uses real Ollama and configured workers through an isolated
Hive web instance. It leaves requested files and sessions available for
inspection. The harness submits requests; all workload implementation happens
inside Hive.

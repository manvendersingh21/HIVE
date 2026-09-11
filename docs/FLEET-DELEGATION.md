# Fleet delegation

Hive can coordinate native coding agents over SSH while each assignment keeps its own tmux session and native conversation. The implementation is opt-in for new website requests: start `hive-web` with `HIVE_DELEGATION=1`. Existing saved chats and unrelated tmux sessions remain in the same database and Sessions view.

This feature is undergoing live validation. The current demonstration uses **Claude on `cis-a6000` and Codex on `archlinux-worker`**. The original Air assignment was superseded following the user's device change. The Mac mini runs Hive's coordinator; the transfer endpoints must continue independently of its agent conversations. A completed, independently verified two-way 1 GiB proof has not yet been recorded. Do not treat installed agents or model discovery as proof of successful execution.

## Planning and inventory

The coordinator refreshes the existing machine graph and per-device agent records before planning. Background refresh also runs at startup and periodically. Records retain executable paths, versions, authentication state, runtime requirements, controls, model discovery, timestamps and successful invocation evidence as separate fields. Offline devices remain in fleet memory with stale evidence. Provider credentials stay on their devices.

Qwen receives the fleet description and produces assignments containing `key`, `device`, `agent`, `model`, `workspace`, `objective`, `dependencies`, `acceptance_criteria` and `required_capabilities`. Hive validates configured destinations, unique assignment keys, dependency ordering, workspace containment and placement constraints before launch. Canonical explicit phrases such as `Claude on cis-a6000` are enforced. User phrasing outside this explicit-pair form still depends on planner interpretation; there is no general natural-language constraint parser.

Workspaces must be children of `~/hive-workspaces/`. Laptop and login-node restrictions remain in effect; direct heavy work on scheduler-managed hosts is rejected. A missing executable, authentication requirement, runtime or model becomes `needs-setup` on the named device. Installing software does not silently count as a successful model invocation.

## Remote execution and persistence

SSH uses configured worker aliases, explicit ports and strict host-key checking. Private runner bundles live at `~/.hive/runners/v1-<content digest>/`; active runs retain their original bundle. Each assignment has a journal at `~/.hive/runs/<run UUID>/journal.db` and a tmux name `hive-agent-<run UUID>`.

The adapters use Claude's Agent SDK, Codex app-server, OpenCode's local server API and AGY's persistent JSON stream. Claude can use an isolated Python SDK environment on Python 3.10 or newer, including machines without Node; the JavaScript SDK is the alternate path. Native interfaces and runtime prerequisites are probed before assignment execution. AGY and OpenCode provider-backed execution still require live validation; their presence in inventory must not be read as working authentication or model availability.

Hive stores run, device, agent, requested model, actual native model when reported, workspace, tmux name, native conversation identity, event cursor and execution state. Remote journal ownership and a coordinator launch claim prevent automatic duplicate launches. A lost connection is reconciled from the existing journal. If prior native actions have an uncertain outcome, the runner requests reconciliation rather than replaying them automatically. A failed, disconnected or partially initialized native run exposes **Review recovery** in chat. Hive retrieves the authoritative remote snapshot, blockers, uncertain delivery IDs and a fingerprint. The operator records the recovery reason and verified effects of the last action, then acknowledges every uncertain delivery without replaying it. A blocked or changed snapshot cannot resume. Accepted recovery restarts the existing session and native conversation and records an audit event; ordinary browser reload and coordinator restart do not create a new assignment. This recovery path was exercised on the live Arch Codex run after **Stop** interrupted its turn: the same native conversation resumed without replaying the interrupted delivery.

Peer questions, answers, agreements and deployment reports are task-scoped messages with stable delivery IDs and acknowledgments. Guidance joins the same native conversation. A busy native adapter may deliver queued guidance at a turn boundary. Qwen reviews completed or waiting runs and can send follow-up work to those same conversations. Native completion alone does not establish that all task acceptance criteria passed.

## Permissions and the website

Deterministic runner policy decides which tool calls can proceed routinely. Scoped edits and recognized project test, build and isolated dependency commands are supported. Destructive changes, privileged actions, protected control files, unknown shell forms and actions outside the workspace require review. The policy is deliberately conservative: some routine commands still produce a prompt. Native sandbox and approval controls remain enabled.

A pending approval carries the exact action and immutable fingerprint. Chat shows the device, agent, action, reason and file-change details when available. **Continue** permits that exact action once; **Stop** rejects it. A changed action requires a new decision. Decisions persist and are delivered to the remote journal after reconnect. AGY uses pre-tool hooks with denial and a matching one-use grant because its stream does not accept permission-control messages.

Delegated chat receipts leave the composer available. Run cards show state, model, output, guidance controls and **Open Session** links. Polling updates cards while work or coordinator review is outstanding. Browser reload reconstructs runs and approvals from durable storage; expanded output panels are only retained during polling, not across a full reload. The Sessions API retains unrelated sessions and adds run metadata; an unreachable run can have a synthetic row even when its tmux server cannot currently be queried.

## HTTP API

These endpoints use the existing authenticated website session.

| Method and path | Purpose |
| --- | --- |
| `GET /api/runs?conversation_id=<id>` | List durable runs, optionally within one saved chat. |
| `GET /api/runs/<id>/events?after=<sequence>` | Read events after an exclusive cursor. |
| `POST /api/runs/<id>/decisions` | Save `{id, fingerprint, decision}`; decision is `continue` or `stop`. |
| `POST /api/runs/<id>/messages` | Queue guidance with UUID `id` and nonempty `text` of at most 16,000 bytes. |
| `POST /api/runs/<id>/replace` | Supersede an assignment using `device`, optional `agent` and optional additional `context`. |
| `GET /api/runs/<id>/recovery` | Inspect the remote journal, recovery blockers, uncertain delivery IDs and fingerprint. |
| `POST /api/runs/<id>/recovery` | Reconcile using `{fingerprint, reason, evidence, acknowledge_ids}` and resume the existing native conversation when permitted. |
| `POST /api/runs/<id>/retry-setup` | Retry setup only for an assignment without an existing deployed runner claim. |
| `GET /api/sessions` | Existing session array enriched with optional `run` metadata. Partial discovery errors use `x-hive-session-errors`. |
| `GET /terminal/<tmux-name>?host=<device>` | Open that specific remote session. |

Replacing an assignment creates a new run identity while preserving the task and peer work. It does not migrate a native conversation to another device. Hive retires the old assignment's owned tmux session when that device becomes reachable.

## Verification and rollout

Run the Rust workspace checks and the runner contract tests before enabling delegation for new production requests:

```sh
cargo test --workspace --locked
python3 -m unittest discover -s hive-core/src/delegation/runner -p 'test_*.py'
```

The browser check requires Playwright and Chrome. Fixture mode serves local static pages through intercepted routes and never writes to a live API:

```sh
NODE_PATH=/path/to/node_modules node scripts/check-delegation-browser.cjs
```

It verifies live polling with an available composer, escaped exact approval details, the decision payload, pending approval persistence after reload, output expansion during refresh and session metadata. Recovery fixtures additionally verify escaped recorded actions, required evidence and delivery acknowledgments, disabled resumption when the server reports blockers, and the exact fingerprint-bound recovery request. Add `--live` to also inspect existing runs, or `--live-only` for only those checks. Supply `HIVE_TEST_URL`, `HIVE_TEST_PASSWORD`, `HIVE_TEST_CONVERSATION`, and comma-separated `HIVE_TEST_RUNS`. `HIVE_TEST_CHROME` can override the Chrome executable path. Live checks read run/session/event APIs and terminal HTML, and reload the saved chat; they never approve actions, send guidance, start tasks, or attach an interactive terminal.

The WebSocket proof must still establish fresh agent-authored endpoints, a shared protocol, usable `send`/`get` commands, authenticated private connectivity, bounded memory, persistent services, both directions of 1 GiB transfer with SHA-256 agreement, quoted filenames, interrupted-transfer recovery, invalid-authentication rejection and path traversal rejection. Verify that evidence before moving the opt-in setting to production. Database migration and service restart must preserve existing chats and avoid two coordinators owning the same live runs.

Native interface references: [Claude Agent SDK](https://code.claude.com/docs/en/agent-sdk/user-input), [Codex app-server](https://learn.chatgpt.com/docs/app-server), [OpenCode server](https://opencode.ai/docs/server/), [AGY headless mode](https://antigravity.google/docs/cli/headless), [AGY hooks](https://antigravity.google/docs/hooks/).

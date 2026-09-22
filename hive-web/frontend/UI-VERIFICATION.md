# Hive frontend verification

## 2026-09-21 live delegation QA

PRA tested branch `pra/frontend-qa` from the dedicated worktree
`.claude/worktrees/pra-frontend-qa`, with PRB owning and independently fixing
backend failures found by the browser flow.

The production frontend build passes and all 36 deterministic browser tests
pass (four opt-in live tests skip by default). A new opt-in live test performs
the requested path through the real UI: it submits a task naming `cis-a6000`, requires a run card for that
machine, handles the agent's Continue approval and Retry setup controls, and
checks the remote output in the run events. It then requests collaborating
agents on `cis-a6000` and `mac-air`, requires exactly one run card for
each machine, handles both approval flows, and checks their peer-event
handshake. A bad live password now fails at login within 10 seconds instead of
waiting for the whole test timeout.

The resumed pass also covers fleet-settings add/remove, protected configured
workers, failed-add draft preservation, removal errors, and provider-save
failure/retry. The real local terminal flow passed in 5.3 seconds: login,
session creation, command output, resize, session cleanup, and logout. Live
login/logout checks accept the backend's canonical `/login` URL as well as
`/login/`; successful login also requires the authenticated Sign out control.

Two additional regressions reproduced real draft loss: typing the next message
while a previous send was pending erased the new text on success, in both the
main chat and direct-agent composer. Both paths now clear only the draft that
was actually sent and preserve subsequent edits. Both tests failed before the
fix and pass afterward. Another real frontend defect hid all agent events after
the API's first 300 rows. The viewer now offers Load more events; its regression
verifies pagination and refresh, and a real UI replay retrieved all 614 Luna
events and 69 Haiku events, including the incoming peer acknowledgment at row
301. The full 36-test suite and production build are green.

### Final lower-tier live results

The user subsequently restricted remote QA to lower-tier models. New tests
pin `gpt-5.6-luna` for cis-a6000/Codex, `haiku` for mac-air/Claude, and
`zai-coding-plan/glm-5.3-flash` for Arch/OpenCode. Tests reject unapproved model
overrides, compare the master's complete assignment set (device, agent, model)
against the request, and check runtime model evidence. The Claude `haiku` alias
must resolve to a `claude-haiku-*` identifier. There is no high-model fallback.
The lower-tier live results below supersede the earlier default-model runs.
The interrupted high-model QA runner and the old Sonnet handshake survivor
were terminated and marked superseded after the restriction; evidence remains
in the disposable QA database. No new high-model tests were launched.

- The named cis-a6000/Codex command completed on `gpt-5.6-luna` with actual
  successful command output, run `10419d03-ac0c-4fea-86fe-c6cb25c55b12`.
- The two-machine browser test passed in 2.8 minutes. Runs
  `fd408430-9c79-47bc-8cf8-b2afb0a7e5d9` (cis-a6000/Luna) and
  `eb4f834a-e0d7-4c1c-8a99-38c618329ad9` (mac-air/Haiku) exchanged actual
  questions, answers, agreements and acknowledgments. Haiku resolved to
  `claude-haiku-4-5-20251001`. A subsequent replay through the final paginated
  UI verified non-initial incoming acknowledgments, not just initial-task ACKs.
- Arch/OpenCode Flash passed in 7.6 minutes, run
  `0930ae15-2ec0-4334-a0b1-b95250cbeabc`, 55 events, actual model
  `zai-coding-plan/glm-5.3-flash`. The native completed Bash event contains
  `HIVE_OPENCODE_WORKER_OK`. The Flash CLI access check also passed.

An earlier Luna smoke run printed the token but combined it with failing
temporary-directory cleanup. The strict successful-command assertion rejected
that result. The final smoke prompt requests a standalone `printf` in Hive's
already disposable workspace, explicitly allowing Hive's bookkeeping files;
the output/exit-code assertion was not weakened.

The isolated server now serves the latest `hive-web/frontend/out` directly via
`HIVE_WEB_STATIC`, with no browser static override. The master stays on Z.AI
Coding using the existing `Z_AI` configuration; Qwen is not used. The final
frontend artifacts and counterparty verdict are tracked by HACP contract
`c-631a4466f758455b885b1c8cdb5b1172`, revision
`65bf3ce09c5668f1edccd3f7772b93d8ccfb62e0ddfca5ed05317c29f79ddceb`.

The previous strict two-machine run failed because cis-a6000/Codex exhausted
its account quota; its successful single-worker command event was verified
before the failure. This exposed a backend hang when a surviving peer waited
for a failed partner. PRB's fix now permits coordinator review for that case
and rejects messages to failed runs. PRA independently verified the focused
regressions, full core/web tests, and release build (HACP ACCEPT). It was not
live-replayed on the old Sonnet survivor, to avoid violating the newer model
restriction. PRB also fixed deterministic enforcement of explicit
`AGENT [agent] on DEVICE using model MODEL` and exact assignment counts;
wrong/null models, wrong placements and extra assignments are rejected before
dispatch. Unconstrained requests retain automatic selection. This is tested
for the documented explicit phrasing, not a guarantee for every natural-language
paraphrase. A planner's invalid workspace is now replaced with a fresh safe
child of `~/hive-workspaces/` before the unchanged workspace validation; valid
workspaces and device/agent/model selections remain unchanged. Both backend
contracts passed PRA's independent focused/full core-web tests and release
builds. Their latest release is running on isolated `:18280`.

Live testing found backend defects that PRB fixed under separate HACP
contracts and PRA independently verified:

- Cloud master-agent providers silently dropped the delegation JSON schema,
  so Z.AI returned an invalid plan and no worker run was created. Cloud prompts
  now include the schema and tolerate fenced JSON replies.
- Setup validation ignored a device-agent's verified invocation model when it
  was not also present in the CLI's short alias list. This left the verified
  `mac-air` Claude run in `needs-setup`; the verified invocation model now
  counts as available.
- Completed runs were repeatedly polled, and disconnected runs retried every
  three seconds, overloading the worker. Idle completed runs are now skipped;
  sync failures back off for 30 seconds. Launch probing allows 150 seconds.
- OpenCode's discovered model catalog was saved only after successful inference.
  A subscription failure on the default model therefore prevented selecting a
  working alternative. Runtime catalogs now persist independently of successful
  invocation evidence; the latter still requires actual success.

Before the lower-tier restriction, the requested pair passed on isolated `:18280`
(one test, 3.5 minutes): a named `cis-a6000/codex` task returned
`HIVE_REMOTE_WORKER_OK`, then `cis-a6000/codex` and `mac-air/claude` completed
their assignments and exchanged `HIVE_TWO_MACHINE_HANDSHAKE`. The test checks
each run card separately. The isolated server uses `/tmp/hive-pra-isolated/hive.db`
to avoid racing production review/sync leases. PRB reported deploying the
initial idle-polling fix to production with user approval; this frontend QA
does not certify production deployment of every later change.

The additional `live named opencode worker` test targets `archlinux-worker`.
Its first run launched real OpenCode and reached the provider, then failed
because the account lacks access to the default `zai-coding-plan/glm-5.3-highspeed`
(provider code 1311). `HIVE_UI_OPENCODE_MODEL` can request an explicit model
without changing the remote default configuration. Model catalog presence
alone does not establish subscription entitlement.

After PRB's independently verified catalog fix, the explicit
`zai-coding-plan/glm-5.2` browser run passed in 6.5 minutes. Run
`04893ddc-8900-46c1-80f7-1e1f462926cf` completed with 25 events. The assertion
requires a native completed Bash tool event whose output contains
`HIVE_OPENCODE_WORKER_OK`; task text and prompt echoes cannot satisfy it.
The model was also independently exercised through the remote OpenCode CLI.
Cold OpenCode provider discovery took several minutes on the loaded worker;
the live browser test has a ten-minute bound.

Reproduce with `HIVE_UI_LIVE_URL`,
`HIVE_UI_TEST_PASSWORD`, `HIVE_UI_OPENCODE_WORKER=archlinux-worker`, and
`HIVE_UI_OPENCODE_MODEL=zai-coding-plan/glm-5.3-flash`, then run
`npm run test:ui -- --grep 'live named opencode worker'`.
For the pair, also set `HIVE_UI_REMOTE_WORKERS=cis-a6000,mac-air`,
`HIVE_UI_CODEX_MODEL=gpt-5.6-luna`, and `HIVE_UI_CLAUDE_MODEL=haiku`, then run
`npm run test:ui -- --grep 'live named worker and two-machine collaboration'`.

## Historical verification: 2026-09-18

Verified 2026-09-18 by HACP peer A. Backend work belongs to peer B.
The remaining sections preserve that earlier snapshot; its limitations and
test counts are historical, superseded by the September 21 results above.

## Result and limits

The frontend production build passes. The browser suite has 29 passing
deterministic tests plus two opt-in live tests.

The local live terminal test passed against the Rust server at
`http://127.0.0.1:18080`: browser login, session creation, terminal connection,
typing a command, observing its output, resizing, returning to Sessions,
killing the created session, and logging out. The command
`printf 'HIVE_%s\n' 'UI_OK'` produced `HIVE_UI_OK`.
All tmux sessions created for this verification were removed.

The live chat test submitted and saved a command request. The UI displayed the
real backend failure: Ollama at `http://localhost:11434/api/chat` was unreachable.
This verifies failure reporting, **not successful command execution through chat**.
The test also exercised live machine re-probing and incident history.
One saved verification conversation remains in history as evidence.

Both live checks served the newly built HTML/JavaScript/CSS through browser
request interception, while authentication, API requests and terminal
WebSockets used the real Rust backend. This was necessary because the existing
server embeds older page shells at compile time. These checks **do not certify
deployment or server static routing**. Peer B's runtime page-serving fix is
pending; peer B reported that its execution environment rejected writes to the
shared checkout.

There are no configured remote workers in this checkout. Real SSH terminal
access, successful model-driven chat commands, and two agents communicating on
two machines remain unverified. The agent controls were exercised with API
fixtures matching the current Rust schemas, not live agents.

## Browser coverage

| Area | Exercised behavior |
| --- | --- |
| Authentication | Correct/incorrect password, network failure, sign-out success/failure, expired API authorization |
| Navigation | Every main page, active-page indicator, terminal back link, mobile navigation |
| Sessions | Name validation, machine and agent selection, working directory, creation, duplicate error, partial-host warning, terminal link, kill confirmation/cancel/failure/204 success |
| Terminal | Target parameters, binary keystrokes, text resize messages, disconnect/reconnect, missing target, cleanup on navigation |
| Chat | JSON creation/submission, UUID fallback, draft preservation on failure, saved history, search, pagination, new chat, slow response isolation, reopening/polling an active chat, composer locking |
| Chat approvals | Command/target display, approve/deny payloads, result session links, expandable execution details |
| Agent runs | Both machine/agent cards, terminal links, events, direct agent messages, message failure, continue/stop decisions, retry setup |
| Machines | Actual graph response, reachability, facts/tools, prompt preview, re-probe success/failure |
| Incidents | Actual analysis/review-state fields, escaped process output, resume, note, corrected command, abort, cancellation, history, conflict errors, terminal links |
| Layout | Restored shared styling, visible mobile history and composer, no horizontal overflow at 390px |

## Reproduce

Requires Node/npm and Google Chrome. Playwright is a development dependency.

```sh
cd hive-web/frontend
npm ci
npm run build
npm run test:ui
```

The default suite starts a static server on loopback port 18081 and skips live
tests. Test traces on failure go to `/tmp/hive-ui-test-results`.

For an explicitly selected test backend, set `HIVE_UI_LIVE_URL` and
`HIVE_UI_TEST_PASSWORD`, then run:

```sh
npm run test:ui -- --grep 'live local tmux|live chat records'
```

These tests create a local tmux session and a saved chat; the terminal test
deletes its own session. The chat test reports either completion or a visible
backend failure, so inspect the printed outcome before claiming chat execution
works.

Only while the page-serving fix is pending, set
`HIVE_UI_STATIC_OVERRIDE=1` to use local `out/` files with the real backend.
This also grants the test browser local-network access because intercepted
documents otherwise trigger Chrome's local-network protection. Rerun without
this override after deploying the backend fix.

Screenshots from the live checks:
`/tmp/hive-ui-live-terminal.png` and `/tmp/hive-ui-live-chat.png`.

## Fixes

Frontend requests now set JSON content types, handle empty success responses,
show server errors, and redirect expired logins. Pages consume the actual
machine and incident schemas. The chat UI offers approval controls and
machine-specific agent terminal/message controls, retains drafts on errors,
protects against stale conversation responses, and resumes polling saved work.
Session actions validate names and report failures. Terminal input, resize,
reconnect and lifecycle cleanup are wired to the backend protocol.

HACP frontend contract: `c-59bb5ebb1b0d45adbbf375fa4931df00`.
Frozen revision: `b1af66fa1b1bdc8e047adc8340a8dd426238c00f40eb97c6ff284de041174c6f`.
Counterparty verification is pending; this report is peer A's evidence.

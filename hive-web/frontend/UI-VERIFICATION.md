# Hive frontend verification

Verified 2026-09-18 by HACP peer A. Backend work belongs to peer B.

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

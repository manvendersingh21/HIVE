# Saved web chats

Open the Agent page and choose **New chat** to start a conversation. Chats are
saved automatically when you send a message. Use the **Chats** sidebar to search
by title or message contents and reopen a conversation. On a phone, tap **Chats**
in the header. The URL keeps the selected chat so a page reload reopens it.

Your messages, replies, command output, errors, and approval state live in the
configured SQLite database (`~/.hive/hive.db` by default). The web server uses the
existing `conversations` and `messages` tables plus `web_chat_turns` for request
state and structured command results. Database files remain private (0600), and
all history endpoints require the same login as chat and terminals. History can
be read even when Ollama is unavailable. All previously stored conversations,
including CLI conversations, appear in the list. Exchanges from older browser
sessions that were never stored in the database cannot be recovered by this
feature.

Follow-up requests receive the most recent ten messages from the selected chat,
limited to 600 characters per message. This is bounded context, not the complete
history of a long chat. Switching chats does not inject another chat's transcript.
The full saved transcript remains available in the UI and through `hive search`.
`hive memory reindex` can add saved chats to the semantic index; saving and reopening
chat does not wait for embeddings or knowledge extraction.

Messages are committed before planning starts. Work continues if the browser
loses its connection, and the result is saved for reopening. Repeated submissions
with the same request ID do not rerun commands. A chat accepts one request at a
time; finish pending approvals before sending the next message.

After a server restart, interrupted planning/execution is marked interrupted,
with a reminder that commands may already have run. It is never automatically
replayed. Plans that were already awaiting approval remain available to review.
Approving a saved plan executes only its remaining steps; completed commands are
not repeated. Errors and results are saved in the same assistant message slot.

## API

- `GET /api/chats?q=...&offset=0`: search/list 50 conversations at a time.
- `POST /api/chats` with `{}` or `{"project_id":"my-project"}`: create a chat.
- `GET /api/chats/{id}`: return the conversation and ordered messages/results.
- `POST /api/chat`: the existing request accepts optional `conversation_id` and
  UUID `request_id` fields; responses also include `conversation_id`.
- `POST /api/chat/{run_id}/approve`: the existing approval route reads the saved
  plan and rejects duplicate or conflicting execution attempts.

## Verification

```sh
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked
python3 scripts/check-chat-history.py
node scripts/check-chat-timeouts.cjs
git diff --check
```

The integration test uses a disposable SQLite database, fake Ollama, no cloud
keys, and no workers. It checks persistence, search, authentication, context
isolation, duplicate requests, restart recovery, model failures, browser
disconnects, and approvals without command replay. Optional Playwright checks
(`--browser`, with Playwright available through `NODE_PATH` and a Chrome executable
selected by `HIVE_CHROME`) exercise desktop and phone layouts, reload, search,
reopening, and HTML escaping.

Deployment verification (2026-09-09): all 442 workspace tests, both mock
integration scripts, browser timeout checks, and desktop/phone Playwright checks
passed. The installed Ollama web service was rebuilt and restarted. All six
previously stored conversations were visible. A live two-turn conversation
executed its first print request in 24.05 seconds and correctly reused the marker
from the prior message in 12.55 seconds. Reopening with a new authenticated
session returned all four messages, which were also verified directly in SQLite.

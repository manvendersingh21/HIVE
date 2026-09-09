# NVIDIA master reasoning and memory

This checkout selects `llm.single_provider = "nvidia"` and
`nvidia/nemotron-3-ultra-550b-a55b`. All master planning, classification, skill
selection, knowledge extraction, and model watchdog review use that provider.
Skill provider overrides and complexity recommendations cannot override this
setting. A missing key or failed NVIDIA call does not fall back to Ollama or
another cloud provider. Worker CLIs and the HACP runtime retain their independent
authentication and execution behavior.

The client calls NVIDIA directly with the existing Rust HTTP stack. Chat requests
use temperature 1, top-p 0.95, max_tokens 16384, non-streaming responses, and
`chat_template_kwargs: {"enable_thinking": true}` for all master calls. Ultra uses
its default thinking behavior; the DeepSeek-specific effort field is omitted.
Explicit older DeepSeek model configurations retain their original
`thinking`/`reasoning_effort` template. Only final `content` is used;
reasoning content is never interpreted as commands. Empty/incomplete responses
and invalid plans fail before execution. Transient HTTP/transport failures have
at most two retries, with exponential delays, inside one total deadline
(`llm.nvidia.timeout_secs`, default 120 seconds). Authentication/access failures
are returned immediately. The existing command and approval gates remain active.

Terminal rule checks continue during cloud watchdog review. Each session has at
most one outstanding review; unchanged output is not reviewed repeatedly. Ending
or cancelling supervision drops its outstanding request. Model errors remain
inconclusive, as in the existing watchdog; rule checks remain active.

## Environment setup

For an interactive shell, set both keys without adding their values to shell history:

```sh
read -rs NVIDIA_API_KEY_FLASH
read -rs NVIDIA_API_KEY_EMBEDDING
export NVIDIA_API_KEY_FLASH NVIDIA_API_KEY_EMBEDDING
./target/debug/hive task --local --deny-flagged -d "Run echo hello locally"
```

For the existing web launch script, add `NVIDIA_API_KEY_FLASH=...` and `NVIDIA_API_KEY_EMBEDDING=...` to the private
`~/.config/hive/web.env` alongside `HIVE_WEB_PASSWORD`, then restart the service
using your existing deployment procedure. The script exports the password and both API keys.
Do not put keys in `hive.toml` or commit them. The CLI does not automatically read
`.env` or `web.env`; export both keys in its process environment. A manually launched
`hive-web` inherits the same shell environment.

Older configurations without `single_provider` preserve their complexity routing
and local fallback. Local configuration now has defaults and can be omitted from
a NVIDIA-only configuration. NVIDIA mode does not probe Ollama at web startup;
API errors are reported through the existing chat route. CLI and web plans report
serialized provider `nvidia` and the model returned by NVIDIA (or the requested
model if the response omits it).

Master reasoning, including Ultra and knowledge extraction, uses only
`NVIDIA_API_KEY_FLASH`. This existing variable name is retained so credentials do
not need renaming when changing the reasoning model. Memory indexing, retrieval,
and reindexing use only `NVIDIA_API_KEY_EMBEDDING`. Missing-key and authentication errors identify
the relevant variable. The old shared `NVIDIA_API_KEY` is no longer used.

## Memory migration

This checkout uses `memory.embedding_provider = "nvidia"` and
`memory.embedding_model = "nvidia/nemotron-3-embed-1b"`. Searches send
`input_type = "query"`; indexed text and both sides of entity deduplication use
`passage`. Provider, model, and dimensions are recorded with each vector.
Semantic retrieval and deduplication exclude incompatible spaces. Legacy vectors
whose provenance was never recorded remain stored but are excluded until rebuilt.

```sh
./target/debug/hive memory
./target/debug/hive memory reindex
./target/debug/hive search "database decision" --project my-project
```

Reindex reads the existing messages and entities and preserves projects, history,
and graph facts. It commits one conversation (all chunks) or entity at a time,
after successful embedding. On interruption or failure, rerun the command: it
skips records already committed for the current source text and embedding space.
Old vectors survive failed replacements. Reindex runs independently of
`memory.auto_index`. The command exits nonzero if records failed; memory status
reports remaining work. Failed semantic search reports the error while returning
history and keyword matches. Reindex the persistent database during a quiet period
to avoid spending requests on source records that are still changing.

## Verification

```sh
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked
python3 scripts/check-nvidia.py
python3 scripts/check-nvidia.py --live
# Optional: read only the two NVIDIA keys from a dotenv file, without sourcing it
python3 scripts/check-nvidia.py --live --key-file .env
git diff --check
```

The smoke script creates a disposable database/configuration, removes the other
provider keys from child environments, points Ollama at an unreachable port, and
checks classification, planning, both embedding modes, CLI task execution,
memory indexing/reindex/search, and authenticated web chat. It executes only a
requested harmless print task, with no configured workers, and stops its temporary
web process. Mock tests need loopback socket access; live checks need outbound
access and NVIDIA model permissions.

References: [Ultra model](https://docs.api.nvidia.com/nim/reference/nvidia-nemotron-3-ultra-550b-a55b),
[Ultra chat API](https://docs.api.nvidia.com/nim/reference/nvidia-nemotron-3-ultra-550b-a55b-infer),
[previous Flash model](https://docs.api.nvidia.com/nim/reference/deepseek-ai-deepseek-v4-flash-0731),
[chat API](https://docs.api.nvidia.com/nim/reference/deepseek-ai-deepseek-v4-flash-0731-infer),
[embedding API](https://docs.api.nvidia.com/nim/reference/nvidia-nemotron-3-embed-1b-infer).

## Ultra validation and activation — 2026-09-09

Switched the checkout and NVIDIA default model to
`nvidia/nemotron-3-ultra-550b-a55b`. The existing Rust client uses Ultra's
`enable_thinking` template and accepts final answers separately from reasoning.
No LangChain dependency is required. Explicit DeepSeek configurations continue
using their original template.

| Live check | Outcome | Elapsed |
| --- | --- | --- |
| Standalone classification | Correct `SIMPLE`, HTTP 200 | 60.92 s |
| Standalone planning | Valid command-plan JSON, HTTP 200 | 3.30 s |
| Disposable CLI task plus memory indexing | Print command executed; memory indexed | 42.29 s / 43.31 s |
| Memory reindex | Passed | 0.03 s |
| Memory query | Passed | 0.57 s |
| Disposable authenticated web task | Command executed with Ultra reported | 4.52 s / 5.61 s |
| Restarted installed web service | `hive-ultra-ready` printed, exit 0, Ultra reported | 5.59 s |

Workspace build, test compilation, all 436 tests, the mock CLI/web integration,
and whitespace checks passed. Release CLI and web binaries were rebuilt. The
existing `dev.hive.web` service was restarted and verified healthy with chat
enabled. Its current listening address and master identity were placed in the
private launch environment to preserve them across the restart. Ultra uses
`NVIDIA_API_KEY_FLASH`; embeddings use `NVIDIA_API_KEY_EMBEDDING`.

Live smoke checks now pass end to end. Latency varied across calls, including the
60.92-second standalone classification; the 120-second request deadline remains.

## Previous Flash validation — 2026-09-09

Workspace build, test compilation, workspace tests, `git diff --check`, and the
mock CLI/web smoke test passed. Tests cover legacy configuration/schema migration,
interrupted reindex and retry, transactional rollback, provider/model/dimension
isolation (including entity dedup), deadlines/retries/authentication/model access,
invalid plans, reasoning separation, delayed review responsiveness, request
cancellation, and unchanged-output suppression. The mock smoke check also runs in
CI on Linux and macOS.

Live checks used the available key and disposable configuration/database:

| Check | Outcome | Elapsed |
| --- | --- | --- |
| Nemotron passage embedding | Passed, 2,048 dimensions | 0.76 s |
| Nemotron query embedding | Passed, 2,048 dimensions | 0.57 s |
| Flash low-effort classification | Response read timed out | 120 s timeout |
| Flash high-effort planning | Response read timed out | 120.30 s |
| Rust CLI harmless print task, first attempt | NVIDIA HTTP 400; stopped before execution | 0.30 s |
| Rust CLI harmless print task, retry | NVIDIA request deadline exceeded; stopped before execution | 120.98 s |

Live Flash planning and execution readiness was **not established**. The service
returned no usable classification or plan during these checks. No automatic model
substitution was enabled. Live web execution and extraction could not be validated
through the failed chat provider; their routing and behavior passed mock checks.
The checkout has since switched to Ultra; `scripts/check-nvidia.py --live` now checks Ultra. Individual
API checks are available with `--probe classification`, `--probe planning`, or
`--probe embeddings`; `--probe integration` runs the CLI/web path directly.

After separating the credentials, the build, test compilation, 435 workspace
tests, and mock CLI/web smoke checks passed using distinct Flash/embedding keys.
Live embeddings with `NVIDIA_API_KEY_EMBEDDING` passed (passage 0.63 s, query
0.54 s; 2,048 dimensions). Classification with the distinct
`NVIDIA_API_KEY_FLASH` still timed out after 120.22 s.

# Optional NVIDIA provider

The checkout is back on **Ollama**: `llm.single_provider = "local"`,
`llm.local.model = "qwen3.5:9b"`, `memory.embedding_provider = "local"`, and
`memory.embedding_model = "nomic-embed-text"`. Planning, classification, skill
selection, extraction, watchdog review, and memory retrieval use local models.
NVIDIA keys are not required. Run `hive memory reindex` when switching embedding
models; the provider/model isolation and resumable migration remain available.

The NVIDIA client is retained as an optional provider. The following sections
document its configuration and the earlier hosted-model trials. They do not
describe the active deployment. To explicitly opt in, set
`llm.single_provider = "nvidia"`, configure `[llm.nvidia]`, and export the two keys.
A missing key or failed call never silently switches providers. Worker CLIs and
the HACP runtime retain their independent authentication and execution behavior.

The client calls NVIDIA directly with the existing Rust HTTP stack. Chat requests
use temperature 1, top-p 0.95, max_tokens 16384, non-streaming responses, and
`chat_template_kwargs: {"enable_thinking": true}` for planning. Auxiliary calls
(classification, skill selection, extraction, and watchdog review) use
`enable_thinking: false` to avoid full reasoning latency. The DeepSeek-specific
effort field is omitted for Ultra.
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

For NVIDIA memory, use `memory.embedding_provider = "nvidia"` and
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
[embedding API](https://docs.api.nvidia.com/nim/reference/nvidia-nemotron-3-embed-1b-infer).

## Web request deadlines

Web planning has a 150-second total deadline across memory, skill selection,
classification, and planning. A planning timeout returns HTTP 504 before command
execution. The browser bounds chat and approval requests (including response-body
reads) to 180 seconds and reports network loss or expired sessions explicitly.
It does not automatically retry: after a transport timeout, execution may still
be running, so inspect Sessions before repeating a command.

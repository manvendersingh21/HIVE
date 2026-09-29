### Fixed
- Restore automatic graph and RAG indexing for saved web replies. The durable web chat path bypassed the CLI completion hook; failed extraction now remains retryable instead of being treated as a successful empty result.
- Retrieve project memory and daily operational lessons in web planning. Preserve graph, vectors, and source watermarks together when ingestion fails, and remove indexed delegated transcripts when their chat is deleted.

- Preserve the full transcript across RAG chunk boundaries, including long words with zero overlap.

### Added
- An in-process nightly memory catch-up job in hive-web, configured by `[memory.nightly] enabled = true` and `time = "03:00"` in host local time, plus authenticated `POST /api/memory/ingest` with counts and a durable progress watermark.
- Idempotent ingestion of saved conversations and all synchronized delegated-run events, including late events and edited replies. Each successful source version advances its watermark; unavailable Ollama logs one warning and skips the batch, leaving sources pending.
- One daily lessons-learned node with observed failures, retries, agent/device pairs and reasons, available to both planners. Automatic ingestion and planner recall use only installed local Ollama models from `[llm.local]` and `[memory]`, reject remote endpoints/cloud aliases, and never use provider fallback. Explicit CLI reindex/search retains its configured embedding provider.

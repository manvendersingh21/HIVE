### Fixed

- Marked `claude` runtime-ready when any `python3` (>=3.10) on `PATH` can import `claude_agent_sdk`, and recorded that interpreter as `sdk_python`.
- Added support for reusing a shared virtual environment at `~/.hive/sdk-venv` across content-hashed runner directories.
- Preserved fallback to hashed runner `.sdk/bin/python` and node `@anthropic-ai/claude-agent-sdk`.

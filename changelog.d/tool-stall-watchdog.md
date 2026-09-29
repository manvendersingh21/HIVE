### Fixed

- A run no longer sits in `working` with no sign of trouble when a tool call
  never finishes (BUG18: a background loop inherited the bash tool's output
  pipe, so the finished build's call never saw EOF). The runner watches
  OpenCode tool parts, Codex `commandExecution` items, Claude `tool_use` blocks
  and Cursor tool calls; after 10 minutes with no native event while a call is
  in progress it emits one `stalled` event naming the tool, its command (up to
  300 characters) and how long it has been silent. New output clears the stall;
  another needs a further full silent interval.
- The run's metadata records the stall, and the session page and state chip
  show `Stalled: <command> silent for <m> min`. The run stays `working`.

### Changed

- The prompt Hive gives agents tells them never to leave background processes
  attached to the tool's stdout/stderr: start them with `nohup` or `setsid` and
  `>file 2>&1 </dev/null`, or avoid background loops.

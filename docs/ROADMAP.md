# Hive — Roadmap

> Historical phase plan, not a current feature guarantee. See the
> [current limitations](../README.md#current-limitations).

All 10 phases, in dependency order, from the original implementation plan.
Picking the work up? Start with the [contributor guide](../CONTRIBUTING.md).

| # | Phase | Depends on | Status |
|:---:|:---|:---|:---|
| 1 | Scaffold, `hive-common` types, workspace | — | ✅ done |
| 2 | LLM router + agent loop | 1 | ✅ done |
| 3 | Worker pool, SSH delegation, tmux creation | 2 | ✅ done, live-verified |
| 4 | `hive-worker` daemon — real task execution | 1 | ✅ done, live-verified |
| 5 | `hive-web` — web terminal + agent UI | 3 | ✅ done, live on the master |
| 6 | `hive-cli` — CLI subcommands | 2–4 | 🟡 all core commands live; `finetune export` honestly not-implemented |
| 7 | Skill system | 2 | ✅ done, live-verified end to end |
| 8 | Fine-tuning pipeline | 2 | ⬜ empty struct, not started |
| 9 | Memory — projects, knowledge graph, RAG | 2 | ✅ done, live-verified cross-process |
| 10 | Safety watchdog | 3, 7 | ✅ done, live-verified |

Legend: ✅ done · 🟡 partial · ⬜ not started

## Known limitations

- **Phase 2/3 — local execution has no confirmation gate yet on its own; the
  watchdog (Phase 10) is what makes it safe.** Remote subtasks are delegated
  through the worker pool, not executed by the planner directly.
- **Phase 3 — supervision is tied to process lifetime.** `hive task` returns as
  soon as delegation starts, so anything it delegates keeps running but stops
  being *watched* the moment the CLI exits; the remote command itself is
  unaffected. `hive chat` supervises for its whole session. Continuous
  supervision independent of any one CLI invocation needs the persistent
  master (Phase 6), which is where the supervisor now actually lives.
- **Phase 3 — remote agentic-CLI delegation isn't built.** Routing
  `AiProvider::Claude`/`Codex` to a supervised `claude`/`codex` session on a
  *worker* (rather than a plain shell command) needs those CLIs installed and
  authenticated there; the existing local-session machinery should extend to
  cover it.
- **Phase 4 — pause is a real `SIGSTOP` to the pane's foreground process
  group, not an interrupt.** An interrupt would end the job and make resume
  impossible. Signalling goes through `libc::killpg` directly (no shell, no
  argument parsing) and refuses process-group IDs `<= 1` or the daemon's own
  group — an earlier version shelled out to `kill -STOP -<pgid>`, and a
  negative pgid there means "every process the caller can signal," which
  once stopped unrelated services on a shared host.
- **Phase 5 — the browser-to-worker hop skips SSH.** `hive-web` runs *on* the
  worker, so that leg would be a loopback connection to itself; running one
  instance per worker is simpler than a master-side SSH-fanout aggregator,
  and only worth revisiting once there are enough workers to justify it.
- **Phase 6 — `hive workers add/remove` is deliberately not built.**
  `config/workers.toml` is hand-maintained, and its comments carry reasoning
  that matters (e.g. `host` must be an SSH config alias, never a raw IP,
  because this repo is public) — a TOML serializer would drop every one of
  them on the first edit.
- **Phase 7 — `hive skills add/remove` authoring commands are deliberately
  not built.** Skills are a handful of hand-written TOML files in a
  user-owned directory; an authoring UI hasn't earned its complexity yet.
- **Phase 8 is a stub.** `hive-core/src/finetune/mod.rs` is an empty
  `DataCollector`; `hive finetune export` exists as a subcommand and reports
  honestly that it isn't implemented.
- **Phase 10 — two known gaps.** Incident-log database path resolution is
  duplicated in `hive-web` and `hive-core` rather than centralized, and
  `SupervisorHandle::stop_session`/`shutdown` exist but nothing calls them
  yet — supervision can be addressed individually, just not from a CLI or
  web surface.

## Open questions carried over from planning

1. **Worker details** — hostnames/IPs and SSH usernames are placeholders in
   the example config; each deployment provides its own.
2. **Cloud spend** — no daily cost cap or confirmation threshold exists for
   Claude/Gemini/Codex calls.
3. **Web exposure** — password auth is LAN-only-adequate. Anything reachable
   beyond the LAN (e.g. over Tailscale) should add TOTP or client certs.
4. **Fine-tuning corpus** — build up from usage, or seed from existing logs?

# Adapter-edge findings — Phase S heterogeneous smoke (W1)

**Date:** 2026-09-04 · **Run:** `run-smoke` · **Scope:** one stock CLI worker ↔ `hive-adapter` ↔
throwaway loopback coordinator sink (`127.0.0.1:7919`). Topology-neutral by construction: no
master, no arbiter, no orchestrator. Hard time-box honored; first-blocker-recorded-not-fixed rule
honored.

## Outcome

`codex exec` completed a full file-edge lifecycle against a real coordinator-shaped sink:
read `BRIEF.md` and the delivered `INBOX/run.started`, wrote well-formed
`OUTBOX/001-work.started.json`, `hello.txt`, and `REPORT.json`; the adapter stamped and ingested
the envelope (sink seq 39), drained the outbox (`sent: 1, retained: 0`), relayed the worker's own
report verbatim as `report.submitted` (seq 40), and exited cleanly on the exit file.
The claude leg was blocked by an account session limit (below); heterogeneous coverage was
achieved with codex only this session.

## Findings (topology-neutral, feed HACP/2.0)

1. **Write rights are a spawn-time concern, not a protocol one.** `codex exec` defaults to a
   read-only sandbox. It read the brief and INBOX perfectly, could not write anything
   ("writing is blocked by read-only sandbox"), and the fix lived in the launch line
   (`--sandbox workspace-write`), invisible to the adapter. For 2.0: a worker that cannot write
   its workspace cannot hold up its side of a contract, and nothing post-spawn can repair that —
   workspace write semantics belong in the delegation/launch contract.

2. **Exit code is not an outcome.** The sandbox-blocked codex run **exited 0** having produced
   nothing. Sentinel-based exit capture reported a healthy worker. Truth is the artifact set and
   the report, never the process status — the 2.0 verification model (evidence over signals) is
   the right spine; supervisors must never treat exit 0 as success.

3. **The brief-as-file edge works with stock CLIs.** A tiny pointer prompt
   ("Read BRIEF.md in this directory and follow it exactly") plus a structured brief produced
   byte-exact JSON files, unprompted, from a stock CLI — codex even self-validated both JSON
   files. Argv stays trivially small; the task lives in the file. Confirms the file edge as the
   2.0 default binding for agent hosts.

4. **The poll endpoint must filter by addressee.** The throwaway sink ignored the `agent=`
   parameter and to-addressing: every INBOX accumulated other agents' heartbeats and each
   agent's own boomeranged messages (worker b received worker a's heartbeats and its own
   `hello`). Harmless at N=1, this is cross-role noise and information leakage at scale.
   2.0's reference coordinator: poll pages are filtered by addressed-to agent; the adapter's
   at-least-once + message-id dedup behaved correctly throughout.

5. **tmux targeting gotchas re-verified under live load.** Pane-scoped commands require the
   trailing colon (`=name:`; bare `=name` → "can't find pane"), and any *shell-side* tooling must
   quote `=name` (zsh equals-expansion tries to resolve it as a command). `LocalSessionHost`
   builds these targets in Rust and is immune; the sentinel-in-log exit capture worked for both
   a real failure (exit 1) and a real success (exit 0).

6. **Adapter final-pass and crash-recovery semantics verified against real worker output.**
   On exit-file discovery the adapter drained the outbox, relayed the worker's own report, and
   exited. Accidentally also verified: the adapter was group-killed mid-run by the harness (tool
   timeout), and a relaunch re-`hello`'d, deduplicated, and completed the final pass
   idempotently — at-least-once edges recovered exactly as designed. Harness artifact disclosed;
   outcome informative.

7. **Sink contract shape confirmed sufficient.** `POST …/ingest` answering `{"status":
   "accepted", "seq": N}` (409 duplicate) and `GET …/messages?since=&agent=` answering
   `{"state", "seq", "messages"}` drove the whole lifecycle; bearer auth enforced on both legs
   (401 observed on unauthenticated POST; adapter authenticates both).

## Remaining gap

The claude leg remains unexercised (account session limit, resets 07:30 America/Los_Angeles).
Not a protocol finding; rerun is free once the limit clears. Scratch evidence retained at
`$TMPDIR/opencode/phase-s/` (sink log, `ingested.jsonl`, both agent dirs, adapter logs).

---

## Live HACP/2.0 runs (2026-09-04, W6)

Real CLI pairs over the file edge via `interop/live/hacp-live.py` — settled:
`claude×codex` and `agy×opencode` (9 frames each, accept verdicts, all verifier
checks adapter-corroborated). Transcripts committed under `interop/live/transcripts/`.
The claude leg above is now exercised. New findings:

8. **Narrated success without an artifact is common and confident.** Two different CLIs
   (opencode/qwen3.8-max twice, agy once) replied "created and validated on disk" /
   "I have created … using the file-writing tool" while the file did not exist at the
   expected path. The adapter's check-file-exists-before-believing rule (§9.4 applied to
   agents, not just to verdicts) is what kept the runs honest; a retry with an explicit
   "your previous reply did not create the file" note recovers some cases but not all.
   Never promote an agent's self-report to evidence.

9. **Briefs must carry absolute paths.** agy (antigravity) resolves "the current
   directory" to `~/.gemini/antigravity-cli/scratch/`, ignoring the process cwd; its
   `--add-dir <workspace>` flag plus absolute paths in the brief fixes it. Every
   file-producing brief in `hacp-live.py` now names the absolute target path.

10. **Exit codes remain meaningless.** Every CLI call in both settled runs exited 0 —
    including the ones that produced nothing (finding 8). Consistent with finding 4:
    adapters must derive outcome from artifacts, never from exit status.

---

## The HIVE runtime's own live runs (2026-09-05, M1)

The same lifecycle, now driven by `hive-core::runtime` through `hive collab run` rather
than by the Python script — every agent invocation a supervised tmux session, every
claim measured by `runtime::attest`. Two pairs settled on their first attempt:

| Pair | Frames | Verdict | Corroborated | Contradicted | Unmatched |
|:---|:---:|:---|:---:|:---:|:---:|
| `claude` sup × `codex` wrk | 10 | accept | 5 | 0 | 0 |
| `agy` sup × `claude` wrk | 10 | accept | 4 | 0 | 0 |

Ten frames rather than the Python driver's nine: this runtime also emits `session.close`,
so the session reaches a terminal state on the wire instead of being abandoned.
Transcripts and reports: `interop/live/transcripts/hive-*.{jsonl,json}`.

11. **A contract can be satisfied completely and still deliver nothing.** In
    `claude × codex` the supervising agent authored three acceptance criteria — one line,
    non-empty, final word `ready` — and the performing agent wrote a file containing
    exactly `ready\n`, six bytes. Every criterion holds. The verifier ran `shasum`, `wc`,
    `xxd` and `awk` for real and accepted, correctly. The objective given to the run was
    "a status report confirming the work is done, ending with the word ready", and no
    report was produced.

    Nothing malfunctioned. The worker satisfied the contract, the verifier verified the
    contract, and the contract was not the objective. The identical artifact — same
    digest, `ed1a545b…`, same six bytes — came out of the Python driver's `claude × codex`
    run a day earlier, so this is reproducible, not a fluke.

    It is worth being precise about what this does and does not show. §9.4 protects
    against an agent lying about what it did; it says nothing about whether what it did
    was worth doing. That second gap is a **drafting** problem — the supervising agent
    wrote criteria that a degenerate output satisfies — and the place to close it is the
    authoring brief and, eventually, an objective-satisfaction check that is not the
    contract's own acceptance list. Adding a rule to the verifier would be the wrong fix:
    the verifier did its job exactly.

    Recorded rather than patched. The measurement is more useful than a quick guard, and
    the next milestone that touches formation should be checked against it.

12. **Both settled runs needed no retries.** Twelve of twelve invocations across the two
    runs produced their required file on the first attempt (9–26 s each). Finding 8's
    narrated-success failure did not reappear here, which is evidence about briefs rather
    than about vendors: every brief in `runtime::brief` names an absolute target path
    (finding 9) and states the required file as the single deliverable.

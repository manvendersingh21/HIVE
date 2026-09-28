"use client";
import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { api } from "../../lib/api";
import { TERMINAL_STATES } from "../../lib/runEvents";
import { TeamPanel } from "../../components/TeamPanel";
import { RelayAudit } from "../../components/RelayAudit";
import { Coordination } from "../../components/Coordination";
import { Shell } from "../../components/Nav";
import { usePoll } from "../../lib/poll";
import {
  Run,
  RunAttention,
  RunComposer,
  RunTitle,
  StateChip,
  Transcript,
  killSession,
  rawTerminalUrl,
  sessionUrl,
  useRunEvents,
} from "../../components/RunView";

export default function SessionPage() {
  const [id, setId] = useState<string>();
  const [runs, setRuns] = useState<Run[]>();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setId(new URLSearchParams(window.location.search).get("run") || "");
  }, []);
  const current = useRef(id);
  current.current = id;
  // Only this run's task: its siblings, not every run Hive has ever made.
  const load = useCallback(async () => {
    if (!id) return;
    try {
      const data = await api<Run[]>(`/api/runs?task_of=${encodeURIComponent(id)}`);
      if (current.current === id) setRuns(data);
      setError("");
    } catch (e) {
      setError((e as Error).message);
    }
  }, [id]);
  // Once every run in the task has finished, nothing on this page can change.
  const live = !runs || runs.some((r) => !TERMINAL_STATES.includes(r.state));
  usePoll(load, 3000, [load], live);
  const run = runs?.find((r) => r.id === id);
  const siblings = run ? runs!.filter((r) => r.task_id === run.task_id) : [];
  const { events, error: eventsError, earlier, loadingEarlier } = useRunEvents(
    run,
    500,
    !!run && !TERMINAL_STATES.includes(run.state),
  );
  async function kill() {
    if (!run || !confirm(`Stop and remove the ${run.assignment.agent} session on ${run.assignment.device}?`))
      return;
    setBusy(true);
    try {
      await killSession(run);
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Shell>
      <main className="session-page">
        <div className="row session-crumbs">
          <Link href="/sessions/">← Sessions</Link>
          {run && (
            <Link href={`/?chat=${encodeURIComponent(run.conversation_id)}`}>
              Open chat
            </Link>
          )}
        </div>
        {id === "" && <p className="empty">No session selected.</p>}
        {id && runs && !run && <p className="empty">This session no longer exists.</p>}
        {!runs && !error && <p className="muted">Loading session…</p>}
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
        {run && (
          <div className="session-grid">
            <section className="session-main">
              <div className="card">
                <RunTitle run={run} />
                <p>{run.assignment.objective}</p>
                <RunAttention run={run} siblings={siblings} events={events} refresh={load} />
              </div>
              <Transcript events={events} earlier={earlier} loadingEarlier={loadingEarlier} />
              {eventsError && <p className="error">{eventsError}</p>}
              <RunComposer run={run} refresh={load} />
            </section>
            <aside className="session-side">
              <TeamPanel
                key={run.task_id}
                taskId={run.task_id}
                currentId={run.id}
                select={setId}
                live={live}
              />
              <Coordination run={run} siblings={siblings} onSelect={setId} />
              <RelayAudit key={run.id} run={run} live={live} />
              <div className="card">
                <h3>Details</h3>
                <dl>
                  <dt>Workspace</dt>
                  <dd className="mono">{run.assignment.workspace}</dd>
                  <dt>Session</dt>
                  <dd className="mono">{run.tmux_name}</dd>
                  <dt>Assignment</dt>
                  <dd className="mono">{run.assignment.key}</dd>
                </dl>
                {!!run.assignment.acceptance_criteria?.length && (
                  <details>
                    <summary>
                      Acceptance criteria ({run.assignment.acceptance_criteria.length})
                    </summary>
                    <ul>
                      {run.assignment.acceptance_criteria.map((c) => (
                        <li key={c}>{c}</li>
                      ))}
                    </ul>
                  </details>
                )}
              </div>
              {siblings.length > 1 && (
                <div className="card">
                  <h3>Same task</h3>
                  {siblings.map((s) => (
                    <Link
                      key={s.id}
                      href={sessionUrl(s.id)}
                      className={`sibling ${s.id === run.id ? "current" : ""}`}
                      onClick={() => setId(s.id)}
                    >
                      <span>
                        {s.assignment.agent} on {s.assignment.device}
                      </span>
                      <StateChip state={s.state} run={s} />
                    </Link>
                  ))}
                </div>
              )}
              <div className="card">
                <h3>Advanced</h3>
                <div className="row">
                  <Link className="button" href={rawTerminalUrl(run)}>
                    Raw terminal
                  </Link>
                  <button className="danger" disabled={busy} onClick={() => void kill()}>
                    Kill session
                  </button>
                </div>
                <p className="muted small">
                  The raw terminal shows the runner&apos;s protocol stream, not a
                  normal agent screen.
                </p>
              </div>
            </aside>
          </div>
        )}
      </main>
    </Shell>
  );
}

"use client";
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { api } from "../../lib/api";
import { TERMINAL_STATES } from "../../lib/runEvents";
import { Shell } from "../../components/Nav";
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
  const load = useCallback(async () => {
    try {
      setRuns(await api<Run[]>("/api/runs"));
      setError("");
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);
  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 3000);
    return () => clearInterval(timer);
  }, [load]);
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
                      <StateChip state={s.state} />
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

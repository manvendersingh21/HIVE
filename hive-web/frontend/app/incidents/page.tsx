"use client";
import { useEffect, useState } from "react";
import Link from "next/link";
import { api, terminalUrl } from "../../lib/api";
import { Shell } from "../../components/Nav";
type Incident = {
  id: string;
  worker: string;
  tmux_session: string;
  analysis: {
    severity: string;
    category?: string;
    reason: string;
    suggested_action?: string;
  };
  review_state: string;
  created_at: string;
  flagged_output: string;
};
export default function IncidentsPage() {
  const [all, setAll] = useState(false);
  const [items, setItems] = useState<Incident[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [notice, setNotice] = useState("");
  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const data = await api<Incident[]>(
          `/api/incidents${all ? "?all=1" : ""}`,
        );
        if (!cancelled) setItems(data);
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      }
    }
    void load();
    const timer = setInterval(() => void load(), 5000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [all]);
  async function decide(item: Incident, kind: string) {
    let decision: string | Record<string, string> = kind;
    if (kind === "resume_with_note" || kind === "modify_and_resume") {
      const note = prompt(
        kind === "resume_with_note"
          ? "Note for the agent"
          : "Corrected command",
      );
      if (!note?.trim()) return;
      decision = { [kind]: note.trim() };
    }
    if (
      kind === "abort" &&
      !confirm(`Abort ${item.worker}/${item.tmux_session}?`)
    )
      return;
    setBusy(item.id);
    setError("");
    setNotice("");
    try {
      const result = await api<{ applied: string }>(
        `/api/incidents/${encodeURIComponent(item.id)}/decide`,
        { method: "POST", body: JSON.stringify(decision) },
      );
      setNotice(
        result.applied === "session_already_gone"
          ? "Decision saved; the session had already finished."
          : "Decision applied.",
      );
      setItems(await api<Incident[]>(`/api/incidents${all ? "?all=1" : ""}`));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy("");
    }
  }
  return (
    <Shell>
      <main className="content">
        <h1>Incidents</h1>
        <div className="bar">
          <p>Review commands paused by the watchdog.</p>
          <label className="muted">
            <input
              type="checkbox"
              checked={all}
              onChange={(e) => setAll(e.target.checked)}
            />{" "}
            Show history
          </label>
        </div>
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
        {notice && <p role="status">{notice}</p>}
        {!items.length && <div className="empty">No incidents recorded.</div>}
        {items.map((item) => (
          <article className="card" key={item.id}>
            <div className="row">
              <span className="badge">{item.analysis.severity}</span>
              <strong className="grow">
                {item.analysis.category || "Watchdog incident"}
              </strong>
              <span>{item.review_state}</span>
              <span className="muted">{item.created_at}</span>
            </div>
            <p>{item.analysis.reason}</p>
            <pre>{item.flagged_output}</pre>
            <p>{item.analysis.suggested_action}</p>
            <Link href={terminalUrl(item.tmux_session, item.worker)}>
              Open {item.worker} / {item.tmux_session}
            </Link>
            {item.review_state === "pending_review" && (
              <div className="row" style={{ marginTop: 14 }}>
                <button
                  disabled={!!busy}
                  className="primary"
                  onClick={() => void decide(item, "resume")}
                >
                  Resume
                </button>
                <button
                  disabled={!!busy}
                  onClick={() => void decide(item, "resume_with_note")}
                >
                  Resume with note
                </button>
                <button
                  disabled={!!busy}
                  onClick={() => void decide(item, "modify_and_resume")}
                >
                  Modify and resume
                </button>
                <button
                  disabled={!!busy}
                  className="danger"
                  onClick={() => void decide(item, "abort")}
                >
                  Abort
                </button>
              </div>
            )}
          </article>
        ))}
      </main>
    </Shell>
  );
}

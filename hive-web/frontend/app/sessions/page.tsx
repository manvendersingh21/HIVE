"use client";
import { FormEvent, useEffect, useState } from "react";
import Link from "next/link";
import { api, request, terminalUrl } from "../../lib/api";
import { Shell } from "../../components/Nav";
import { Run, StateChip, sessionUrl } from "../../components/RunView";
import { stateTone } from "../../lib/runEvents";
type Session = {
  name: string;
  host: string;
  windows: number;
  attached: boolean;
  current_command: string;
  window_name: string;
  run?: Run;
};
// Delegated runs, grouped by what they need from a person.
const GROUPS: [string, (state: string) => boolean][] = [
  ["Needs attention", (s) => ["attention", "bad"].includes(stateTone(s))],
  ["Running", (s) => stateTone(s) === "running"],
  ["Queued", (s) => stateTone(s) === "queued"],
  ["Finished", (s) => ["done", "stale"].includes(stateTone(s))],
];
type Host = { host: string; name: string };
export default function SessionsPage() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [hosts, setHosts] = useState<Host[]>([
    { host: "local", name: "local" },
  ]);
  const [name, setName] = useState("");
  const [host, setHost] = useState("local");
  const [kind, setKind] = useState("shell");
  const [directory, setDirectory] = useState("");
  const [error, setError] = useState("");
  const [warning, setWarning] = useState("");
  const [busy, setBusy] = useState(false);
  const [loaded, setLoaded] = useState(false);
  async function load() {
    try {
      const response = await request("/api/sessions");
      setSessions(await response.json());
      setWarning(
        (
          JSON.parse(
            response.headers.get("x-hive-session-errors") || "[]",
          ) as string[]
        ).join("; "),
      );
      setHosts(await api<Host[]>("/api/session-hosts"));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoaded(true);
    }
  }
  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 4000);
    return () => clearInterval(timer);
  }, []);
  async function create(event: FormEvent) {
    event.preventDefault();
    if (busy || !name.trim()) return;
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name.trim())) {
      setError(
        "Use 1–64 letters, numbers, underscores or hyphens for the session name.",
      );
      return;
    }
    setBusy(true);
    setError("");
    try {
      await api("/api/sessions", {
        method: "POST",
        body: JSON.stringify({
          name: name.trim(),
          host,
          kind,
          working_dir: directory.trim() || undefined,
        }),
      });
      setName("");
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  async function kill(session: Session) {
    if (!confirm(`Kill ${session.host}/${session.name}?`)) return;
    setBusy(true);
    setError("");
    try {
      await api(
        `/api/sessions/${encodeURIComponent(session.name)}?host=${encodeURIComponent(session.host)}`,
        { method: "DELETE" },
      );
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  const agents = sessions.filter((s) => s.run);
  const terminals = sessions.filter((s) => !s.run && s.windows > 0);
  return (
    <Shell>
      <main className="content">
        <h1>Sessions</h1>
        <form className="card row" onSubmit={create} aria-label="Start a terminal">
          <label>
            Session name
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="session name"
              autoCapitalize="off"
              maxLength={64}
              required
            />
          </label>
          <label>
            Machine
            <select
              aria-label="Machine"
              value={host}
              onChange={(e) => setHost(e.target.value)}
            >
              {hosts.map((item) => (
                <option key={item.host} value={item.host}>
                  {item.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Session type
            <select
              aria-label="Session type"
              value={kind}
              onChange={(e) => setKind(e.target.value)}
            >
              <option value="claude">claude</option>
              <option value="codex">codex</option>
              <option value="shell">shell</option>
            </select>
          </label>
          <label>
            Working directory
            <input
              value={directory}
              onChange={(e) => setDirectory(e.target.value)}
              placeholder="Default directory"
            />
          </label>
          <button className="primary" disabled={busy}>
            {busy ? "Working…" : "Start"}
          </button>
        </form>
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
        {warning && (
          <p role="status" className="error">
            Some machines could not be listed: {warning}
          </p>
        )}
        <h2>Agent sessions</h2>
        {!loaded && <div className="empty">Loading sessions…</div>}
        {loaded && !agents.length && (
          <div className="empty">No agent sessions yet. Ask Hive to delegate work from the Agent tab.</div>
        )}
        {GROUPS.map(([label, match]) => {
          const group = agents.filter((s) => match(s.run!.state));
          if (!group.length) return null;
          return (
            <section key={label} className="session-group">
              <h3>
                {label} <span className="muted">{group.length}</span>
              </h3>
              <div className="session-cards">
                {group.map(({ run }) => (
                  <Link key={run!.id} href={sessionUrl(run!.id)} className="card session-card">
                    <div className="bar">
                      <strong>
                        {run!.assignment.agent} on {run!.assignment.device}
                      </strong>
                      <StateChip state={run!.state} />
                    </div>
                    <p className="clamp">{run!.assignment.objective}</p>
                    <span className="muted small mono">{run!.assignment.workspace}</span>
                  </Link>
                ))}
              </div>
            </section>
          );
        })}
        <h2>Terminals</h2>
        <div className="grid">
          {!terminals.length && (
            <div className="empty">
              {loaded ? "No terminal sessions. Start one above." : "Loading sessions…"}
            </div>
          )}
          {terminals.map((session) => (
            <article
              className="card row"
              key={`${session.host}/${session.name}`}
            >
              <span className={`dot ${session.attached ? "on" : ""}`} />
              <div className="grow">
                <strong>{session.name}</strong>
                <div className="muted">
                  {session.host} ·{" "}
                  {session.window_name || session.current_command}
                </div>
              </div>
              <Link
                className="button primary"
                href={terminalUrl(session.name, session.host)}
              >
                Open
              </Link>
              <button
                className="danger"
                disabled={busy}
                onClick={() => void kill(session)}
              >
                Kill
              </button>
            </article>
          ))}
        </div>
      </main>
    </Shell>
  );
}

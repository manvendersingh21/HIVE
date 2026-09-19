"use client";
import { FormEvent, useEffect, useState } from "react";
import Link from "next/link";
import { api, request, terminalUrl } from "../../lib/api";
import { Shell } from "../../components/Nav";
type Session = {
  name: string;
  host: string;
  windows: number;
  attached: boolean;
  current_command: string;
  window_name: string;
  run?: { state: string };
};
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
  return (
    <Shell>
      <main className="content">
        <h1>Sessions</h1>
        <form className="card row" onSubmit={create}>
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
        <div className="grid">
          {!sessions.length && (
            <div className="empty">
              {loaded ? "No tmux sessions." : "Loading sessions…"}
            </div>
          )}
          {sessions.map((session) => (
            <article
              className="card row"
              key={`${session.host}/${session.name}`}
            >
              <span className={`dot ${session.attached ? "on" : ""}`} />
              <div className="grow">
                <strong>{session.name}</strong>
                <div className="muted">
                  {session.host} ·{" "}
                  {session.window_name || session.current_command}{" "}
                  {session.run && `· ${session.run.state}`}
                </div>
              </div>
              {session.windows > 0 ? (
                <>
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
                </>
              ) : (
                <span className="muted">Terminal unavailable</span>
              )}
            </article>
          ))}
        </div>
      </main>
    </Shell>
  );
}

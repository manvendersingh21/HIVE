"use client";
import { FormEvent, useEffect, useRef, useState } from "react";
import { api, terminalUrl } from "../lib/api";

type Registered = {
  name: string;
  host: string;
  container: string;
  managed: boolean;
  reachable: boolean;
  missing?: string[];
  agents?: string[];
  error?: string;
};
type Available = { container: string; image: string; status: string; running: boolean };

/// Settings → Containers: Docker containers Hive can run agents in. Adding
/// and removing only change Hive's list; the container itself is untouched.
export function Containers() {
  const [items, setItems] = useState<Registered[]>();
  const [hosts, setHosts] = useState<string[]>([]);
  const [host, setHost] = useState("");
  const currentHost = useRef(host);
  currentHost.current = host;
  const [available, setAvailable] = useState<Available[]>();
  const [picked, setPicked] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState("");

  async function load() {
    setItems(await api<Registered[]>("/api/containers"));
  }
  useEffect(() => {
    void load().catch((e) => setError((e as Error).message));
    void api<string[]>("/api/containers/hosts")
      .then((list) => {
        setHosts(list);
        setHost((current) => current || list[0] || "");
      })
      .catch((e) => setError((e as Error).message));
  }, []);

  async function browse() {
    setError("");
    setNotice("");
    setAvailable(undefined);
    setPicked("");
    setBusy(true);
    const wanted = host;
    try {
      const list = await api<Available[]>(
        `/api/containers/available?host=${encodeURIComponent(wanted)}`,
      );
      // A list for another machine would add its container under this one.
      if (currentHost.current === wanted) setAvailable(list);
    } catch (e) {
      if (currentHost.current === wanted)
        setError(`Docker on ${wanted}: ${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  }

  async function add(event: FormEvent) {
    event.preventDefault();
    if (!picked || !name.trim()) return;
    setError("");
    setNotice("");
    setBusy(true);
    try {
      await api("/api/containers", {
        method: "POST",
        body: JSON.stringify({ name: name.trim(), host, container: picked }),
      });
      setNotice(`Added ${name.trim()}. Hive is checking which agents it has.`);
      setPicked("");
      setName("");
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  // A container Hive created is deleted with it, so that asks once in the page.
  async function remove(item: Registered) {
    setError("");
    setNotice("");
    if (item.managed && confirming !== item.name) {
      setConfirming(item.name);
      return;
    }
    setConfirming("");
    setBusy(true);
    try {
      await api(
        `/api/containers/${encodeURIComponent(item.name)}${item.managed ? "?delete=1" : ""}`,
        { method: "DELETE" },
      );
      setNotice(
        item.managed
          ? `Deleted ${item.name} and its container.`
          : `Removed ${item.name} from Hive. The container itself is still there.`,
      );
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  // A shell inside the container, for logging agents in or installing tools.
  async function shell(item: Registered) {
    setError("");
    const session = `shell-${Date.now().toString(36)}`;
    try {
      await api("/api/sessions", {
        method: "POST",
        body: JSON.stringify({ name: session, host: item.name, kind: "shell" }),
      });
      window.location.assign(terminalUrl(session, item.name));
    } catch (e) {
      setError((e as Error).message);
    }
  }

  return (
    <section className="card" aria-labelledby="containers-heading">
      <h2 id="containers-heading">Containers</h2>
      <p className="muted">
        Docker containers Hive can run agents in, like any other machine. A container needs
        python3 (3.10 or newer), tmux and the agent CLIs, each logged in.
      </p>
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
      {notice && <p role="status">{notice}</p>}
      {!items ? (
        <p className="muted">Loading containers…</p>
      ) : !items.length ? (
        <div className="empty">No containers added.</div>
      ) : (
        items.map((item) => (
          <article className="card" key={item.name} data-container={item.name}>
            <div className="row">
              <span className={`dot ${item.reachable ? "on" : ""}`} />
              <strong>{item.name}</strong>
              <span className="muted mono">
                {item.container} on {item.host}
              </span>
              <span className="grow" />
              <button disabled={busy || !item.reachable} onClick={() => void shell(item)}>
                Open shell
              </button>
              <button className="danger" disabled={busy} onClick={() => void remove(item)}>
                {confirming === item.name ? "Delete container" : "Remove"}
              </button>
              {confirming === item.name && (
                <button className="ghost" onClick={() => setConfirming("")}>
                  Cancel
                </button>
              )}
            </div>
            {item.managed && (
              <p className="muted small">Managed by Hive · created by Hive&apos;s planner</p>
            )}
            {confirming === item.name && (
              <p className="error small">
                This deletes the container {item.container} on {item.host} and everything
                inside it, including agent workspaces. Press Delete container to confirm.
              </p>
            )}
            {!item.reachable ? (
              <p className="error small">{item.error || "Not reachable"}</p>
            ) : (
              <p className="muted small">
                {item.agents?.length ? `Agents: ${item.agents.join(", ")}` : "No agent CLIs found"}
                {!!item.missing?.length && ` · Missing: ${item.missing.join(", ")}`}
              </p>
            )}
          </article>
        ))
      )}
      <h3>Add a container</h3>
      <div className="row">
        <label>
          Machine
          <select
            aria-label="Docker machine"
            value={host}
            disabled={busy}
            onChange={(e) => {
              setHost(e.target.value);
              setAvailable(undefined);
            }}
          >
            {hosts.map((h) => (
              <option key={h} value={h}>
                {h}
              </option>
            ))}
          </select>
        </label>
        <button disabled={busy || !host} onClick={() => void browse()}>
          {busy && !available ? "Listing…" : "List containers"}
        </button>
      </div>
      {available && !available.length && (
        <div className="empty">No containers on {host}.</div>
      )}
      {available && available.length > 0 && (
        <form onSubmit={add}>
          {available.map((c) => (
            <label key={c.container} className="row">
              <input
                type="radio"
                name="container"
                value={c.container}
                checked={picked === c.container}
                disabled={!c.running}
                onChange={() => {
                  setPicked(c.container);
                  setName((current) => current || c.container);
                }}
              />
              <span className="mono">{c.container}</span>
              <span className="muted small">
                {c.image} · {c.status}
              </span>
            </label>
          ))}
          <label>
            Name in Hive
            <input
              aria-label="Name in Hive"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. dev-box"
              maxLength={63}
            />
          </label>
          <button className="primary" disabled={busy || !picked || !name.trim()}>
            Add container
          </button>
        </form>
      )}
    </section>
  );
}

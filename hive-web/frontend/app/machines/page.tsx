"use client";
import { useEffect, useState } from "react";
import { api, apiText } from "../../lib/api";
import { Shell } from "../../components/Nav";
type Machine = {
  name: string;
  host?: string;
  reachable?: boolean;
  os?: string;
  arch?: string;
  cpu_cores?: number;
  memory_gb?: number;
  tools?: string[];
  tags?: string[];
};
type Graph = {
  entities: {
    id: string;
    kind: string;
    name: string;
    attrs: Omit<Machine, "name">;
  }[];
  edges: { from: string; to: string; relation: string }[];
};
export default function MachinesPage() {
  const [machines, setMachines] = useState<Machine[]>([]);
  const [prompt, setPrompt] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [loaded, setLoaded] = useState(false);
  async function load() {
    const graph = await api<Graph>("/api/machines");
    setMachines(
      graph.entities
        .filter((e) => e.kind === "machine")
        .map((e) => ({
          ...e.attrs,
          name: e.name,
          tools: graph.edges
            .filter(
              (edge) => edge.from === e.id && edge.relation === "has_tool",
            )
            .map(
              (edge) =>
                graph.entities.find((node) => node.id === edge.to)?.name ||
                edge.to,
            ),
        })),
    );
    setPrompt(await apiText("/api/machines/prompt"));
  }
  useEffect(() => {
    void load()
      .catch((e) => setError(e.message))
      .finally(() => setLoaded(true));
  }, []);
  async function refresh() {
    setBusy(true);
    setError("");
    try {
      await api("/api/machines/refresh", { method: "POST" });
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
        <h1>Machines</h1>
        <div className="bar">
          <p>The machines Hive can use for your work.</p>
          <button disabled={busy} onClick={() => void refresh()}>
            {busy ? "Probing…" : "Re-probe"}
          </button>
        </div>
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
        {!machines.length ? (
          <div className="empty">
            {loaded ? "No machine data yet." : "Loading machines…"}
          </div>
        ) : (
          machines.map((machine) => (
            <article className="card" key={machine.name}>
              <div className="row">
                <span className={`dot ${machine.reachable ? "on" : ""}`} />
                <strong>{machine.name}</strong>
                <span>{machine.reachable ? "Online" : "Unreachable"}</span>
                <span className="muted">{machine.host}</span>
              </div>
              <p className="muted">
                {[
                  machine.os,
                  machine.arch,
                  machine.cpu_cores && `${machine.cpu_cores} cores`,
                  machine.memory_gb && `${machine.memory_gb} GB RAM`,
                ]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
              <div className="row">
                {[
                  ...new Set([
                    ...(machine.tools || []),
                    ...(machine.tags || []),
                  ]),
                ].map((value) => (
                  <span className="pill" key={value}>
                    {value}
                  </span>
                ))}
              </div>
            </article>
          ))
        )}
        {prompt && (
          <details className="card">
            <summary>Agent prompt preview</summary>
            <pre>{prompt}</pre>
          </details>
        )}
      </main>
    </Shell>
  );
}

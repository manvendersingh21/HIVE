"use client";
import { useCallback, useState } from "react";
import Link from "next/link";
import { api } from "../lib/api";
import { usePoll } from "../lib/poll";
import { sessionUrl } from "./RunView";

type Profile = {
  agent_id: string;
  key: string;
  role: string;
  agent: string;
  model: string | null;
  device: string;
  owned_paths: string[];
  current_task: string;
  status: string;
  dependencies: string[];
  last_seen: string | null;
  relay_fingerprint: string;
};

export function TeamPanel({ taskId, currentId, select }: {
  taskId: string; currentId: string; select: (id: string) => void;
}) {
  const [team, setTeam] = useState<Profile[]>();
  const [error, setError] = useState("");
  const load = useCallback(async () => {
    try {
      setTeam(await api<Profile[]>(`/api/tasks/${encodeURIComponent(taskId)}/team`));
      setError("");
    } catch (e) { setError((e as Error).message); }
  }, [taskId]);
  usePoll(load, 3000, [load]);
  return <section className="card" aria-label="Team">
    <h3>Team</h3>
    {error && <p role="alert">{error}</p>}
    {!team && !error && <p className="muted">Loading team…</p>}
    {team?.length === 0 && <p className="muted">No teammates.</p>}
    {team?.map((p) => <article key={p.agent_id}>
      <Link href={sessionUrl(p.agent_id)} onClick={() => select(p.agent_id)}
        aria-current={p.agent_id === currentId ? "page" : undefined}>{p.role}</Link>
      <p>{p.agent} on {p.device} · {p.status}</p>
      <dl>
        <dt>Owned paths</dt><dd>{p.owned_paths.length ? p.owned_paths.join(", ") : "None assigned"}</dd>
        <dt>Dependencies</dt><dd>{p.dependencies.length ? p.dependencies.join(", ") : "None"}</dd>
      </dl>
    </article>)}
  </section>;
}

"use client";
import { useCallback, useState } from "react";
import { api } from "../lib/api";
import { usePoll } from "../lib/poll";
import { Run } from "./RunView";

type Audit = {
  mode: string;
  chain_valid: boolean;
  total_entries: number;
  head: { position: number; digest: string };
  entries: { digest: string; record: {
    position: number; event: string; at: string; message_id: string;
    seq: number; detail: { reason?: string };
  } }[];
};

export function RelayAudit({ run }: { run: Run }) {
  const [audit, setAudit] = useState<Audit>();
  const [error, setError] = useState("");
  const load = useCallback(async () => {
    try {
      setAudit(await api<Audit>(`/api/runs/${encodeURIComponent(run.id)}/audit`));
      setError("");
    } catch (e) { setError((e as Error).message); }
  }, [run.id]);
  usePoll(load, 5000, [load]);
  return (
    <section className="card relay-audit" aria-label="Relay audit">
      <h3>Relay-attested (HACP Secure degraded mode)</h3>
      <p className="muted small">Attests messages after the coordinator receives them. It does not attest an agent&apos;s reasoning or protect its local journal.</p>
      {run.identity && <details>
        <summary>Run public identity</summary>
        <dl><dt>Ed25519 public key</dt><dd className="mono">{run.identity.public_key}</dd>
          <dt>SHA-256 fingerprint</dt><dd className="mono">{run.identity.fingerprint}</dd></dl>
      </details>}
      {error ? <p className="error" role="alert">Audit unavailable: {error}</p> : audit ? <>
        <p role="status" className={audit.chain_valid ? "muted" : "error"}>
          {audit.chain_valid ? "Audit chain verified" : "Audit chain integrity failure"}
        </p>
        <details><summary>Recent audit events ({audit.total_entries})</summary>
          {audit.entries.length === 0 ? <p className="muted">No relay messages yet.</p> : <ol>
            {audit.entries.map(({ record, digest }) => <li key={record.position}>
              <strong>{record.event}</strong> · {record.at} · sequence {record.seq}
              <div className="mono small">{record.message_id}</div>
              {record.detail.reason && <p>{record.detail.reason}</p>}
              <details><summary>Row digest</summary><code>{digest}</code></details>
            </li>)}
          </ol>}
          <p className="muted small">Showing up to 100 events for this run. Verification covers the full coordinator audit chain.</p>
        </details>
      </> : <p className="muted">Loading relay audit…</p>}
    </section>
  );
}

import Link from "next/link";
import { Run, sessionUrl } from "./RunView";

export type Agreement = {
  contract: {
    contract_id: string;
    participants: string[];
    state: string;
    revisions: { number: number; digest: string; content: { agreement?: string } }[];
  };
  proposed_digest: string;
  proposed_terms: { agreement?: string };
};

export function Coordination({ run, siblings, onSelect }: { run: Run; siblings: Run[]; onSelect: (id: string) => void }) {
  return <>
    {!!run.contracts?.length && <section className="card coordination" aria-label="Interface contracts">
      <h3>Interface contracts</h3>
      {run.contracts.map((record) => {
        const contract = record.contract;
        const frozen = contract.revisions.at(-1);
        const pending = contract.state === "amending" || !frozen;
        return <div key={contract.contract_id}>
          <p>{contract.participants.map((party, index) => {
            const id = party.replace(/^urn:hacp:agent:/, "");
            const peer = siblings.find((s) => s.id === id);
            return <span key={party}>{index > 0 && " ↔ "}<Link href={sessionUrl(id)} onClick={() => onSelect(id)}>{peer?.assignment.key || id}</Link></span>;
          })}</p>
          {frozen && <>
            <strong>Frozen revision {frozen.number}</strong>
            <p>{frozen.content.agreement}</p>
            <div className="small muted">HACP v2 frozen digest</div>
            <code className="contract-digest">{frozen.digest}</code>
          </>}
          {pending && <details>
            <summary>{frozen ? "Amendment pending both parties" : "Proposal awaiting agreement"}</summary>
            <p>{record.proposed_terms.agreement}</p>
            <div className="small muted">Proposal digest to acknowledge</div>
            <code className="contract-digest">{record.proposed_digest}</code>
          </details>}
        </div>;
      })}
    </section>}
    {run.completion && <section className="card coordination" aria-label="Acceptance evidence">
      <h3>Acceptance evidence</h3>
      <p>{({ accept: "Accepted", rework: "Rework requested", no_agreement: "No agreement" })[run.completion.record.verdict]}</p>
      <p className="small muted">Rework rounds: {run.completion.record.rework_rounds} / {run.assignment.max_rework ?? 2}</p>
      {run.completion.measurements.map((check, index) => <details key={index}>
        <summary>Check {index + 1}: {check.passed ? "passed" : "failed"}</summary>
        <pre>{check.detail}</pre>
      </details>)}
      <details><summary>Verdict history</summary>
        {run.completion.record.evidence.map((evidence, index) => <pre key={index}>{evidence}</pre>)}
      </details>
    </section>}
  </>;
}

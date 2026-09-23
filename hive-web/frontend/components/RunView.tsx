"use client";
import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { api, requestId, terminalUrl } from "../lib/api";
import {
  Entry,
  RunEvent,
  TERMINAL_STATES,
  describeAction,
  lastError,
  stateTone,
  transcript,
} from "../lib/runEvents";
import { Markdown } from "../lib/markdown";

export type Approval = {
  id: string;
  fingerprint: string;
  consumed: number;
  decision?: string | null;
  reason?: string;
  action?: unknown;
  details?: { changes?: { path?: string }[] } | null;
};
export type Run = {
  id: string;
  task_id: string;
  conversation_id: string;
  tmux_name: string;
  state: string;
  runner_path?: string | null;
  assignment: {
    key: string;
    device: string;
    agent: string;
    model?: string | null;
    objective: string;
    workspace: string;
    dependencies?: string[];
    acceptance_criteria?: string[];
  };
  metadata?: {
    error?: string;
    actual_model?: string;
    approvals?: Approval[];
  };
  review?: { status?: string; summary?: string } | null;
};

export const sessionUrl = (id: string) => `/session/?run=${encodeURIComponent(id)}`;

const STATE_LABELS: Record<string, string> = {
  "awaiting-approval": "Needs approval",
  "needs-setup": "Needs setup",
  "waiting-for-peer": "Waiting for peer",
};
export function StateChip({ state }: { state: string }) {
  return (
    <span className={`chip ${stateTone(state)}`} data-state={state}>
      {STATE_LABELS[state] || state}
    </span>
  );
}

async function post(run: Run, path: string, body: unknown = {}) {
  await api(`/api/runs/${encodeURIComponent(run.id)}/${path}`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/// Journal events for a run: the latest `tail` first, then new ones as they
/// arrive while `live` is set.
export function useRunEvents(run: Run | undefined, tail: number, live: boolean) {
  const [events, setEvents] = useState<RunEvent[]>();
  const [error, setError] = useState("");
  const last = useRef<number | undefined>(undefined);
  const id = run?.id;
  useEffect(() => {
    last.current = undefined;
    setEvents(undefined);
  }, [id]);
  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const after = last.current;
        const url = `/api/runs/${encodeURIComponent(id!)}/events?${
          after === undefined ? `tail=${tail}` : `after=${after}`
        }`;
        const next = await api<RunEvent[]>(url);
        if (cancelled) return;
        if (next.length) last.current = next.at(-1)!.seq;
        else if (after === undefined) last.current = 0;
        setEvents((current) =>
          after === undefined ? next : [...(current || []), ...next],
        );
        setError("");
        // A full page means more are waiting; fetch them straight away.
        if (after !== undefined && next.length === 300) return void poll();
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      }
      if (!cancelled && live) timer = setTimeout(() => void poll(), 2000);
    }
    void poll();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [id, tail, live]);
  // Opening at the tail skips older history; page back through it on request.
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  const first = events?.[0]?.seq;
  async function loadEarlier() {
    if (!id || first === undefined || loadingEarlier) return;
    setLoadingEarlier(true);
    try {
      const page = await api<RunEvent[]>(
        `/api/runs/${encodeURIComponent(id)}/events?after=${Math.max(0, first - 301)}`,
      );
      const older = page.filter((e) => e.seq < first);
      setEvents((current) => [...older, ...(current || [])]);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoadingEarlier(false);
    }
  }
  return {
    events,
    error,
    earlier: first !== undefined && first > 1 ? loadEarlier : undefined,
    loadingEarlier,
  };
}

/// The one approval card: who wants what, the exact command, and the choice.
/// Chat steps and delegated runs both render through it.
export function ApprovalPrompt({
  title,
  reason,
  command,
  cwd,
  status,
  denyLabel,
  busy,
  error,
  onApprove,
  onDeny,
}: {
  title: string;
  reason?: string;
  command: string;
  cwd?: string;
  status?: React.ReactNode;
  denyLabel: string;
  busy: boolean;
  error?: string;
  onApprove: () => void;
  onDeny: () => void;
}) {
  return (
    <div className="approval">
      <div className="approval-head">
        <strong>{title}</strong>
        {reason && <span className="muted">{reason}</span>}
      </div>
      <pre className="cmd">{command}</pre>
      {cwd && <div className="muted mono small">in {cwd}</div>}
      {status || (
        <div className="row">
          <button className="primary" disabled={busy} onClick={onApprove}>
            Approve
          </button>
          <button className="danger" disabled={busy} onClick={onDeny}>
            {denyLabel}
          </button>
        </div>
      )}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </div>
  );
}

function ApprovalCard({
  run,
  approval,
  refresh,
}: {
  run: Run;
  approval: Approval;
  refresh: () => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const { command, cwd } = describeAction(approval.action, approval.details);
  const verb = command.startsWith("Edit ") || command === "Apply file changes" ? "change files" : "run a command";
  async function decide(decision: "continue" | "stop") {
    setBusy(true);
    setError("");
    try {
      await post(run, "decisions", {
        id: approval.id,
        fingerprint: approval.fingerprint,
        decision,
      });
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <ApprovalPrompt
      title={`${run.assignment.agent} wants to ${verb}`}
      reason={approval.reason}
      command={command}
      cwd={cwd}
      status={
        run.state === "disconnected" ? (
          <p className="muted">
            The agent disconnected before this was answered, so approving now has no effect.
          </p>
        ) : approval.decision ? (
          <p role="status" className="muted">
            You chose <strong>{approval.decision === "stop" ? "deny" : "approve"}</strong>
            . Waiting for the agent to pick it up…
          </p>
        ) : undefined
      }
      denyLabel="Deny and stop"
      busy={busy}
      error={error}
      onApprove={() => void decide("continue")}
      onDeny={() => void decide("stop")}
    />
  );
}

/// What a person must know or do right now: pending approvals, the failure
/// reason, what a queued run is waiting for.
export function RunAttention({
  run,
  siblings,
  events,
  refresh,
}: {
  run: Run;
  siblings: Run[];
  events?: RunEvent[];
  refresh: () => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const pending = (run.metadata?.approvals || []).filter((a) => !a.consumed);
  const failure =
    run.metadata?.error || (events && run.state === "failed" ? lastError(events) : undefined);
  const waitingOn = (run.assignment.dependencies || []).map((key) => ({
    key,
    run: siblings.find((s) => s.assignment.key === key),
  }));
  async function retry() {
    setBusy(true);
    setError("");
    try {
      await post(run, "retry-setup");
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <>
      {pending.map((approval) => (
        <ApprovalCard key={approval.id} run={run} approval={approval} refresh={refresh} />
      ))}
      {failure && (
        <div className="banner bad" role="alert">
          <strong>Why it failed</strong>
          <div>{failure}</div>
        </div>
      )}
      {run.state === "queued" && waitingOn.length > 0 && (
        <div className="banner">
          <strong>Waiting to start</strong>
          <div>
            Starts after{" "}
            {waitingOn.map(({ key, run: dep }, i) => (
              <span key={key}>
                {i > 0 && ", "}
                {dep ? (
                  <Link href={sessionUrl(dep.id)}>
                    {dep.assignment.agent} on {dep.assignment.device}
                  </Link>
                ) : (
                  <code>{key}</code>
                )}{" "}
                {dep && (
                  <>
                    (<StateChip state={dep.state} />)
                  </>
                )}
              </span>
            ))}{" "}
            completes.
          </div>
        </div>
      )}
      {!run.runner_path && ["needs-setup", "disconnected"].includes(run.state) && (
        <div className="banner">
          <strong>The agent never launched on {run.assignment.device}</strong>
          <div className="row">
            <button disabled={busy} onClick={() => void retry()}>
              Retry setup
            </button>
          </div>
        </div>
      )}
      {run.review?.summary && (
        <div className="banner">
          <strong>Review: {run.review.status}</strong>
          <div>{run.review.summary}</div>
        </div>
      )}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </>
  );
}

// Claude's Read and Write can return whole files; keep the transcript light.
const LIMIT = 20000;
const clip = (text: string) =>
  text.length > LIMIT ? `${text.slice(0, LIMIT)}\n… truncated; Raw events has the rest` : text;

function EntryView({ entry }: { entry: Entry }) {
  switch (entry.type) {
    case "state":
      return (
        <div className="t-state">
          <StateChip state={entry.state} />
        </div>
      );
    case "prompt":
      return (
        <details className="t-prompt">
          <summary>Instructions sent to the agent</summary>
          <div className="t-text">{entry.text}</div>
        </details>
      );
    case "agent":
      return <Markdown className="t-agent" text={entry.text} />;
    case "reasoning":
      return (
        <details className="t-muted">
          <summary>Thinking</summary>
          <div className="t-text">{entry.text}</div>
        </details>
      );
    case "command":
      return (
        <details className="t-cmd" open={!!entry.exitCode || entry.status === "failed"}>
          <summary>
            <span className="mono">$ {entry.command.split("\n")[0]}</span>
            {entry.exitCode != null ? (
              <span className={`exit ${entry.exitCode ? "bad" : ""}`}>
                exit {entry.exitCode}
              </span>
            ) : (
              entry.status === "failed" && <span className="exit bad">failed</span>
            )}
          </summary>
          {entry.command.includes("\n") && <pre className="cmd">{entry.command}</pre>}
          {entry.cwd && <div className="muted mono small">in {entry.cwd}</div>}
          {entry.output && <pre>{clip(entry.output)}</pre>}
        </details>
      );
    case "files":
      return (
        <details className="t-cmd">
          <summary>
            Changed {entry.changes.length} file{entry.changes.length === 1 ? "" : "s"}:{" "}
            <span className="mono">
              {entry.changes.map((c) => c.path.split("/").pop()).join(", ")}
            </span>
          </summary>
          {entry.changes.map((c) => (
            <div key={c.path}>
              <div className="muted mono small">
                {c.kind} {c.path}
              </div>
              {c.diff && <pre>{clip(c.diff)}</pre>}
            </div>
          ))}
        </details>
      );
    case "tool":
      return (
        <details className="t-cmd">
          <summary>
            Tool <span className="mono">{entry.name}</span>
            {entry.error ? (
              <span className="exit bad">{entry.error}</span>
            ) : (
              entry.status && <span className="exit">{entry.status}</span>
            )}
          </summary>
          {entry.input && <pre>{clip(entry.input)}</pre>}
          {entry.output && <pre>{clip(entry.output)}</pre>}
        </details>
      );
    case "approval":
      return (
        <div className="t-approval">
          <strong>Asked to run:</strong> <span className="mono">{entry.command.split("\n")[0]}</span>
          {entry.reason && <div className="muted small">{entry.reason}</div>}
        </div>
      );
    case "approval-resolved":
      return (
        <div className="t-state muted small">
          {entry.decision === "stop" ? "Denied" : "Approved"}
        </div>
      );
    case "error":
      return <div className="t-error">{entry.text}</div>;
    case "result":
      return (
        entry.error ? (
          <div className="t-error">{entry.text}</div>
        ) : (
          <Markdown className="t-result" text={entry.text} />
        )
      );
  }
}

export function Transcript({
  events,
  earlier,
  loadingEarlier,
}: {
  events?: RunEvent[];
  earlier?: () => Promise<void>;
  loadingEarlier?: boolean;
}) {
  const [raw, setRaw] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const entries = events ? transcript(events) : [];
  const onScroll = useCallback((e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }, []);
  // Follow new activity inside the box only; the page itself stays put.
  useEffect(() => {
    if (stick.current && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [entries.length]);
  return (
    <div className="transcript" onScroll={onScroll} ref={box}>
      <div className="row transcript-bar">
        <strong className="grow">Activity</strong>
        <label className="muted small row">
          <input type="checkbox" checked={raw} onChange={(e) => setRaw(e.target.checked)} />
          Raw events
        </label>
      </div>
      {earlier && (
        <div className="t-state">
          <button className="ghost small" disabled={loadingEarlier} onClick={() => {
            stick.current = false;
            void earlier();
          }}>
            {loadingEarlier ? "Loading…" : "Show earlier activity"}
          </button>
        </div>
      )}
      {events === undefined && <p className="muted">Loading activity…</p>}
      {events && !entries.length && !raw && (
        <p className="muted">No agent activity yet.</p>
      )}
      {raw ? (
        <pre>{JSON.stringify(events, null, 2)}</pre>
      ) : (
        entries.map((entry, i) => <EntryView key={`${entry.seq}-${i}`} entry={entry} />)
      )}
    </div>
  );
}

export function RunComposer({ run, refresh }: { run: Run; refresh: () => Promise<void> }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const closed = run.state === "superseded";
  async function submit(e: FormEvent) {
    e.preventDefault();
    const draft = text;
    if (busy || !draft.trim() || closed) return;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await post(run, "messages", { id: requestId(), text: draft.trim() });
      // Keep anything typed while the request was in flight.
      setText((current) => (current === draft ? "" : current));
      setNotice("Sent. The agent receives it on its next turn.");
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <form className="run-composer" onSubmit={submit}>
      <textarea
        aria-label={`Message ${run.assignment.agent} on ${run.assignment.device}`}
        placeholder={`Message ${run.assignment.agent} on ${run.assignment.device}…`}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            e.currentTarget.form?.requestSubmit();
          }
        }}
        maxLength={16000}
        rows={2}
        disabled={closed}
      />
      <button className="primary" disabled={busy || !text.trim() || closed}>
        {busy ? "Sending…" : "Send"}
      </button>
      {notice && (
        <p role="status" className="muted small full">
          {notice}
        </p>
      )}
      {error && (
        <p role="alert" className="error full">
          {error}
        </p>
      )}
    </form>
  );
}

export function RunTitle({ run }: { run: Run }) {
  const model = run.metadata?.actual_model || run.assignment.model;
  return (
    <div className="bar">
      <div className="row">
        <strong>
          {run.assignment.agent} on {run.assignment.device}
        </strong>
        {model && <span className="pill mono">{model}</span>}
      </div>
      <StateChip state={run.state} />
    </div>
  );
}

/// The chat's inline view of a run: status and anything needing a decision,
/// with the full session one click away.
export function RunSummary({
  run,
  siblings,
  refresh,
}: {
  run: Run;
  siblings: Run[];
  refresh: () => Promise<void>;
}) {
  // Enough history for the failure reason and latest message; the session
  // page streams the rest.
  const { events } = useRunEvents(run, 60, !TERMINAL_STATES.includes(run.state));
  const latest = events
    ? transcript(events).filter((e) => e.type === "agent").at(-1)
    : undefined;
  return (
    <article className="card run-card">
      <RunTitle run={run} />
      <p className="clamp">{run.assignment.objective}</p>
      <RunAttention run={run} siblings={siblings} events={events} refresh={refresh} />
      {latest && latest.type === "agent" && (
        <p className="latest clamp">
          <span className="muted">Latest: </span>
          {latest.text}
        </p>
      )}
      <div className="row">
        <Link className="button primary" href={sessionUrl(run.id)}>
          Open session
        </Link>
        <span className="muted mono small">{run.assignment.workspace}</span>
      </div>
    </article>
  );
}

export function killSession(run: Run) {
  return api(
    `/api/sessions/${encodeURIComponent(run.tmux_name)}?host=${encodeURIComponent(run.assignment.device)}`,
    { method: "DELETE" },
  );
}
export const rawTerminalUrl = (run: Run) => terminalUrl(run.tmux_name, run.assignment.device);

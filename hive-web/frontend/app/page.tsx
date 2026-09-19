"use client";
import { FormEvent, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { api, requestId, terminalUrl } from "../lib/api";
import { Shell } from "../components/Nav";
type Chat = { id: string; title?: string; updated_at?: string };
type Run = {
  id: string;
  tmux_name: string;
  state: string;
  runner_path?: string;
  assignment: {
    device: string;
    agent: string;
    objective: string;
    workspace: string;
  };
  metadata?: {
    error?: string;
    approvals?: {
      id: string;
      fingerprint: string;
      consumed: number;
      [key: string]: unknown;
    }[];
  };
  review?: { status?: string; summary?: string };
};
type Reply = {
  run?: {
    id: string;
    steps: {
      id: number;
      command: string;
      target: { kind: string; worker?: string };
      risk?: { reason: string };
    }[];
  };
  result?: {
    awaiting_approval: number[];
    sessions: { session_name: string; worker_name: string }[];
  };
  delegation?: { runs: Run[] };
};
type Message = {
  id?: number;
  role: string;
  content: string;
  status?: string;
  reply?: Reply;
};
type ChatData = { messages: Message[] };
const running = (messages: Message[]) =>
  ["planning", "executing", "awaiting_approval"].includes(
    messages.at(-1)?.status || "",
  );

function RunCard({ run, refresh }: { run: Run; refresh: () => Promise<void> }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [events, setEvents] = useState<unknown>();
  async function act(path: string, body?: unknown) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await api(`/api/runs/${encodeURIComponent(run.id)}/${path}`, {
        method: "POST",
        body: JSON.stringify(body || {}),
      });
      setNotice("Request saved.");
      await refresh();
      return true;
    } catch (e) {
      setError((e as Error).message);
      return false;
    } finally {
      setBusy(false);
    }
  }
  async function inspect() {
    setError("");
    try {
      setEvents(await api(`/api/runs/${encodeURIComponent(run.id)}/events`));
    } catch (e) {
      setError((e as Error).message);
    }
  }
  return (
    <article className="card run-card">
      <div className="bar">
        <strong>
          {run.assignment.agent} on {run.assignment.device}
        </strong>
        <span className="badge">{run.state}</span>
      </div>
      <p>{run.assignment.objective}</p>
      <p className="muted">{run.assignment.workspace}</p>
      <div className="row">
        <Link
          className="button"
          href={terminalUrl(run.tmux_name, run.assignment.device)}
        >
          Open terminal
        </Link>
        <button onClick={() => void inspect()}>Refresh events</button>
        {!run.runner_path &&
          ["needs-setup", "disconnected"].includes(run.state) && (
            <button disabled={busy} onClick={() => void act("retry-setup")}>
              Retry setup
            </button>
          )}
      </div>
      {run.review?.summary && (
        <p>
          {run.review.status}: {run.review.summary}
        </p>
      )}
      {run.metadata?.error && <p className="error">{run.metadata.error}</p>}
      {run.metadata?.approvals
        ?.filter((a) => a.consumed === 0)
        .map((a) => (
          <div className="card" key={a.id}>
            <pre>{JSON.stringify(a, null, 2)}</pre>
            <button
              disabled={busy}
              onClick={() =>
                void act("decisions", {
                  id: a.id,
                  fingerprint: a.fingerprint,
                  decision: "continue",
                })
              }
            >
              Continue agent
            </button>{" "}
            <button
              disabled={busy}
              onClick={() =>
                void act("decisions", {
                  id: a.id,
                  fingerprint: a.fingerprint,
                  decision: "stop",
                })
              }
            >
              Stop agent
            </button>
          </div>
        ))}
      {events !== undefined && (
        <details open>
          <summary>Agent events</summary>
          <pre>{JSON.stringify(events, null, 2)}</pre>
        </details>
      )}
      <form
        onSubmit={async (e: FormEvent) => {
          e.preventDefault();
          if (
            text.trim() &&
            (await act("messages", {
              id: requestId(),
              text: text.trim(),
            }))
          )
            setText("");
        }}
      >
        <label>
          Message {run.assignment.agent} on {run.assignment.device}
          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            maxLength={16000}
            rows={2}
          />
        </label>
        <button disabled={busy || !text.trim() || run.state === "superseded"}>
          Send to agent
        </button>
      </form>
      {notice && <p role="status">{notice}</p>}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </article>
  );
}
export default function AgentPage() {
  const [chats, setChats] = useState<Chat[]>([]);
  const [active, setActive] = useState<string>();
  const activeRef = useRef<string | undefined>(undefined);
  const [messages, setMessages] = useState<Message[]>([]);
  const [runs, setRuns] = useState<Run[]>([]);
  const [input, setInput] = useState("");
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0);
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [capable, setCapable] = useState(true);
  const searchVersion = useRef(0);
  async function loadChats(q = query, page = 0) {
    const version = ++searchVersion.current;
    try {
      const data = await api<Chat[]>(
        `/api/chats?q=${encodeURIComponent(q)}&offset=${page}`,
      );
      if (version === searchVersion.current) {
        setChats((current) => (page ? [...current, ...data] : data));
        setOffset(page);
      }
    } catch (e) {
      setError((e as Error).message);
    }
  }
  async function refreshRuns(id = activeRef.current) {
    if (!id) return;
    const data = await api<Run[]>(
      `/api/runs?conversation_id=${encodeURIComponent(id)}`,
    );
    if (activeRef.current === id) setRuns(data);
  }
  async function refresh(id: string) {
    const data = await api<ChatData>(`/api/chats/${encodeURIComponent(id)}`);
    if (activeRef.current === id) setMessages(data.messages);
  }
  async function open(id: string) {
    activeRef.current = id;
    setActive(id);
    setMessages([]);
    setRuns([]);
    setError("");
    setLoading(true);
    try {
      await refresh(id);
      await refreshRuns(id);
    } catch (e) {
      if (activeRef.current === id) setError((e as Error).message);
    } finally {
      if (activeRef.current === id) setLoading(false);
    }
  }
  useEffect(() => {
    void api<{ chat: boolean }>("/api/capabilities")
      .then((data) => {
        setCapable(data.chat);
        if (data.chat) void loadChats("", 0);
      })
      .catch((e) => setError(e.message));
  }, []);
  useEffect(() => {
    if (!capable) return;
    const timer = setTimeout(() => void loadChats(query, 0), 250);
    return () => clearTimeout(timer);
  }, [query, capable]);
  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        await refresh(active!);
        await refreshRuns(active!);
      } catch (e) {
        if (!cancelled && activeRef.current === active)
          setError((e as Error).message);
      }
      if (!cancelled) timer = setTimeout(() => void poll(), 2000);
    }
    timer = setTimeout(() => void poll(), 2000);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [active]);
  async function send() {
    const text = input.trim();
    if (!text || busy || loading || running(messages) || !capable) return;
    setBusy(true);
    setError("");
    let id = activeRef.current;
    try {
      if (!id) {
        const created = await api<Chat>("/api/chats", {
          method: "POST",
          body: "{}",
        });
        id = created.id;
        activeRef.current = id;
        setActive(id);
      }
      await api("/api/chat", {
        method: "POST",
        body: JSON.stringify({
          message: text,
          conversation_id: id,
          request_id: requestId(),
          background: true,
        }),
      });
      if (activeRef.current === id) {
        setInput("");
        await refresh(id);
      }
      await loadChats();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  async function approve(runId: string, stepId: number, allowed: boolean) {
    setBusy(true);
    setError("");
    const id = activeRef.current;
    try {
      await api(`/api/chat/${encodeURIComponent(runId)}/approve`, {
        method: "POST",
        body: JSON.stringify({
          approved: allowed ? [stepId] : [],
          denied: allowed ? [] : [stepId],
        }),
      });
      if (id) await refresh(id);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Shell>
      <div className="chat-layout">
        <aside className="chat-sidebar">
          <div className="row">
            <strong className="grow">History</strong>
            <button
              disabled={busy}
              onClick={() => {
                activeRef.current = undefined;
                setActive(undefined);
                setMessages([]);
                setRuns([]);
                setInput("");
                setError("");
                setLoading(false);
              }}
            >
              New chat
            </button>
          </div>
          <input
            aria-label="Search chats"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search chats"
          />
          {chats.map((chat) => (
            <button
              disabled={busy}
              className={`chat-item ${chat.id === active ? "selected" : ""}`}
              key={chat.id}
              onClick={() => void open(chat.id)}
            >
              <strong>{chat.title || "New chat"}</strong>
              <small>{chat.updated_at}</small>
            </button>
          ))}
          {chats.length >= offset + 50 && (
            <button onClick={() => void loadChats(query, offset + 50)}>
              Load more
            </button>
          )}
        </aside>
        <main className="chat-main">
          <div className="chat-feed">
            {!capable && (
              <p role="status">
                This host serves terminals only. Open{" "}
                <Link href="/sessions/">Sessions</Link> to connect.
              </p>
            )}
            {loading && <p role="status">Loading conversation…</p>}
            {!messages.length && !loading && capable && (
              <div className="empty">
                Ask Hive to run a command on a named machine, or ask two agents
                on two machines to collaborate.
              </div>
            )}
            {messages.map((message, index) => (
              <article
                className={`message ${message.role}`}
                key={message.id || index}
              >
                <span className="badge">
                  {message.role === "user" ? "You" : "Hive"}
                </span>{" "}
                {message.status && <small>{message.status}</small>}
                <div>{message.content}</div>
                {message.reply?.result?.sessions?.map((session) => (
                  <p key={`${session.worker_name}/${session.session_name}`}>
                    <Link
                      href={terminalUrl(
                        session.session_name,
                        session.worker_name,
                      )}
                    >
                      Open {session.worker_name} / {session.session_name}
                    </Link>
                  </p>
                ))}
                {message.status === "awaiting_approval" &&
                  message.reply?.run?.steps
                    .filter((step) =>
                      message.reply?.result?.awaiting_approval.includes(
                        step.id,
                      ),
                    )
                    .map((step) => (
                      <div className="card" key={step.id}>
                        <strong>
                          {step.target.worker || step.target.kind}
                        </strong>
                        <pre>{step.command}</pre>
                        <p>{step.risk?.reason}</p>
                        <button
                          disabled={busy}
                          className="primary"
                          onClick={() =>
                            void approve(message.reply!.run!.id, step.id, true)
                          }
                        >
                          Approve command
                        </button>{" "}
                        <button
                          disabled={busy}
                          onClick={() =>
                            void approve(message.reply!.run!.id, step.id, false)
                          }
                        >
                          Deny command
                        </button>
                      </div>
                    ))}
                {message.reply && (
                  <details>
                    <summary>Execution details</summary>
                    <pre>{JSON.stringify(message.reply, null, 2)}</pre>
                  </details>
                )}
              </article>
            ))}
            {runs.map((run) => (
              <RunCard key={run.id} run={run} refresh={() => refreshRuns()} />
            ))}
            {error && (
              <p role="alert" className="error">
                {error}
              </p>
            )}
          </div>
          <form
            className="composer"
            onSubmit={(event) => {
              event.preventDefault();
              void send();
            }}
          >
            <textarea
              aria-label="Message Hive"
              value={input}
              onChange={(event) => setInput(event.target.value)}
              onKeyDown={(event) => {
                if (
                  event.key === "Enter" &&
                  !event.shiftKey &&
                  !event.nativeEvent.isComposing
                ) {
                  event.preventDefault();
                  void send();
                }
              }}
              placeholder="Ask Hive…"
              rows={2}
              disabled={!capable}
            />
            <button
              className="primary"
              disabled={
                busy ||
                loading ||
                running(messages) ||
                !input.trim() ||
                !capable
              }
            >
              {busy ? "Sending…" : running(messages) ? "Running…" : "Send"}
            </button>
          </form>
        </main>
      </div>
    </Shell>
  );
}

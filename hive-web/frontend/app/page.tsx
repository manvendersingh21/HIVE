"use client";
import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { api, requestId, terminalUrl } from "../lib/api";
import { Shell } from "../components/Nav";
import { Run, RunSummary } from "../components/RunView";
type Chat = { id: string; title?: string; updated_at?: string };
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
  delegation?: { task_id?: string; runs: Run[] };
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
        const linked = new URLSearchParams(window.location.search).get("chat");
        if (data.chat && linked) void open(linked);
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
    const draft = input;
    const text = draft.trim();
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
        setInput((current) => current === draft ? "" : current);
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
  // Each turn's runs render under the reply that dispatched them; a run whose
  // turn isn't loaded still shows at the end.
  const runsFor = (message: Message) => {
    const task = message.reply?.delegation?.task_id;
    return task ? runs.filter((r) => r.task_id === task) : [];
  };
  const tasks = new Set(messages.map((m) => m.reply?.delegation?.task_id).filter(Boolean));
  const orphans = runs.filter((r) => !tasks.has(r.task_id));
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
                {runsFor(message).map((run) => (
                  <RunSummary
                    key={run.id}
                    run={run}
                    siblings={runsFor(message)}
                    refresh={() => refreshRuns()}
                  />
                ))}
                {message.reply && (
                  <details>
                    <summary>Execution details</summary>
                    <pre>{JSON.stringify(message.reply, null, 2)}</pre>
                  </details>
                )}
              </article>
            ))}
            {orphans.map((run) => (
              <RunSummary
                key={run.id}
                run={run}
                siblings={runs.filter((r) => r.task_id === run.task_id)}
                refresh={() => refreshRuns()}
              />
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

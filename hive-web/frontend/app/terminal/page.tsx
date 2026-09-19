"use client";
import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import type { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
export default function TerminalPage() {
  const [target, setTarget] = useState({ name: "", host: "local" });
  const [attempt, setAttempt] = useState(0);
  const container = useRef<HTMLDivElement>(null);
  const [state, setState] = useState("connecting");
  useEffect(() => {
    const query = new URLSearchParams(window.location.search);
    const name = query.get("name") || "";
    setTarget({ name, host: query.get("host") || "local" });
    if (!name) setState("Missing session name");
  }, []);
  const { name, host } = target;
  useEffect(() => {
    if (!name || !container.current) return;
    let ws: WebSocket | undefined;
    let disposed = false;
    let term: Terminal | undefined;
    let observer: ResizeObserver | undefined;
    setState("connecting");
    void (async () => {
      const [{ Terminal }, { FitAddon }] = await Promise.all([
        import("@xterm/xterm"),
        import("@xterm/addon-fit"),
      ]);
      if (disposed || !container.current) return;
      term = new Terminal({
        cursorBlink: true,
        fontSize: window.innerWidth < 700 ? 12 : 14,
        scrollback: 5000,
        theme: {
          background: "#0f1115",
          foreground: "#e6e8ee",
          cursor: "#f5a623",
        },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(container.current);
      fit.fit();
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(
        `${proto}://${location.host}/ws/${encodeURIComponent(name)}?host=${encodeURIComponent(host)}&cols=${term.cols}&rows=${term.rows}`,
      );
      ws.binaryType = "arraybuffer";
      const resize = () => {
        if (disposed) return;
        fit.fit();
        if (ws?.readyState === WebSocket.OPEN && term)
          ws.send(
            JSON.stringify({
              type: "resize",
              cols: term.cols,
              rows: term.rows,
            }),
          );
      };
      ws.onopen = () => {
        if (!disposed) {
          setState("connected");
          resize();
          term?.focus();
        }
      };
      ws.onclose = () => {
        if (!disposed) setState("disconnected");
      };
      ws.onerror = () => {
        if (!disposed)
          setState("Connection failed. Check the session and sign-in.");
      };
      ws.onmessage = (event) => {
        if (!disposed)
          term?.write(
            typeof event.data === "string"
              ? event.data
              : new Uint8Array(event.data),
          );
      };
      term.onData((data) => {
        if (ws?.readyState === WebSocket.OPEN)
          ws.send(new TextEncoder().encode(data));
      });
      observer = new ResizeObserver(resize);
      observer.observe(container.current);
    })().catch((e) => {
      if (!disposed) setState((e as Error).message);
    });
    return () => {
      disposed = true;
      observer?.disconnect();
      ws?.close();
      term?.dispose();
    };
  }, [name, host, attempt]);
  return (
    <div className="terminal-page">
      <header className="terminal-bar">
        <Link href="/sessions/" aria-label="Back to sessions">
          ← Sessions
        </Link>
        <strong>
          {host} / {name}
        </strong>
        <span className="grow" />
        <span role="status">{state}</span>
        {name && state !== "connected" && state !== "connecting" && (
          <button onClick={() => setAttempt((x) => x + 1)}>Reconnect</button>
        )}
      </header>
      <div ref={container} className="terminal" />
    </div>
  );
}

// Unit tests for the run journal → transcript parser. No browser needed; the
// shapes mirror real Codex app-server, Claude stream-json and opencode events
// recorded in delegated_events.
import { test, expect } from "@playwright/test";
import {
  RunEvent,
  TERMINAL_STATES,
  describeAction,
  errorText,
  lastError,
  stateTone,
  transcript,
} from "../lib/runEvents";

let seq = 0;
const ev = (kind: string, payload: unknown): RunEvent => ({ seq: ++seq, kind, payload });
const codex = (method: string, params: unknown = {}) => ev("native", { method, params });
const item = (value: Record<string, unknown>) => codex("item/completed", { item: value });
const types = (events: RunEvent[]) => transcript(events).map((e) => e.type);

test.describe("codex app-server events", () => {
  test("streaming deltas are dropped; the completed message is kept once", () => {
    const entries = transcript([
      codex("item/agentMessage/delta", { delta: "Check" }),
      codex("item/agentMessage/delta", { delta: "ing" }),
      item({ type: "agentMessage", text: "Checking the workspace.", phase: "commentary" }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "agent", text: "Checking the workspace.", phase: "commentary" }),
    ]);
  });

  test("the assignment prompt is a collapsed prompt entry", () => {
    const entries = transcript([
      item({ type: "userMessage", content: [{ text: "You are the real worker" }, { text: "{criteria}" }] }),
    ]);
    expect(entries).toEqual([expect.objectContaining({ type: "prompt", text: "You are the real worker\n{criteria}" })]);
  });

  test("empty messages and empty reasoning add nothing", () => {
    expect(
      types([
        item({ type: "agentMessage", text: "" }),
        item({ type: "userMessage", content: [] }),
        item({ type: "reasoning", summary: [], content: [] }),
      ]),
    ).toEqual([]);
  });

  test("reasoning with a summary is kept", () => {
    const [entry] = transcript([item({ type: "reasoning", summary: [{ text: "Plan: inspect first" }], content: [] })]);
    expect(entry).toMatchObject({ type: "reasoning", text: "Plan: inspect first" });
  });

  test("commands prefer the readable commandActions over the shell wrapper", () => {
    const [entry] = transcript([
      item({
        type: "commandExecution",
        command: "/usr/bin/bash -lc 'pwd && ls'",
        commandActions: [{ command: "pwd" }, { command: "ls" }],
        cwd: "/home/u/ws",
        exitCode: 2,
        aggregatedOutput: "ls: cannot access\n",
        status: "failed",
      }),
    ]);
    expect(entry).toEqual(
      expect.objectContaining({
        type: "command",
        command: "pwd\nls",
        cwd: "/home/u/ws",
        exitCode: 2,
        output: "ls: cannot access\n",
        status: "failed",
      }),
    );
  });

  test("commands without actions fall back to the raw command and omit empty output", () => {
    const [entry] = transcript([
      item({ type: "commandExecution", command: "hostname", commandActions: [], exitCode: 0, aggregatedOutput: "" }),
    ]);
    expect(entry).toMatchObject({ type: "command", command: "hostname", exitCode: 0 });
    expect(entry).not.toHaveProperty("output", "");
  });

  test("file changes list each path and kind", () => {
    const [entry] = transcript([
      item({
        type: "fileChange",
        changes: [
          { path: "/ws/app.py", kind: { type: "add" }, diff: "+print(1)" },
          { path: "/ws/README.md", kind: "update" },
        ],
      }),
    ]);
    expect(entry).toEqual(
      expect.objectContaining({
        type: "files",
        changes: [
          { path: "/ws/app.py", kind: "add", diff: "+print(1)" },
          { path: "/ws/README.md", kind: "update", diff: undefined },
        ],
      }),
    );
  });

  test("MCP tool calls show server.tool, arguments and a rejection", () => {
    const [entry] = transcript([
      item({
        type: "mcpToolCall",
        server: "hive",
        tool: "peer",
        arguments: { kind: "question", to: "peer-b" },
        status: "failed",
        error: { message: "user rejected MCP tool call" },
      }),
    ]);
    expect(entry).toMatchObject({
      type: "tool",
      name: "hive.peer",
      status: "failed",
      error: "user rejected MCP tool call",
    });
    expect(JSON.parse((entry as { input: string }).input)).toEqual({ kind: "question", to: "peer-b" });
  });

  test("a failed turn reports its error; a clean turn adds nothing", () => {
    const entries = transcript([
      codex("turn/completed", { turn: { error: null, items: [] } }),
      codex("turn/completed", { turn: { error: { message: "You've hit your usage limit." } } }),
    ]);
    expect(entries).toEqual([expect.objectContaining({ type: "error", text: "You've hit your usage limit." })]);
  });

  test("a native error notification is unwrapped", () => {
    const [entry] = transcript([codex("error", { error: { message: "stream disconnected" } })]);
    expect(entry).toMatchObject({ type: "error", text: "stream disconnected" });
  });

  test("native approval requests show the command, cwd and file edits", () => {
    const entries = transcript([
      codex("item/commandExecution/requestApproval", {
        command: "/usr/bin/bash -lc git status",
        commandActions: [{ command: "git status" }],
        cwd: "/ws",
        reason: "needs review",
      }),
      codex("item/fileChange/requestApproval", { itemId: "change-a" }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "approval", command: "git status", cwd: "/ws", reason: "needs review" }),
      expect.objectContaining({ type: "approval", command: "Apply file changes" }),
    ]);
  });

  test("bookkeeping notifications never reach the transcript", () => {
    expect(
      types([
        codex("thread/started"),
        codex("turn/started"),
        codex("item/started", { item: { type: "commandExecution" } }),
        codex("item/commandExecution/outputDelta", { delta: "x" }),
        codex("account/rateLimits/updated"),
        codex("thread/tokenUsage/updated"),
        codex("mcpServer/startupStatus/updated"),
        codex("remoteControl/status/changed"),
        codex("turn/diff/updated"),
        item({ type: "somethingNew" }),
      ]),
    ).toEqual([]);
  });
});

test.describe("Hive journal events", () => {
  test("repeated states collapse, changes are kept", () => {
    const entries = transcript([
      ev("state", { state: "launching" }),
      ev("state", { state: "working" }),
      ev("state", { state: "working" }),
      ev("state", { state: "awaiting-approval" }),
      ev("state", {}),
    ]);
    expect(entries.map((e) => (e as { state: string }).state)).toEqual(["launching", "working", "awaiting-approval"]);
  });

  test("Hive's approval replaces the native request it describes", () => {
    const entries = transcript([
      codex("item/commandExecution/requestApproval", { commandActions: [{ command: "rm -rf build" }] }),
      ev("approval", {
        id: "ap-1",
        fingerprint: "f",
        reason: "Command may delete data",
        action: { tool: "item/commandExecution/requestApproval", arguments: { commandActions: [{ command: "rm -rf build" }], cwd: "/ws" } },
      }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "approval", id: "ap-1", command: "rm -rf build", cwd: "/ws", reason: "Command may delete data" }),
    ]);
  });

  test("a request re-issued after approval is shown once; a new command is shown", () => {
    const action = { arguments: { commandActions: [{ command: "pwd" }] } };
    const entries = transcript([
      ev("approval", { id: "ap-1", action }),
      ev("approval-consumed", { decision: "continue" }),
      codex("item/commandExecution/requestApproval", { commandActions: [{ command: "pwd" }] }),
      codex("item/commandExecution/requestApproval", { commandActions: [{ command: "ls" }] }),
    ]);
    expect(entries.map((e) => e.type)).toEqual(["approval", "approval-resolved", "approval"]);
    expect(entries[2]).toMatchObject({ command: "ls" });
  });

  test("a consumed approval records the decision", () => {
    const [entry] = transcript([ev("approval-consumed", { decision: "stop", id: "ap-1" })]);
    expect(entry).toMatchObject({ type: "approval-resolved", decision: "stop" });
  });

  test("runner errors are unwrapped and shown once alongside the turn error", () => {
    const message = "You've hit your usage limit. Try again at 1:39 AM.";
    const entries = transcript([
      codex("turn/completed", { turn: { error: { message } } }),
      ev("error", { message: JSON.stringify({ error: { message }, status: "failed" }) }),
      ev("error", { message: "SSH connection lost" }),
      ev("error", { message: "SSH connection lost" }),
    ]);
    expect(entries.map((e) => (e as { text: string }).text)).toEqual([message, "SSH connection lost"]);
  });

  test("unknown kinds and malformed payloads are ignored, never thrown", () => {
    expect(
      types([
        ev("peer", { kind: "question", text: "hi" }),
        ev("acknowledgment", { message_id: "m" }),
        { seq: 99, kind: "native", payload: null },
        ev("native", { type: "assistant" }),
        ev("native", { method: "item/completed" }),
      ]),
    ).toEqual([]);
  });
});

test.describe("Claude stream-json events", () => {
  const assistant = (...content: unknown[]) => ev("native", { type: "assistant", message: { content } });

  test("text, Bash and other tools render; thinking does not", () => {
    const entries = transcript([
      assistant({ type: "thinking", thinking: "secret" }, { type: "text", text: "Creating the workspace." }),
      assistant({ type: "tool_use", name: "Bash", input: { command: "mkdir -p ws && ls" } }),
      assistant({ type: "tool_use", name: "Read", input: { file_path: "a.py" } }),
      assistant({ type: "text", text: "   " }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "agent", text: "Creating the workspace." }),
      expect.objectContaining({ type: "command", command: "mkdir -p ws && ls", status: "started" }),
      expect.objectContaining({ type: "tool", name: "Read" }),
    ]);
  });

  test("tool results attach to the command or tool that asked for them", () => {
    const user = (...content: unknown[]) => ev("native", { type: "user", message: { content } });
    const entries = transcript([
      assistant({ type: "tool_use", id: "t1", name: "Bash", input: { command: "ls" } }),
      assistant({ type: "tool_use", id: "t2", name: "Bash", input: { command: "false" } }),
      assistant({ type: "tool_use", id: "t3", name: "Grep", input: { pattern: "x" } }),
      assistant({ type: "tool_use", id: "t4", name: "Read", input: { file_path: "gone" } }),
      user({ type: "tool_result", tool_use_id: "t1", content: "a.py\nb.py" }),
      user({ type: "tool_result", tool_use_id: "t2", is_error: true, content: [{ type: "text", text: "exit 1" }] }),
      user({ type: "tool_result", tool_use_id: "t3", content: [{ type: "text", text: "a.py:1:x" }] }),
      user({ type: "tool_result", tool_use_id: "t4", is_error: true, content: "no such file" }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "command", command: "ls", output: "a.py\nb.py", status: "completed" }),
      expect.objectContaining({ type: "command", command: "false", output: "exit 1", status: "failed" }),
      expect.objectContaining({ type: "tool", name: "Grep", output: "a.py:1:x", status: "completed" }),
      expect.objectContaining({ type: "tool", name: "Read", error: "no such file", status: "failed" }),
    ]);
  });

  test("Write, Edit and MultiEdit show as file changes with a diff", () => {
    const entries = transcript([
      assistant({ type: "tool_use", name: "Write", input: { file_path: "/w/a.py", content: "x = 1\ny = 2" } }),
      assistant({ type: "tool_use", name: "Edit", input: { file_path: "/w/b.py", old_string: "old", new_string: "new" } }),
      assistant({
        type: "tool_use",
        name: "MultiEdit",
        input: { file_path: "/w/c.py", edits: [{ old_string: "a", new_string: "b" }, { old_string: "c", new_string: "d" }] },
      }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "files", changes: [{ path: "/w/a.py", kind: "write", diff: "+x = 1\n+y = 2" }] }),
      expect.objectContaining({ type: "files", changes: [{ path: "/w/b.py", kind: "edit", diff: "-old\n+new" }] }),
      expect.objectContaining({ type: "files", changes: [{ path: "/w/c.py", kind: "edit", diff: "-a\n+b\n\n-c\n+d" }] }),
    ]);
  });

  test("failed tool results become errors; successful ones stay quiet", () => {
    const user = (...content: unknown[]) => ev("native", { type: "user", message: { content } });
    const entries = transcript([
      user({ type: "tool_result", is_error: false, content: "ok" }),
      user({ type: "tool_result", is_error: true, content: [{ text: "permission denied" }] }),
      user({ type: "tool_result", is_error: true, content: "exit 1" }),
    ]);
    expect(entries.map((e) => (e as { text: string }).text)).toEqual(["permission denied", "exit 1"]);
  });

  test("a result repeating the final message is dropped; others are kept", () => {
    const entries = transcript([
      assistant({ type: "text", text: "All done.\n" }),
      ev("native", { type: "result", result: "All done.", is_error: false }),
      ev("native", { type: "result", result: "Different summary", is_error: false }),
      ev("native", { type: "result", subtype: "error_max_turns", is_error: true }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "agent", text: "All done.\n" }),
      expect.objectContaining({ type: "result", text: "Different summary", error: false }),
      expect.objectContaining({ type: "result", text: "error_max_turns", error: true }),
    ]);
  });

  test("system and rate-limit events are ignored", () => {
    expect(
      types([
        ev("native", { type: "system", subtype: "init" }),
        ev("native", { type: "rate_limit_event", rate_limit_info: {} }),
      ]),
    ).toEqual([]);
  });
});

test.describe("opencode events", () => {
  test("text, bash and other tools render from message parts", () => {
    const entries = transcript([
      ev("native", {
        info: { role: "assistant" },
        parts: [
          { type: "step-start" },
          { type: "text", text: "Running it now." },
          { type: "tool", tool: "bash", state: { status: "completed", input: { command: "printf OK" }, output: "OK" } },
          { type: "tool", tool: "edit", state: { status: "error", input: { path: "a" }, error: "no such file" } },
          { type: "text", text: "" },
        ],
      }),
    ]);
    expect(entries).toEqual([
      expect.objectContaining({ type: "agent", text: "Running it now." }),
      expect.objectContaining({ type: "command", command: "printf OK", output: "OK", status: "completed" }),
      expect.objectContaining({ type: "tool", name: "edit", status: "error", error: "no such file" }),
    ]);
  });
});

test.describe("helpers", () => {
  test("errorText unwraps serialized errors and keeps plain text", () => {
    expect(errorText(JSON.stringify({ error: { message: "limit" } }))).toBe("limit");
    expect(errorText(JSON.stringify({ message: "top-level" }))).toBe("top-level");
    expect(errorText("plain failure")).toBe("plain failure");
    expect(errorText("{not json")).toBe("{not json");
    expect(errorText(undefined)).toBe("");
    expect(errorText({ code: 7 })).toBe('{"code":7}');
  });

  test("describeAction reads metadata strings, event objects and Claude tools", () => {
    const codexAction = { arguments: { command: "bash -lc x", commandActions: [{ command: "x" }, { command: "y" }], cwd: "/ws" } };
    expect(describeAction(JSON.stringify(codexAction))).toEqual({ command: "x\ny", cwd: "/ws" });
    expect(describeAction(codexAction)).toEqual({ command: "x\ny", cwd: "/ws" });
    expect(describeAction({ arguments: { command: "hostname" } })).toEqual({ command: "hostname", cwd: undefined });
    expect(
      describeAction({ tool: "Bash", arguments: { command: "mkdir ws", description: "make" }, workspace: "/Users/u/ws" }),
    ).toEqual({ command: "mkdir ws", cwd: "/Users/u/ws" });
    expect(describeAction("not json at all")).toEqual({ command: "not json at all" });
    expect(describeAction({ tool: "WebFetch", arguments: { url: "https://example.com" } }).command).toBe(
      'WebFetch {"url":"https://example.com"}',
    );
  });

  test("describeAction names the files a file-change approval edits", () => {
    const action = JSON.stringify({ tool: "item/fileChange/requestApproval", arguments: { itemId: "c" }, workspace: "/ws" });
    expect(describeAction(action, { changes: [{ path: "/ws/app.py" }, { path: "/ws/b.py" }] })).toEqual({
      command: "Edit /ws/app.py, /ws/b.py",
      cwd: "/ws",
    });
    expect(describeAction(action)).toEqual({ command: "Apply file changes", cwd: "/ws" });
    expect(describeAction(action, { changes: [] }).command).toBe("Apply file changes");
  });

  test("lastError finds the most recent failure from any source", () => {
    expect(lastError([])).toBeUndefined();
    expect(lastError([item({ type: "agentMessage", text: "fine" })])).toBeUndefined();
    expect(
      lastError([
        ev("error", { message: "first" }),
        item({ type: "agentMessage", text: "retrying" }),
        ev("error", { message: "second" }),
      ]),
    ).toBe("second");
    expect(lastError([ev("native", { type: "result", result: "max turns", is_error: true })])).toBe("max turns");
    expect(lastError([ev("native", { type: "result", result: "done", is_error: false })])).toBeUndefined();
  });

  test("every run state maps to the group people act on", () => {
    const tones = Object.fromEntries(
      [
        "awaiting-approval",
        "needs-setup",
        "failed",
        "disconnected",
        "launching",
        "working",
        "reviewing",
        "waiting-for-peer",
        "queued",
        "completed",
        "superseded",
      ].map((s) => [s, stateTone(s)]),
    );
    expect(tones).toEqual({
      "awaiting-approval": "attention",
      "needs-setup": "attention",
      failed: "bad",
      disconnected: "stale",
      launching: "running",
      working: "running",
      reviewing: "running",
      "waiting-for-peer": "running",
      queued: "queued",
      completed: "done",
      superseded: "done",
    });
  });

  test("terminal states stop live polling", () => {
    expect([...TERMINAL_STATES].sort()).toEqual(["completed", "disconnected", "failed", "superseded"]);
    for (const live of ["working", "awaiting-approval", "queued", "launching"]) expect(TERMINAL_STATES).not.toContain(live);
  });
});

// Turns a delegated run's journal (`/api/runs/{id}/events`) into a readable
// transcript. Native events come in two protocols: Codex app-server JSON-RPC
// (`payload.method`) and Claude stream-json (`payload.type`). Streaming deltas
// are dropped — the completed item carries the same text.

export type RunEvent = {
  id?: string;
  seq: number;
  kind: string;
  payload: any;
};

export type Entry =
  | { type: "state"; seq: number; state: string }
  | { type: "prompt"; seq: number; text: string }
  | { type: "agent"; seq: number; text: string; phase?: string }
  | { type: "reasoning"; seq: number; text: string }
  | {
      type: "command";
      seq: number;
      command: string;
      cwd?: string;
      exitCode?: number | null;
      output?: string;
      status?: string;
      toolId?: string;
    }
  | {
      type: "files";
      seq: number;
      changes: { path: string; kind: string; diff?: string }[];
      toolId?: string;
    }
  | {
      type: "tool";
      seq: number;
      name: string;
      input?: string;
      output?: string;
      status?: string;
      error?: string;
      toolId?: string;
    }
  | { type: "approval"; seq: number; id?: string; command: string; cwd?: string; reason?: string }
  | { type: "approval-resolved"; seq: number; decision: string }
  | { type: "error"; seq: number; text: string }
  | { type: "stalled"; seq: number; text: string }
  | { type: "result"; seq: number; text: string; error: boolean };

const str = (v: unknown) => (typeof v === "string" ? v : v == null ? "" : JSON.stringify(v));

// `error` events carry the runner's message, which is often a serialized
// Codex turn. Pull the human sentence out of it.
export function errorText(message: unknown): string {
  const raw = str(message);
  try {
    const parsed = JSON.parse(raw);
    return parsed?.error?.message || parsed?.message || raw;
  } catch {
    return raw;
  }
}

// An approval's `action` is an object in events and a JSON string in run
// metadata; both describe the command the agent wants to run.
export function describeAction(
  action: unknown,
  details?: { changes?: { path?: string }[] } | null,
): { command: string; cwd?: string } {
  let value: any = action;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value);
    } catch {
      return { command: value };
    }
  }
  const args = value?.arguments || value?.params || value || {};
  // File edits carry no command; Hive attaches the paths they touch.
  if (value?.tool === "item/fileChange/requestApproval") {
    const paths = (details?.changes || []).map((c) => str(c.path)).filter(Boolean);
    return {
      command: paths.length ? `Edit ${paths.join(", ")}` : "Apply file changes",
      cwd: args.cwd || value?.workspace || undefined,
    };
  }
  const actions: any[] = Array.isArray(args.commandActions) ? args.commandActions : [];
  const command =
    actions.map((a) => str(a.command)).filter(Boolean).join("\n") ||
    str(args.command) ||
    (args.reason ? str(args.reason) : "") ||
    (value?.tool ? `${value.tool} ${JSON.stringify(args)}` : JSON.stringify(value));
  return { command, cwd: args.cwd || value?.workspace || undefined };
}

function codex(seq: number, p: any): Entry[] {
  const params = p.params || {};
  switch (p.method) {
    case "item/completed": {
      const item = params.item || {};
      switch (item.type) {
        case "userMessage": {
          const text = (item.content || [])
            .map((c: any) => str(c.text))
            .join("\n");
          return text ? [{ type: "prompt", seq, text }] : [];
        }
        case "agentMessage":
          return item.text ? [{ type: "agent", seq, text: item.text, phase: item.phase }] : [];
        case "reasoning": {
          const text = [...(item.summary || []), ...(item.content || [])]
            .map((c: any) => str(c?.text ?? c))
            .join("\n")
            .trim();
          return text ? [{ type: "reasoning", seq, text }] : [];
        }
        case "commandExecution": {
          const actions: any[] = item.commandActions || [];
          return [
            {
              type: "command",
              seq,
              command: actions.map((a) => str(a.command)).filter(Boolean).join("\n") || str(item.command),
              cwd: item.cwd,
              exitCode: item.exitCode,
              output: item.aggregatedOutput || undefined,
              status: item.status,
            },
          ];
        }
        case "fileChange":
          return [
            {
              type: "files",
              seq,
              changes: (item.changes || []).map((c: any) => ({
                path: str(c.path),
                kind: str(c.kind?.type || c.kind),
                diff: c.diff,
              })),
            },
          ];
        case "mcpToolCall":
          return [
            {
              type: "tool",
              seq,
              name: [item.server, item.tool].filter(Boolean).join("."),
              input: item.arguments ? JSON.stringify(item.arguments, null, 2) : undefined,
              status: item.status,
              error: item.error?.message,
            },
          ];
        default:
          return [];
      }
    }
    case "turn/completed": {
      const error = params.turn?.error;
      return error ? [{ type: "error", seq, text: str(error.message || error) }] : [];
    }
    case "item/commandExecution/requestApproval":
    case "item/fileChange/requestApproval": {
      const { command, cwd } = describeAction({ arguments: params });
      return [
        {
          type: "approval",
          seq,
          command: p.method.startsWith("item/fileChange") ? "Apply file changes" : command,
          cwd,
          reason: params.reason || undefined,
        },
      ];
    }
    case "error":
      return [{ type: "error", seq, text: errorText(params.error?.message || params.message || params) }];
    default:
      return [];
  }
}

// Claude's file tools carry the edit itself; show it as a diff, not JSON.
const lines = (text: unknown, mark: string) =>
  str(text).split("\n").map((line) => mark + line).join("\n");
function claudeFiles(seq: number, c: any): Entry | undefined {
  const input = c.input || {};
  const path = str(input.file_path || input.notebook_path);
  const edits: any[] =
    c.name === "Edit" ? [input] : c.name === "MultiEdit" && Array.isArray(input.edits) ? input.edits : [];
  if (c.name === "Write")
    return { type: "files", seq, toolId: c.id, changes: [{ path, kind: "write", diff: lines(input.content, "+") }] };
  if (edits.length && edits.some((e) => e.old_string || e.new_string))
    return {
      type: "files",
      seq,
      toolId: c.id,
      changes: [
        {
          path,
          kind: "edit",
          diff: edits.map((e) => `${lines(e.old_string, "-")}\n${lines(e.new_string, "+")}`).join("\n\n"),
        },
      ],
    };
  return undefined;
}

const resultText = (c: any) =>
  Array.isArray(c.content)
    ? c.content.map((x: any) => str(x.text)).filter(Boolean).join("\n")
    : str(c.content);

function claude(seq: number, p: any): Entry[] {
  const content: any[] = Array.isArray(p.message?.content) ? p.message.content : [];
  switch (p.type) {
    case "assistant":
      return content.flatMap((c): Entry[] => {
        if (c.type === "text" && c.text?.trim()) return [{ type: "agent", seq, text: c.text }];
        if (c.type === "tool_use") {
          if (c.name === "Bash")
            return [{ type: "command", seq, toolId: c.id, command: str(c.input?.command), status: "started" }];
          const files = claudeFiles(seq, c);
          if (files) return [files];
          return [{ type: "tool", seq, toolId: c.id, name: str(c.name), input: JSON.stringify(c.input, null, 2) }];
        }
        return [];
      });
    case "result":
      return [{ type: "result", seq, text: str(p.result || p.subtype), error: !!p.is_error }];
    default:
      return [];
  }
}

// A Claude tool result answers the tool_use with the same id. Attach its
// output there; a failure nothing claims is still shown as an error.
function claudeResults(out: Entry[], seq: number, p: any): Entry[] {
  const content: any[] = Array.isArray(p.message?.content) ? p.message.content : [];
  return content.flatMap((c): Entry[] => {
    if (c.type !== "tool_result") return [];
    const text = resultText(c);
    const use = c.tool_use_id && out.findLast((x) => "toolId" in x && x.toolId === c.tool_use_id);
    if (use && use.type === "command") {
      use.output = text || undefined;
      use.status = c.is_error ? "failed" : "completed";
      return [];
    }
    if (use && use.type === "tool") {
      if (c.is_error) use.error = text || "failed";
      else use.output = text || undefined;
      use.status = c.is_error ? "failed" : "completed";
      return [];
    }
    return c.is_error ? [{ type: "error", seq, text }] : [];
  });
}

// opencode reports whole messages as `{info, parts: [...]}`.
function opencode(seq: number, p: any): Entry[] {
  return (p.parts as any[]).flatMap((part): Entry[] => {
    if (part?.type === "text" && str(part.text).trim()) return [{ type: "agent", seq, text: part.text }];
    if (part?.type !== "tool") return [];
    const state = part.state || {};
    if (part.tool === "bash")
      return [
        {
          type: "command",
          seq,
          command: str(state.input?.command),
          output: state.output ? str(state.output) : undefined,
          status: state.status,
        },
      ];
    return [
      {
        type: "tool",
        seq,
        name: str(part.tool),
        input: state.input ? JSON.stringify(state.input, null, 2) : undefined,
        status: state.status,
        error: state.error ? str(state.error) : undefined,
      },
    ];
  });
}

// Cursor headless (`cursor-agent -p --output-format stream-json`) wraps the
// tool it is running in one `<name>ToolCall` key. `call_id` is the correlation
// key: the same string arrives in `args.toolCallId` on the started event and
// again on the completed one.
const cursorTool = (body: any): [string, any] | undefined => {
  if (!body || typeof body !== "object") return undefined;
  const key = Object.keys(body).find((k) => k.endsWith("ToolCall") && k !== "toolCallId");
  return key ? [key.slice(0, -"ToolCall".length), body[key]] : undefined;
};

// Tools that change the workspace render as diffs from the edit's own result,
// not as a JSON blob of arguments that have not been applied yet.
const CURSOR_EDIT_TOOLS = /^(edit|write|delete|create|apply|patch|move|rename|search_replace|multi_edit)$/;

function cursorShell(seq: number, id: string, call: any, started: boolean): Entry {
  const args = call?.args || {};
  return {
    type: "command",
    seq,
    toolId: id,
    command: str(args.command),
    cwd: args.workingDirectory || undefined,
    output: started ? undefined : str(call?.result?.success?.stdout) || undefined,
    exitCode: started ? undefined : (call?.result?.success?.exitCode ?? null),
    status: started ? "started" : (call?.result?.success?.exitCode ? "failed" : "completed"),
  };
}

function cursorToolCall(out: Entry[], seq: number, p: any): Entry[] {
  const id = str(p.call_id);
  const tool = cursorTool(p.tool_call);
  if (!tool) return [];
  const [name, call] = tool;
  const started = p.subtype !== "completed";
  const result = call?.result?.success || {};
  if (name === "shell" || (call?.args?.command != null && !CURSOR_EDIT_TOOLS.test(name)))
    return started ? [cursorShell(seq, id, call, true)] : attach(out, id, cursorShell(seq, id, call, false));
  if (CURSOR_EDIT_TOOLS.test(name)) {
    // The diff only exists once the edit has been applied.
    if (started) return [];
    const path = str(result.path || call?.args?.path);
    if (!path) return [];
    return [
      {
        type: "files",
        seq,
        toolId: id,
        changes: [{ path, kind: "edit", diff: result.diffString ? str(result.diffString) : undefined }],
      },
    ];
  }
  if (started)
    return [
      {
        type: "tool",
        seq,
        toolId: id,
        name,
        input: Object.keys(call?.args || {}).length ? JSON.stringify(call.args, null, 2) : undefined,
        status: "started",
      },
    ];
  return attach(out, id, { type: "tool", seq, toolId: id, name, output: str(result), status: "completed" });
}

// A completed call answers the started one with the same call_id; update that
// entry in place. An unmatched completion still shows its own output.
function attach(out: Entry[], id: string, entry: Entry): Entry[] {
  const use = id && out.findLast((x) => "toolId" in x && x.toolId === id);
  if (use && use.type === "command" && entry.type === "command") {
    if (entry.output) use.output = entry.output;
    if (entry.exitCode !== undefined) use.exitCode = entry.exitCode;
    use.status = entry.status;
    return [];
  }
  if (use && use.type === "tool" && entry.type === "tool") {
    if (entry.output) use.output = entry.output;
    use.status = entry.status;
    return [];
  }
  return [entry];
}

export function transcript(events: RunEvent[]): Entry[] {
  const out: Entry[] = [];
  const push = (entry: Entry) => {
    // A failed turn is reported by the agent and again by the runner, and a
    // Claude result repeats its final message; show each once.
    if (entry.type === "error" && out.some((x) => x.type === "error" && x.text === entry.text)) return;
    if (entry.type === "approval") {
      // Codex re-issues an approved request natively; one card per command.
      const last = out.findLast((x) => x.type === "approval");
      if (last?.type === "approval" && last.command === entry.command) return;
    }
    if (entry.type === "result" && !entry.error) {
      const last = out.findLast((x) => x.type === "agent");
      if (last?.type === "agent" && last.text.trim() === entry.text.trim()) return;
    }
    out.push(entry);
  };
  // Cursor streams its reasoning as `thinking` deltas until it sends the
  // matching `completed`; join consecutive deltas into one entry, flushed by
  // any other event. Its assistant text and result already render as they do
  // for Claude, so only thoughts and tool calls need handling here.
  let thought: Extract<Entry, { type: "reasoning" }> | null = null;
  const flushThought = () => {
    if (thought && thought.text.trim()) push(thought);
    thought = null;
  };
  for (const e of events) {
    const p = e.payload || {};
    if (e.kind === "native" && (p.type === "thinking" || p.type === "tool_call")) {
      if (p.type === "thinking" && p.subtype === "delta" && str(p.text)) {
        if (!thought) thought = { type: "reasoning", seq: e.seq, text: "" };
        thought.text += str(p.text);
        continue;
      }
      flushThought();
      (p.type === "thinking" ? [] : cursorToolCall(out, e.seq, p)).forEach(push);
      continue;
    }
    flushThought();
    switch (e.kind) {
      case "state":
        // Consecutive repeats ("working", "working") add nothing.
        if (p.state && !(out.at(-1)?.type === "state" && (out.at(-1) as any).state === p.state))
          out.push({ type: "state", seq: e.seq, state: p.state });
        break;
      case "error":
        push({ type: "error", seq: e.seq, text: errorText(p.message) });
        break;
      case "approval": {
        const { command, cwd } = describeAction(p.action);
        // The native requestApproval right before it describes the same
        // command; keep one card carrying Hive's review reason.
        if (out.at(-1)?.type === "approval") out.pop();
        out.push({ type: "approval", seq: e.seq, id: p.id, command, cwd, reason: p.reason });
        break;
      }
      case "approval-consumed":
        out.push({ type: "approval-resolved", seq: e.seq, decision: str(p.decision) });
        break;
      case "stalled":
        out.push({ type: "stalled", seq: e.seq, text: stalledLabel(p) });
        break;
      case "native":
        (typeof p.method === "string"
          ? codex(e.seq, p)
          : Array.isArray(p.parts)
            ? opencode(e.seq, p)
            : p.type === "user"
              ? claudeResults(out, e.seq, p)
              : claude(e.seq, p)
        ).forEach(push);
        break;
    }
  }
  return out;
}

// The most recent failure a person should see, from any source.
export function lastError(events: RunEvent[]): string | undefined {
  const entries = transcript(events);
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (entry.type === "error") return entry.text;
    if (entry.type === "result" && entry.error) return entry.text;
  }
  return undefined;
}

export const TERMINAL_STATES = ["completed", "failed", "no_agreement", "superseded", "disconnected"];

export function stateTone(
  state: string,
): "attention" | "running" | "queued" | "done" | "bad" | "stale" {
  if (["awaiting-approval", "needs-setup"].includes(state)) return "attention";
  if (["failed", "no_agreement"].includes(state)) return "bad";
  // The runner lost contact; nothing it was doing will finish on its own.
  if (state === "disconnected") return "stale";
  if (["launching", "working", "reviewing", "verifying", "waiting-for-peer"].includes(state)) return "running";
  // A quota pause resumes by itself after the reset, like a queued run starts.
  if (["queued", "paused-quota"].includes(state)) return "queued";
  return "done";
}

export type Quota = { agent?: string; resets_at?: number; message?: string };

// The runner's watchdog: a tool call in progress with no native event for
// `silent_minutes`. The run is still working; the tool may yet finish.
export type Stall = {
  tool?: string;
  command?: string;
  silent_since?: number;
  silent_minutes?: number;
  reason?: string;
};

export function stalledLabel(stall: Stall): string {
  return `Stalled: ${stall.command || stall.tool || "tool call"} silent for ${stall.silent_minutes ?? 0} min`;
}

// A stall only means something while the run is still working on that turn.
export const activeStall = (state: string, stall?: Stall | null): Stall | undefined =>
  state === "working" && stall && (stall.command || stall.tool) ? stall : undefined;

// "Paused: codex quota resets at 3:05 PM" in the viewer's own time zone; a
// reset beyond the next day also names the day.
export function pausedLabel(quota: Quota | undefined, agent: string, now = Date.now()): string {
  const who = quota?.agent || agent;
  if (!quota?.resets_at) return `Paused: ${who} quota`;
  const reset = new Date(quota.resets_at * 1000);
  const time = reset.getTime() - now < 24 * 3600 * 1000
    ? reset.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    : reset.toLocaleString([], { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
  return `Paused: ${who} quota resets at ${time}`;
}

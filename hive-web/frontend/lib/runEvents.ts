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
    }
  | { type: "files"; seq: number; changes: { path: string; kind: string; diff?: string }[] }
  | {
      type: "tool";
      seq: number;
      name: string;
      input?: string;
      status?: string;
      error?: string;
    }
  | { type: "approval"; seq: number; id?: string; command: string; cwd?: string; reason?: string }
  | { type: "approval-resolved"; seq: number; decision: string }
  | { type: "error"; seq: number; text: string }
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

function claude(seq: number, p: any): Entry[] {
  const content: any[] = Array.isArray(p.message?.content) ? p.message.content : [];
  switch (p.type) {
    case "assistant":
      return content.flatMap((c): Entry[] => {
        if (c.type === "text" && c.text?.trim()) return [{ type: "agent", seq, text: c.text }];
        if (c.type === "tool_use") {
          if (c.name === "Bash")
            return [{ type: "command", seq, command: str(c.input?.command), status: "started" }];
          return [{ type: "tool", seq, name: str(c.name), input: JSON.stringify(c.input, null, 2) }];
        }
        return [];
      });
    case "user":
      return content.flatMap((c): Entry[] => {
        if (c.type !== "tool_result" || !c.is_error) return [];
        const text = Array.isArray(c.content) ? c.content.map((x: any) => str(x.text)).join("\n") : str(c.content);
        return [{ type: "error", seq, text }];
      });
    case "result":
      return [{ type: "result", seq, text: str(p.result || p.subtype), error: !!p.is_error }];
    default:
      return [];
  }
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
  for (const e of events) {
    const p = e.payload || {};
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
      case "native":
        (typeof p.method === "string"
          ? codex(e.seq, p)
          : Array.isArray(p.parts)
            ? opencode(e.seq, p)
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

export const TERMINAL_STATES = ["completed", "failed", "superseded", "disconnected"];

export function stateTone(
  state: string,
): "attention" | "running" | "queued" | "done" | "bad" | "stale" {
  if (["awaiting-approval", "needs-setup"].includes(state)) return "attention";
  if (state === "failed") return "bad";
  // The runner lost contact; nothing it was doing will finish on its own.
  if (state === "disconnected") return "stale";
  if (["launching", "working", "reviewing", "waiting-for-peer"].includes(state)) return "running";
  if (state === "queued") return "queued";
  return "done";
}

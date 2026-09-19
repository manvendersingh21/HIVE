import { test, expect, Page } from "@playwright/test";
import { readFile } from "node:fs/promises";
import { resolve, extname } from "node:path";

const chat = { id: "chat-1", title: "Machine work", updated_at: "Today" };
const session = {
  name: "ui-test",
  host: "worker-a",
  windows: 1,
  attached: false,
  current_command: "bash",
  window_name: "shell",
};
const run = {
  id: "run-1",
  tmux_name: "agent-1",
  state: "working",
  runner_path: "/tmp/runner",
  assignment: {
    device: "worker-a",
    agent: "codex",
    objective: "Collaborate with agent on worker-b",
    workspace: "~/hive-workspaces/test",
  },
  metadata: {},
};
const incident = {
  id: "incident-1",
  worker: "worker-a",
  tmux_session: "ui-test",
  review_state: "pending_review",
  created_at: "Today",
  analysis: {
    severity: "critical",
    category: "destructive_command",
    reason: "Needs review",
  },
  flagged_output: "<img src=x onerror=alert(1)>",
};
async function defaults(page: Page) {
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const values: Record<string, unknown> = {
      "/api/capabilities": { chat: true, terminal: true, master_name: "local" },
      "/api/settings/master-agent": {
        provider: "local",
        options: [
          {
            id: "local",
            label: "Local (Qwen via Ollama)",
            requires_api_key: false,
            configured: true,
          },
          {
            id: "zai",
            label: "Z.AI (GLM)",
            requires_api_key: true,
            configured: false,
          },
          {
            id: "nvidia",
            label: "NVIDIA",
            requires_api_key: true,
            configured: false,
          },
        ],
        local_model: "qwen3.5:9b",
        local_available: true,
      },
      "/api/chats": [chat],
      "/api/chats/chat-1": { messages: [] },
      "/api/runs": [],
      "/api/sessions": [session],
      "/api/session-hosts": [
        { host: "local", name: "local" },
        { host: "worker-a", name: "worker-a" },
      ],
      "/api/incidents": [incident],
      "/api/machines": {
        entities: [
          {
            id: "machine:a",
            name: "worker-a",
            kind: "machine",
            attrs: {
              host: "ssh-a",
              reachable: true,
              os: "linux",
              arch: "x86_64",
            },
          },
          { id: "tool:codex", name: "codex", kind: "tool", attrs: {} },
        ],
        edges: [{ from: "machine:a", to: "tool:codex", relation: "has_tool" }],
      },
    };
    if (path === "/api/machines/prompt")
      return route.fulfill({ body: "worker-a can run codex" });
    if (path in values) return route.fulfill({ json: values[path] });
    await route.fulfill({
      status: 404,
      body: "Unexpected API request: " + path,
    });
  });
}
test.beforeEach(async ({ page }) => {
  await defaults(page);
});

test("navigation works and marks the current page", async ({ page }) => {
  await page.goto("/");
  for (const label of [
    "Sessions",
    "Machines",
    "Incidents",
    "Settings",
    "Agent",
  ]) {
    await page.getByRole("link", { name: label, exact: true }).click();
    await expect(
      page.getByRole("link", { name: label, exact: true }),
    ).toHaveAttribute("aria-current", "page");
  }
});

test("master agent provider settings loads and saves", async ({ page }) => {
  let saved: unknown;
  await page.route("**/api/settings/master-agent", async (route) => {
    if (route.request().method() === "POST") {
      saved = JSON.parse(route.request().postData() || "{}");
      return route.fulfill({
        json: {
          provider: "zai",
          options: [
            {
              id: "local",
              label: "Local (Qwen via Ollama)",
              requires_api_key: false,
              configured: true,
            },
            {
              id: "zai",
              label: "Z.AI (GLM)",
              requires_api_key: true,
              configured: true,
            },
            {
              id: "nvidia",
              label: "NVIDIA",
              requires_api_key: true,
              configured: false,
            },
          ],
          local_model: "qwen3.5:9b",
          local_available: true,
        },
      });
    }
    return route.fulfill({
      json: {
        provider: "local",
        options: [
          {
            id: "local",
            label: "Local (Qwen via Ollama)",
            requires_api_key: false,
            configured: true,
          },
          {
            id: "zai",
            label: "Z.AI (GLM)",
            requires_api_key: true,
            configured: false,
          },
          {
            id: "nvidia",
            label: "NVIDIA",
            requires_api_key: true,
            configured: false,
          },
        ],
        local_model: "qwen3.5:9b",
        local_available: true,
      },
    });
  });
  await page.goto("/settings/");
  const select = page.getByLabel("Master agent provider");
  await expect(select).toHaveValue("local");
  await select.selectOption("zai");
  await page.getByLabel("Z.AI API key").fill("test-zai-key");
  await page.getByRole("button", { name: "Save master agent" }).click();
  await expect(page.getByRole("status")).toHaveText("Master agent updated.");
  expect(saved).toEqual({ provider: "zai", api_key: "test-zai-key" });
  await expect(select).toHaveValue("zai");
  await expect(page.getByLabel("Z.AI API key")).toHaveValue("");
});
test("login reports bad credentials and recovers from network failure", async ({
  page,
}) => {
  await page.route("**/login", (route) =>
    route.request().method() === "POST"
      ? route.fulfill({ status: 401, body: "bad password" })
      : route.continue(),
  );
  await page.goto("/login/");
  await page.getByLabel("Password", { exact: true }).fill("wrong-password");
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText(
    "Incorrect password.",
  );
  await page.route("**/login", (route) =>
    route.request().method() === "POST" ? route.abort() : route.continue(),
  );
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("button", { name: "Sign in" })).toBeEnabled();
});
test("login succeeds and sign out navigates to login", async ({ page }) => {
  await page.route("**/login", (route) =>
    route.request().method() === "POST"
      ? route.fulfill({ status: 200, body: "ok" })
      : route.continue(),
  );
  await page.route("**/logout", (route) =>
    route.fulfill({ status: 200, body: "ok" }),
  );
  await page.goto("/login/");
  await page.getByLabel("Password", { exact: true }).fill("test-password");
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page).toHaveURL("http://127.0.0.1:18081/");
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page).toHaveURL(/\/login\/$/);
});
test("sign out failure stays visible", async ({ page }) => {
  await page.route("**/logout", (route) => route.fulfill({ status: 500 }));
  await page.goto("/");
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.locator('p[role="alert"]')).toContainText(
    "Sign out failed",
  );
});
test("creates a session on the selected machine and preserves working directory", async ({
  page,
}) => {
  let payload: unknown;
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() === "POST") {
      payload = route.request().postDataJSON();
      return route.fulfill({ status: 201, json: session });
    }
    return route.fulfill({ json: [session] });
  });
  await page.goto("/sessions/");
  await page.getByLabel("Session name").fill("ui-new");
  await page.getByLabel("Machine", { exact: true }).selectOption("worker-a");
  await page.getByLabel("Session type").selectOption("codex");
  await page.getByLabel("Working directory").fill("/tmp/test");
  await page.getByRole("button", { name: "Start", exact: true }).click();
  await expect(page.getByLabel("Session name")).toHaveValue("");
  expect(payload).toEqual({
    name: "ui-new",
    host: "worker-a",
    kind: "codex",
    working_dir: "/tmp/test",
  });
  await expect(
    page.getByRole("link", { name: "Open", exact: true }),
  ).toHaveAttribute("href", "/terminal/?name=ui-test&host=worker-a");
});
test("session validation, duplicate errors, partial machine failures and kill cancellation", async ({
  page,
}) => {
  let posts = 0;
  let deletes = 0;
  await page.route("**/api/sessions*", (route) => {
    if (route.request().method() === "POST") {
      posts++;
      return route.fulfill({ status: 409, body: "duplicate session" });
    }
    return route.fulfill({
      json: [session],
      headers: { "x-hive-session-errors": '["worker-b: unreachable"]' },
    });
  });
  await page.route("**/api/sessions/*", (route) => {
    deletes++;
    return route.fulfill({ status: 204 });
  });
  await page.goto("/sessions/");
  await expect(page.getByRole("status")).toContainText("worker-b: unreachable");
  await page.getByLabel("Session name").fill("invalid name");
  await page.getByRole("button", { name: "Start", exact: true }).click();
  expect(posts).toBe(0);
  await page.getByLabel("Session name").fill("duplicate");
  await page.getByRole("button", { name: "Start", exact: true }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("duplicate session");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "Kill" }).click();
  expect(deletes).toBe(0);
});
test("kill checks errors and accepts a successful empty response", async ({
  page,
}) => {
  let deleted = false;
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ json: deleted ? [] : [session] }),
  );
  await page.route("**/api/sessions/*", (route) =>
    route.fulfill({ status: 502, body: "SSH unavailable" }),
  );
  await page.goto("/sessions/");
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Kill" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("SSH unavailable");
  await page.route("**/api/sessions/*", (route) => {
    deleted = true;
    return route.fulfill({ status: 204 });
  });
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Kill" }).click();
  await expect(page.getByText("No tmux sessions.")).toBeVisible();
});
test("machine graph renders facts and tools; re-probe success and failure", async ({
  page,
}) => {
  await page.route("**/api/machines/refresh", (route) =>
    route.fulfill({ status: 502, body: "probe failed" }),
  );
  await page.goto("/machines/");
  await expect(page.getByText("Online", { exact: true })).toBeVisible();
  await expect(page.getByText("codex", { exact: true })).toBeVisible();
  await page.getByText("Agent prompt preview").click();
  await expect(page.getByText("worker-a can run codex")).toBeVisible();
  await page.getByRole("button", { name: "Re-probe" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("probe failed");
  await page.route("**/api/machines/refresh", (route) =>
    route.fulfill({ json: { machines: 1 } }),
  );
  await page.getByRole("button", { name: "Re-probe" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveCount(0);
});
for (const [button, decision, note] of [
  ["Resume", "resume", ""],
  ["Resume with note", "resume_with_note", "Stay in workspace"],
  ["Modify and resume", "modify_and_resume", "echo safe"],
  ["Abort", "abort", ""],
]) {
  test("incident action: " + button, async ({ page }) => {
    let payload: unknown;
    await page.route("**/api/incidents/*/decide", (route) => {
      payload = route.request().postDataJSON();
      return route.fulfill({ json: { applied: "resumed" } });
    });
    await page.goto("/incidents/");
    await expect(page.locator("article img")).toHaveCount(0);
    await expect(page.getByText(incident.flagged_output)).toBeVisible();
    if (note || decision === "abort")
      page.once("dialog", (dialog) => dialog.accept(note));
    await page.getByRole("button", { name: button, exact: true }).click();
    await expect(page.getByRole("status")).toHaveText("Decision applied.");
    expect(payload).toEqual(note ? { [decision]: note } : decision);
    await page.getByLabel("Show history").check();
    await expect(
      page.getByRole("link", { name: "Open worker-a / ui-test" }),
    ).toHaveAttribute("href", "/terminal/?name=ui-test&host=worker-a");
  });
}
test("canceling incident note sends no decision; server failures display", async ({
  page,
}) => {
  let requests = 0;
  await page.route("**/api/incidents/*/decide", (route) => {
    requests++;
    return route.fulfill({ status: 409, json: { error: "Already decided" } });
  });
  await page.goto("/incidents/");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "Resume with note" }).click();
  expect(requests).toBe(0);
  await page.getByRole("button", { name: "Resume", exact: true }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Already decided");
});
test("new chat sends JSON and saved conversation reopens and searches", async ({
  page,
}) => {
  await page.addInitScript(() =>
    Object.defineProperty(crypto, "randomUUID", { value: undefined }),
  );
  let sent: any;
  let messages: unknown[] = [];
  await page.route("**/api/chats", (route) =>
    route.fulfill({
      json: route.request().method() === "POST" ? chat : [chat],
    }),
  );
  await page.route("**/api/chats/chat-1", (route) =>
    route.fulfill({ json: { messages } }),
  );
  await page.route("**/api/chat", (route) => {
    expect(route.request().headers()["content-type"]).toContain(
      "application/json",
    );
    sent = route.request().postDataJSON();
    messages = [
      { role: "user", content: sent.message },
      { role: "assistant", content: "Command completed", status: "completed" },
    ];
    return route.fulfill({ json: { status: "planning" } });
  });
  await page.goto("/");
  await page.getByLabel("Message Hive").fill("Run echo UI_OK on worker-a");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.getByText("Command completed")).toBeVisible();
  expect(sent.conversation_id).toBe("chat-1");
  expect(sent.background).toBe(true);
  expect(sent.request_id).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  await page.getByRole("button", { name: "New chat" }).click();
  await expect(page.getByText("Command completed")).toHaveCount(0);
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("Command completed")).toBeVisible();
  const search = page.waitForRequest((r) =>
    r.url().includes("/api/chats?q=machine"),
  );
  await page.getByLabel("Search chats").fill("machine");
  await search;
});
test("chat creation failure preserves typed input and history", async ({
  page,
}) => {
  await page.route("**/api/chats", (route) =>
    route.request().method() === "POST"
      ? route.fulfill({ status: 503, body: "Storage unavailable" })
      : route.fulfill({ json: [chat] }),
  );
  await page.goto("/");
  await page.getByLabel("Message Hive").fill("Keep this request");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText(
    "Storage unavailable",
  );
  await expect(page.getByLabel("Message Hive")).toHaveValue(
    "Keep this request",
  );
});
for (const allowed of [true, false]) {
  test(
    "chat command " + (allowed ? "approval" : "denial"),
    async ({ page }) => {
      const message = {
        role: "assistant",
        content: "Review command",
        status: "awaiting_approval",
        reply: {
          run: {
            id: "plan-1",
            steps: [
              {
                id: 7,
                command: "echo review",
                target: { kind: "remote", worker: "worker-a" },
              },
            ],
          },
          result: {
            awaiting_approval: [7],
            sessions: [{ session_name: "ui-test", worker_name: "worker-a" }],
          },
        },
      };
      let payload: unknown;
      await page.route("**/api/chats/chat-1", (route) =>
        route.fulfill({ json: { messages: [message] } }),
      );
      await page.route("**/api/chat/plan-1/approve", (route) => {
        payload = route.request().postDataJSON();
        message.status = "completed";
        return route.fulfill({ json: {} });
      });
      await page.goto("/");
      await page.getByRole("button", { name: /Machine work/ }).click();
      await page.getByLabel("Message Hive").fill("another command");
      await expect(
        page.getByRole("button", { name: "Running…" }),
      ).toBeDisabled();
      await page
        .getByRole("button", {
          name: allowed ? "Approve command" : "Deny command",
        })
        .click();
      await expect(
        page.getByRole("button", { name: "Send", exact: true }),
      ).toBeEnabled();
      expect(payload).toEqual({
        approved: allowed ? [7] : [],
        denied: allowed ? [] : [7],
      });
      await page.getByText("Execution details").click();
      await expect(
        page.getByRole("link", { name: "Open worker-a / ui-test" }),
      ).toBeVisible();
    },
  );
}
test("fleet cards show both agents, events, messages, decisions and setup retry", async ({
  page,
}) => {
  let message: any;
  const decisions: any[] = [];
  const pending = {
    ...run,
    state: "needs-setup",
    runner_path: null,
    metadata: {
      approvals: [{ id: "ap-1", fingerprint: "digest", consumed: 0 }],
    },
  };
  await page.route("**/api/runs?*", (route) =>
    route.fulfill({
      json: [
        pending,
        {
          ...run,
          id: "run-2",
          tmux_name: "agent-2",
          assignment: {
            ...run.assignment,
            device: "worker-b",
            agent: "claude",
          },
        },
      ],
    }),
  );
  await page.route("**/api/runs/run-1/events", (route) =>
    route.fulfill({ json: [{ kind: "peer-message", text: "hello worker-b" }] }),
  );
  await page.route("**/api/runs/run-1/messages", (route) => {
    message = route.request().postDataJSON();
    return route.fulfill({ json: { saved: true } });
  });
  await page.route("**/api/runs/run-1/decisions", (route) => {
    decisions.push(route.request().postDataJSON());
    return route.fulfill({ json: { saved: true } });
  });
  await page.route("**/api/runs/run-1/retry-setup", (route) =>
    route.fulfill({ json: { saved: true } }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(
    page.getByText("claude on worker-b", { exact: true }),
  ).toBeVisible();
  const card = page.locator(".run-card").first();
  await card.getByRole("button", { name: "Refresh events" }).click();
  await expect(card.getByText(/hello worker-b/)).toBeVisible();
  await card.getByLabel("Message codex on worker-a").fill("Talk to worker-b");
  await card.getByRole("button", { name: "Send to agent" }).click();
  await expect(card.getByRole("status")).toHaveText("Request saved.");
  expect(message.text).toBe("Talk to worker-b");
  for (const name of ["Continue agent", "Stop agent", "Retry setup"]) {
    await card.getByRole("button", { name }).click();
    await expect(card.getByRole("button", { name })).toBeEnabled();
  }
  expect(decisions.map((d) => d.decision)).toEqual(["continue", "stop"]);
  await expect(
    card.getByRole("link", { name: "Open terminal" }),
  ).toHaveAttribute("href", "/terminal/?name=agent-1&host=worker-a");
});
test("terminal sends binary input and text resize; reconnect and back work", async ({
  page,
}) => {
  const frames: (string | Buffer)[] = [];
  let socket: any;
  let connections = 0;
  await page.routeWebSocket("**/ws/**", (ws) => {
    socket = ws;
    connections++;
    ws.onMessage((data) => frames.push(data));
    ws.send("UI terminal ready\r\n");
  });
  await page.goto("/terminal/?name=ui-test&host=worker-a");
  await expect(page.getByRole("status")).toHaveText("connected");
  await page.locator(".xterm-helper-textarea").pressSequentially("echo UI_OK");
  await page.locator(".xterm-helper-textarea").press("Enter");
  await expect
    .poll(() =>
      frames
        .filter(Buffer.isBuffer)
        .map((x) => x.toString())
        .join(""),
    )
    .toContain("echo UI_OK\r");
  const previous = frames.length;
  await page.setViewportSize({ width: 820, height: 580 });
  await expect
    .poll(() =>
      frames
        .slice(previous)
        .some((x) => typeof x === "string" && JSON.parse(x).type === "resize"),
    )
    .toBe(true);
  socket.close();
  await page.getByRole("button", { name: "Reconnect" }).click();
  await expect.poll(() => connections).toBe(2);
  await page.getByRole("link", { name: "Back to sessions" }).click();
  await expect(page.getByRole("heading", { name: "Sessions" })).toBeVisible();
});
test("terminal without target reports missing session", async ({ page }) => {
  await page.goto("/terminal/");
  await expect(page.getByRole("status")).toHaveText("Missing session name");
});
test("mobile preserves chat history and all navigation", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(page.getByRole("button", { name: "New chat" })).toBeVisible();
  await expect(page.getByLabel("Search chats")).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Machine work/ }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  for (const label of ["Sessions", "Machines", "Incidents"]) {
    await page.getByRole("link", { name: label, exact: true }).click();
    await expect(page.getByRole("heading", { name: label })).toBeVisible();
  }
});
test("terminal-only host disables chat and links sessions", async ({
  page,
}) => {
  await page.route("**/api/capabilities", (route) =>
    route.fulfill({ json: { chat: false, terminal: true } }),
  );
  await page.goto("/");
  await expect(page.getByRole("status")).toContainText(
    "This host serves terminals only",
  );
  await expect(page.getByLabel("Message Hive")).toBeDisabled();
});
test("unauthorized API request returns user to login", async ({ page }) => {
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ status: 401, body: "unauthorized" }),
  );
  await page.goto("/sessions/");
  await expect(page).toHaveURL(/\/login\/$/);
});

test("reopening an active chat resumes polling and recovers the composer", async ({
  page,
}) => {
  let completed = false;
  await page.route("**/api/chats/chat-1", (route) =>
    route.fulfill({
      json: {
        messages: [
          {
            role: "assistant",
            content: completed
              ? "Finished after reconnect"
              : "Planning request",
            status: completed ? "completed" : "planning",
          },
        ],
      },
    }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("Planning request")).toBeVisible();
  await page.getByLabel("Message Hive").fill("Next request");
  await expect(page.getByRole("button", { name: "Running…" })).toBeDisabled();
  completed = true;
  await expect(page.getByText("Finished after reconnect")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Send", exact: true }),
  ).toBeEnabled();
});
test("slow history responses cannot overwrite a new chat", async ({ page }) => {
  let release: () => void = () => {};
  const delayed = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/chats/chat-1", async (route) => {
    await delayed;
    await route.fulfill({
      json: { messages: [{ role: "assistant", content: "Old conversation" }] },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByRole("status")).toHaveText("Loading conversation…");
  await page.getByRole("button", { name: "New chat" }).click();
  release();
  await expect(page.getByText("Old conversation")).toHaveCount(0);
  await expect(page.getByLabel("Message Hive")).toBeEnabled();
});
test("history pagination loads the next page", async ({ page }) => {
  const chats = Array.from({ length: 50 }, (_, i) => ({
    id: String(i),
    title: "Saved chat " + i,
  }));
  await page.route("**/api/chats?*", (route) =>
    route.fulfill({
      json:
        new URL(route.request().url()).searchParams.get("offset") === "50"
          ? [{ id: "last", title: "Final chat" }]
          : chats,
    }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "Load more" }).click();
  await expect(page.getByRole("button", { name: "Final chat" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Load more" })).toHaveCount(0);
});
test("message failures retain existing conversation and draft", async ({
  page,
}) => {
  await page.route("**/api/chats/chat-1", (route) =>
    route.fulfill({
      json: {
        messages: [
          {
            role: "assistant",
            content: "Previous answer",
            status: "completed",
          },
        ],
      },
    }),
  );
  await page.route("**/api/chat", (route) =>
    route.fulfill({ status: 503, body: "Model unavailable" }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("Previous answer")).toBeVisible();
  await page.getByLabel("Message Hive").fill("Retry this command");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Model unavailable");
  await expect(page.getByText("Previous answer")).toBeVisible();
  await expect(page.getByLabel("Message Hive")).toHaveValue(
    "Retry this command",
  );
});
test("fleet message failure retains draft", async ({ page }) => {
  await page.route("**/api/runs?*", (route) => route.fulfill({ json: [run] }));
  await page.route("**/api/runs/run-1/messages", (route) =>
    route.fulfill({ status: 503, body: "Agent unreachable" }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await page.getByLabel("Message codex on worker-a").fill("Keep this");
  await page.getByRole("button", { name: "Send to agent" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Agent unreachable");
  await expect(page.getByLabel("Message codex on worker-a")).toHaveValue(
    "Keep this",
  );
});
test("forbidden action displays error without signing user out", async ({
  page,
}) => {
  await page.route("**/api/machines/refresh", (route) =>
    route.fulfill({ status: 403, body: "Forbidden action" }),
  );
  await page.goto("/machines/");
  await page.getByRole("button", { name: "Re-probe" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Forbidden action");
  await expect(page).toHaveURL(/\/machines\/$/);
});
test("live local tmux: browser login, create, command, resize, kill, logout", async ({
  page,
}) => {
  test.skip(
    !process.env.HIVE_UI_LIVE_URL,
    "Set HIVE_UI_LIVE_URL and HIVE_UI_TEST_PASSWORD for a real backend.",
  );
  test.setTimeout(60000);
  const url = await prepareLive(page);
  const name = "hive-ui-" + Date.now();
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(url + "/login/");
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.HIVE_UI_TEST_PASSWORD!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("link", { name: "Sessions", exact: true }).click();
  await page.getByLabel("Session name").fill(name);
  await page.getByLabel("Session type").selectOption("shell");
  await page.getByRole("button", { name: "Start", exact: true }).click();
  const card = page.locator("article").filter({ hasText: name });
  await card.getByRole("link", { name: "Open", exact: true }).click();
  await expect(page.getByRole("status")).toHaveText("connected");
  let output = "";
  page.on("websocket", (ws) =>
    ws.on("framereceived", (event) => {
      output += event.payload.toString();
    }),
  );
  // Existing socket was opened before listener registration; reconnect to capture its output.
  await page.reload();
  await expect(page.getByRole("status")).toHaveText("connected");
  await page
    .locator(".xterm-helper-textarea")
    .pressSequentially("printf 'HIVE_%s\\n' 'UI_OK'");
  await page.locator(".xterm-helper-textarea").press("Enter");
  await expect.poll(() => output).toContain("HIVE_UI_OK");
  await page.setViewportSize({ width: 900, height: 620 });
  await page.screenshot({ path: "/tmp/hive-ui-live-terminal.png" });
  await page.getByRole("link", { name: "Back to sessions" }).click();
  page.once("dialog", (dialog) => dialog.accept());
  await page
    .locator("article")
    .filter({ hasText: name })
    .getByRole("button", { name: "Kill" })
    .click();
  await expect(page.locator("article").filter({ hasText: name })).toHaveCount(
    0,
  );
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page).toHaveURL(/\/login\/$/);
  expect(errors).toEqual([]);
});

async function prepareLive(page: Page) {
  await page.unrouteAll();
  const url = process.env.HIVE_UI_LIVE_URL!;
  if (process.env.HIVE_UI_STATIC_OVERRIDE === "1") {
    await page
      .context()
      .grantPermissions(["local-network-access"], { origin: url });
    // Exercise the new UI against real APIs while the backend page-serving fix is pending.
    // This mode does not validate backend static routing or deployment.
    await page.route("**/*", async (route) => {
      const request = route.request();
      const path = new URL(request.url()).pathname;
      if (
        request.method() !== "GET" ||
        path.startsWith("/api/") ||
        path.startsWith("/ws/")
      )
        return route.continue();
      const file = resolve(
        "out",
        "." + (path.endsWith("/") ? path + "index.html" : path),
      );
      if (!file.startsWith(resolve("out") + "/")) return route.continue();
      try {
        const types: Record<string, string> = {
          ".html": "text/html",
          ".js": "text/javascript",
          ".css": "text/css",
          ".txt": "text/plain",
        };
        await route.fulfill({
          body: await readFile(file),
          contentType: types[extname(file)] || "application/octet-stream",
        });
      } catch {
        await route.continue();
      }
    });
  }

  return url;
}
test("live chat records a command request and shows its real outcome", async ({
  page,
}) => {
  test.skip(!process.env.HIVE_UI_LIVE_URL, "Requires a real backend.");
  test.setTimeout(200000);
  const url = await prepareLive(page);
  await page.goto(url + "/login/");
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.HIVE_UI_TEST_PASSWORD!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("button", { name: "New chat" }).click();
  await page
    .getByLabel("Message Hive")
    .fill(
      "UI verification: on the local machine run only printf HIVE_CHAT_UI_OK. Report its output.",
    );
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(".message.user")).toContainText("HIVE_CHAT_UI_OK");
  await expect(page.locator(".message.assistant small")).toHaveText(
    /completed|failed|interrupted|awaiting_approval/,
    { timeout: 180000 },
  );
  const outcome = await page.locator(".message.assistant").innerText();
  console.log("Live chat outcome:", outcome.slice(0, 2000));
  await page.screenshot({ path: "/tmp/hive-ui-live-chat.png" });
  await page.getByRole("link", { name: "Machines", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Machines" })).toBeVisible();
  await page.getByRole("button", { name: "Re-probe" }).click();
  await expect(page.getByRole("button", { name: "Re-probe" })).toBeEnabled({
    timeout: 30000,
  });
  await page.getByRole("link", { name: "Incidents", exact: true }).click();
  await page.getByLabel("Show history").check();
  await expect(page.locator('p[role="alert"]')).toHaveCount(0);
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page).toHaveURL(/\/login\/$/);
});

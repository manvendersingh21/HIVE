import { test, expect, Locator, Page } from "@playwright/test";
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
  task_id: "turn-1",
  conversation_id: "chat-1",
  tmux_name: "agent-1",
  state: "working",
  runner_path: "/tmp/runner",
  assignment: {
    key: "codex-a",
    device: "worker-a",
    agent: "codex",
    objective: "Collaborate with agent on worker-b",
    workspace: "~/hive-workspaces/test",
    dependencies: [],
    acceptance_criteria: ["Verified output"],
  },
  metadata: {},
};
// Codex app-server journal: a streamed delta, the completed items, a command.
const codexEvents = [
  { seq: 1, kind: "state", payload: { state: "working" } },
  { seq: 2, kind: "native", payload: { method: "item/agentMessage/delta", params: { delta: "Checking" } } },
  { seq: 3, kind: "native", payload: { method: "item/completed", params: { item: { type: "agentMessage", text: "Checking the workspace first." } } } },
  { seq: 4, kind: "native", payload: { method: "item/completed", params: { item: { type: "commandExecution", command: "/usr/bin/bash -lc pwd", commandActions: [{ command: "pwd" }], cwd: "/home/u/ws", exitCode: 0, aggregatedOutput: "/home/u/ws\n" } } } },
];
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
      "/api/fleet": [
        {
          name: "worker-a",
          host: "ssh-a",
          user: "someone",
          port: null,
          tags: ["linux"],
          status: "online",
          removable: false,
        },
      ],
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
test("fleet settings add and remove only editable workers", async ({ page }) => {
  const configured = { name: "worker-a", host: "ssh-a", user: "someone", port: null,
    tags: ["linux"], status: "online", removable: false };
  const added = { ...configured, name: "qa-worker", host: "ssh-qa", user: "tester",
    tags: ["qa", "light"], removable: true };
  let payload: unknown;
  await page.route("**/api/fleet", async (route) => {
    if (route.request().method() === "POST") {
      payload = route.request().postDataJSON();
      return route.fulfill({ json: [configured, added] });
    }
    await route.fulfill({ json: [configured] });
  });
  await page.route("**/api/fleet/qa-worker", async (route) => {
    expect(route.request().method()).toBe("DELETE");
    await route.fulfill({ json: [configured] });
  });
  await page.goto("/settings/");
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);
  await page.getByLabel("Name", { exact: true }).fill(" qa-worker ");
  await page.getByLabel("Host", { exact: true }).fill(" ssh-qa ");
  await page.getByLabel("SSH user", { exact: true }).fill(" tester ");
  await page.getByLabel("Tags (comma-separated, optional)").fill("qa, light, ");
  await page.getByRole("button", { name: "Add machine" }).click();
  await expect(page.locator("article").filter({ hasText: "qa-worker" })).toBeVisible();
  expect(payload).toEqual({ name: "qa-worker", host: "ssh-qa", user: "tester", tags: ["qa", "light"] });
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue("");
  await page.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(page.locator("article").filter({ hasText: "qa-worker" })).toHaveCount(0);
  await expect(page.locator("article").filter({ hasText: "worker-a" })).toBeVisible();
});

test("fleet settings preserve failed additions and report removal errors", async ({ page }) => {
  await page.route("**/api/fleet", async (route) => route.request().method() === "POST"
    ? route.fulfill({ status: 409, body: "Worker already exists" })
    : route.fulfill({ json: [{ name: "qa-worker", host: "ssh-qa", user: "tester", port: null,
      tags: [], status: "offline", removable: true }] }));
  await page.route("**/api/fleet/qa-worker", (route) =>
    route.fulfill({ status: 503, body: "Could not save fleet" }));
  await page.goto("/settings/");
  await page.getByLabel("Name", { exact: true }).fill("qa-worker");
  await page.getByLabel("Host", { exact: true }).fill("ssh-qa");
  await page.getByLabel("SSH user", { exact: true }).fill("tester");
  await page.getByRole("button", { name: "Add machine" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Worker already exists");
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue("qa-worker");
  await expect(page.getByRole("button", { name: "Add machine" })).toBeEnabled();
  await page.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Could not save fleet");
  await expect(page.locator("article").filter({ hasText: "qa-worker" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toBeEnabled();
});

test("provider save failure preserves key and allows retry", async ({ page }) => {
  await page.route("**/api/settings/master-agent", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    await route.fulfill({ status: 503, body: "Provider unavailable" });
  });
  await page.goto("/settings/");
  await page.getByLabel("Master agent provider").selectOption("zai");
  await page.getByLabel("Z.AI API key").fill("qa-placeholder-key");
  await page.getByRole("button", { name: "Save master agent" }).click();
  await expect(page.locator('p[role="alert"]')).toHaveText("Provider unavailable");
  await expect(page.getByLabel("Z.AI API key")).toHaveValue("qa-placeholder-key");
  await expect(page.getByRole("button", { name: "Save master agent" })).toBeEnabled();
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
  await expect(page).toHaveURL(/\/login\/?$/);
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
  await expect(page.getByText("No terminal sessions. Start one above.")).toBeVisible();
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
test("chat run cards show both agents, approvals, setup retry and link to the session", async ({
  page,
}) => {
  const decisions: any[] = [];
  const pending = {
    ...run,
    state: "needs-setup",
    runner_path: null,
    metadata: {
      approvals: [
        {
          id: "ap-1",
          fingerprint: "digest",
          consumed: 0,
          reason: "Shell expansion requires review",
          action: JSON.stringify({ arguments: { command: "bash -lc 'ls'", commandActions: [{ command: "ls -la" }], cwd: "/home/u/ws" } }),
        },
      ],
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
          assignment: { ...run.assignment, key: "claude-b", device: "worker-b", agent: "claude" },
        },
      ],
    }),
  );
  await page.route("**/api/runs/run-1/decisions", (route) => {
    decisions.push(route.request().postDataJSON());
    return route.fulfill({ json: { saved: true } });
  });
  await page.route("**/api/runs/run-1/retry-setup", (route) =>
    route.fulfill({ json: { saved: true } }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("claude on worker-b", { exact: true })).toBeVisible();
  const card = page.locator(".run-card").first();
  await expect(card.locator(".approval pre")).toHaveText("ls -la");
  await expect(card.getByText("Shell expansion requires review")).toBeVisible();
  for (const name of ["Approve", "Deny and stop", "Retry setup"]) {
    await card.getByRole("button", { name }).click();
    await expect(card.getByRole("button", { name })).toBeEnabled();
  }
  expect(decisions.map((d) => d.decision)).toEqual(["continue", "stop"]);
  expect(decisions[0]).toMatchObject({ id: "ap-1", fingerprint: "digest" });
  await expect(card.getByRole("link", { name: "Open session" })).toHaveAttribute(
    "href",
    "/session/?run=run-1",
  );
});
test("session page renders a readable transcript and streams new events", async ({ page }) => {
  const requests: string[] = [];
  await page.route("**/api/runs", (route) => route.fulfill({ json: [run] }));
  await page.route("**/api/runs/run-1/events*", (route) => {
    const query = new URL(route.request().url()).searchParams;
    requests.push(query.toString());
    if (query.get("tail")) return route.fulfill({ json: codexEvents });
    return route.fulfill({
      json: query.get("after") === "4"
        ? [{ seq: 5, kind: "native", payload: { method: "item/completed", params: { item: { type: "agentMessage", text: "All criteria verified." } } } }]
        : [],
    });
  });
  await page.goto("/session/?run=run-1");
  await expect(page.getByText("codex on worker-a", { exact: true })).toBeVisible();
  await expect(page.getByText("Checking the workspace first.")).toBeVisible();
  await expect(page.locator(".t-cmd summary").first()).toContainText("$ pwd");
  await expect(page.locator(".t-cmd summary").first()).toContainText("exit 0");
  // Deltas are folded into the completed message, never shown on their own.
  await expect(page.locator(".t-agent", { hasText: /^Checking$/ })).toHaveCount(0);
  await expect(page.getByText("All criteria verified.")).toBeVisible();
  expect(requests[0]).toBe("tail=500");
  expect(requests).toContain("after=4");
  await page.getByLabel("Raw events").check();
  await expect(page.locator(".transcript pre")).toContainText("item/agentMessage/delta");
  await expect(page.getByRole("link", { name: "Open chat" })).toHaveAttribute("href", "/?chat=chat-1");
  await expect(page.getByRole("link", { name: "Raw terminal" })).toHaveAttribute(
    "href",
    "/terminal/?name=agent-1&host=worker-a",
  );
});
test("session opened at its tail can page back to earlier activity", async ({ page }) => {
  const message = (seq: number, text: string) => ({
    seq,
    kind: "native",
    payload: { method: "item/completed", params: { item: { type: "agentMessage", text } } },
  });
  await page.route("**/api/runs", (route) => route.fulfill({ json: [{ ...run, state: "completed" }] }));
  await page.route("**/api/runs/run-1/events*", (route) => {
    const query = new URL(route.request().url()).searchParams;
    if (query.get("tail")) return route.fulfill({ json: [message(700, "Newest update")] });
    // after=399 returns 400..699; the view keeps only what precedes its first event.
    return route.fulfill({
      json: query.get("after") === "399" ? [message(400, "Earlier update"), message(700, "Newest update")] : [],
    });
  });
  await page.goto("/session/?run=run-1");
  await expect(page.getByText("Newest update")).toBeVisible();
  await page.getByRole("button", { name: "Show earlier activity" }).click();
  await expect(page.getByText("Earlier update")).toBeVisible();
  await expect(page.getByText("Newest update")).toHaveCount(1);
});
test("failed session explains why and queued session names what it waits for", async ({ page }) => {
  const failed = { ...run, state: "failed" };
  const queued = {
    ...run,
    id: "run-2",
    state: "queued",
    assignment: { ...run.assignment, key: "consumer", device: "worker-b", dependencies: ["codex-a"] },
  };
  await page.route("**/api/runs", (route) => route.fulfill({ json: [failed, queued] }));
  await page.route("**/api/runs/*/events*", (route) =>
    route.fulfill({
      json: route.request().url().includes("run-1")
        ? [
            { seq: 1, kind: "native", payload: { method: "turn/completed", params: { turn: { error: { message: "You've hit your usage limit." } } } } },
            { seq: 2, kind: "error", payload: { message: JSON.stringify({ error: { message: "You've hit your usage limit." } }) } },
          ]
        : [],
    }),
  );
  await page.goto("/session/?run=run-1");
  await expect(page.locator(".banner.bad")).toContainText("You've hit your usage limit.");
  // The turn error and the runner's error event describe one failure.
  await expect(page.locator(".t-error")).toHaveCount(1);
  await page.goto("/session/?run=run-2");
  const waiting = page.locator(".banner", { hasText: "Waiting to start" });
  await expect(waiting).toContainText("codex on worker-a");
  await expect(waiting.locator("[data-state=failed]")).toBeVisible();
});
test("sessions page separates agent sessions by attention from terminals", async ({ page }) => {
  await page.route("**/api/sessions", (route) =>
    route.fulfill({
      json: [
        session,
        { ...session, name: "agent-1", windows: 1, run: { ...run, state: "awaiting-approval" } },
        { ...session, name: "agent-2", windows: 0, run: { ...run, id: "run-2", state: "completed" } },
      ],
    }),
  );
  await page.goto("/sessions/");
  const attention = page.locator(".session-group", { hasText: "Needs attention" });
  await expect(attention.locator(".session-card")).toHaveCount(1);
  await expect(attention.locator(".session-card")).toHaveAttribute("href", "/session/?run=run-1");
  await expect(page.locator(".session-group", { hasText: "Finished" }).locator(".session-card")).toHaveCount(1);
  await expect(page.getByRole("link", { name: "Open", exact: true })).toHaveAttribute(
    "href",
    "/terminal/?name=ui-test&host=worker-a",
  );
});
test("chat shows each turn's runs under the reply that dispatched them", async ({ page }) => {
  const reply = (task: string, text: string) => ({
    role: "assistant",
    content: text,
    status: "completed",
    reply: { delegation: { task_id: task, runs: [] } },
  });
  await page.route("**/api/chats/chat-1", (route) =>
    route.fulfill({
      json: {
        messages: [
          { role: "user", content: "first" },
          reply("turn-1", "First plan"),
          { role: "user", content: "again" },
          reply("turn-2", "Second plan"),
        ],
      },
    }),
  );
  await page.route("**/api/runs?*", (route) =>
    route.fulfill({
      json: [
        run,
        { ...run, id: "run-2", task_id: "turn-2", assignment: { ...run.assignment, device: "worker-b" } },
      ],
    }),
  );
  await page.goto("/?chat=chat-1");
  const second = page.locator(".message.assistant", { hasText: "Second plan" });
  await expect(second.locator(".run-card")).toHaveCount(1);
  await expect(second.locator(".run-card")).toContainText("codex on worker-b");
  await expect(
    page.locator(".message.assistant", { hasText: "First plan" }).locator(".run-card"),
  ).toContainText("codex on worker-a");
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
  await expect(page.getByRole("heading", { name: "Sessions", exact: true })).toBeVisible();
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
    await expect(page.getByRole("heading", { name: label, exact: true })).toBeVisible();
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
  await expect(page).toHaveURL(/\/login\/?$/);
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
test("chat preserves a new draft typed while the previous request is sending", async ({ page }) => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  let received = false;
  await page.route("**/api/chat", async (route) => {
    received = true;
    await gate;
    await route.fulfill({ json: { status: "planning" } });
  });
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  const input = page.getByLabel("Message Hive");
  await input.fill("First request");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect.poll(() => received).toBe(true);
  await input.fill("Keep my next request");
  release();
  await expect(page.getByRole("button", { name: "Sending…", exact: true })).toHaveCount(0);
  await expect(input).toHaveValue("Keep my next request");
});

test("agent message preserves a new draft typed while sending", async ({ page }) => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  let received: any;
  await page.route("**/api/runs", (route) => route.fulfill({ json: [run] }));
  await page.route("**/api/runs/run-1/messages", async (route) => {
    received = route.request().postDataJSON();
    await gate;
    await route.fulfill({ json: { saved: true } });
  });
  await page.goto("/session/?run=run-1");
  const input = page.getByLabel("Message codex on worker-a");
  await input.fill("First message");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect.poll(() => received?.text).toBe("First message");
  await input.fill("Keep my next message");
  release();
  await expect(page.getByText("Sent. The agent receives it on its next turn.")).toBeVisible();
  await expect(input).toHaveValue("Keep my next message");
});

test("fleet message failure retains draft", async ({ page }) => {
  await page.route("**/api/runs", (route) => route.fulfill({ json: [run] }));
  await page.route("**/api/runs/run-1/messages", (route) =>
    route.fulfill({ status: 503, body: "Agent unreachable" }),
  );
  await page.goto("/session/?run=run-1");
  await page.getByLabel("Message codex on worker-a").fill("Keep this");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(".run-composer [role=alert]")).toHaveText("Agent unreachable");
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
  await loginLive(page, url);
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
  await expect(page).toHaveURL(/\/login\/?$/);
  await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();
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

async function loginLive(page: Page, url: string) {
  await page.goto(url + "/login/");
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.HIVE_UI_TEST_PASSWORD!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page).not.toHaveURL(/\/login\/?$/, { timeout: 10000 });
  await expect(page.getByRole("button", { name: "Sign out" })).toBeVisible();
}

type LiveAssignment = { device: string; agent: string; model: string };
type LiveRunEvidence = {
  id: string;
  assignment: LiveAssignment;
  metadata: { actual_model?: string };
};

function lowerTierModel(variable: string, fallback: string) {
  const selected = process.env[variable]?.trim() || fallback;
  // These remote smoke tests must never silently launch a default/high model.
  expect([
    "gpt-5.6-luna", "haiku", "zai-coding-plan/glm-5.3-flash",
    "opencode/mimo-v2.6-flash-free",
  ], `${variable} must explicitly select an approved lower-tier QA model`).toContain(selected);
  return selected;
}

async function expectLivePlacement(page: Page, expected: LiveAssignment[]) {
  const details = page.locator(".message.assistant details pre").last();
  await expect(details).toBeAttached();
  const reply = JSON.parse((await details.textContent())!);
  const runs: LiveRunEvidence[] = reply.delegation.runs;
  const selected = (assignments: LiveAssignment[]) => assignments
    .map(({ device, agent, model }) => ({ device, agent, model }))
    .sort((a, b) => `${a.device}/${a.agent}`.localeCompare(`${b.device}/${b.agent}`));
  expect(selected(runs.map((run) => run.assignment))).toEqual(selected(expected));
  return { conversationId: reply.conversation_id as string, runs };
}

async function expectLivePlan(page: Page) {
  const status = page.locator(".message.assistant small").last();
  await expect(status).toHaveText(/^(completed|failed|interrupted)$/, { timeout: 300000 });
  expect(await status.textContent(), await page.locator(".message.assistant").last().innerText())
    .toBe("completed");
}

async function expectRuntimeModels(
  page: Page,
  placement: Awaited<ReturnType<typeof expectLivePlacement>>,
) {
  const url = new URL("/api/runs", page.url());
  url.searchParams.set("conversation_id", placement.conversationId);
  const response = await page.request.get(url.toString());
  expect(response.ok()).toBe(true);
  const runs: LiveRunEvidence[] = await response.json();
  expect(runs).toHaveLength(placement.runs.length);
  for (const planned of placement.runs) {
    const actual = runs.find((run) => run.id === planned.id);
    expect(actual?.assignment).toMatchObject(planned.assignment);
    if (planned.assignment.model === "haiku") {
      expect(actual?.metadata.actual_model).toMatch(/^claude-haiku-[\w.-]+$/);
    } else {
      expect(actual?.metadata.actual_model).toBe(planned.assignment.model);
    }
  }
}

async function completeLiveRun(page: Page, card: Locator) {
  const deadline = Date.now() + 600000;
  let retriedSetup = false;
  while (Date.now() < deadline) {
    const state = await card.locator(".bar [data-state]").first().getAttribute("data-state");
    if (state === "completed") return;
    if (["disconnected", "needs-setup"].includes(state || "") && !retriedSetup) {
      const retry = card.getByRole("button", { name: "Retry setup" });
      if (await retry.isEnabled()) {
        retriedSetup = true;
        await retry.click();
        await page.waitForTimeout(2000);
        continue;
      }
    }
    if (["failed", "disconnected", "needs-setup", "superseded"].includes(state || ""))
      throw new Error(`Remote agent entered terminal state: ${state}`);
    if (state === "awaiting-approval") {
      const approve = card.getByRole("button", { name: "Approve" });
      if (await approve.count() && await approve.isEnabled()) await approve.click();
    }
    await page.waitForTimeout(2000);
  }
  throw new Error("Remote agent did not complete within 10 minutes.");
}

// The journal itself, paged through the API the session view reads.
async function readLiveEvents(card: Locator): Promise<unknown[]> {
  const href = await card.getByRole("link", { name: "Open session" }).getAttribute("href");
  const id = new URL(href!, card.page().url()).searchParams.get("run")!;
  const events: { seq: number }[] = [];
  for (let pages = 0; ; pages++) {
    expect(pages, "Bounded live event pagination").toBeLessThan(20);
    const after = events.at(-1)?.seq ?? 0;
    const response = await card.page().request.get(
      new URL(`/api/runs/${encodeURIComponent(id)}/events?after=${after}`, card.page().url()).toString(),
    );
    expect(response.ok()).toBe(true);
    const page: { seq: number }[] = await response.json();
    events.push(...page);
    if (page.length < 300) return events;
  }
}

function expectPeerHandshake(events: unknown[]) {
  // Require actual outbound peer traffic and acknowledgment of incoming traffic.
  // Task descriptions and native prompt echoes are not execution evidence.
  expect(events).toEqual(expect.arrayContaining([
    ...["question", "answer"].map((kind) => expect.objectContaining({
      kind: "peer",
      payload: expect.objectContaining({
        kind,
        text: expect.stringContaining("HIVE_TWO_MACHINE_HANDSHAKE"),
      }),
    })),
    expect.objectContaining({
      kind: "acknowledgment",
      payload: expect.objectContaining({ message_id: expect.stringMatching(/^(?!initial$).+/) }),
    }),
  ]));
}

test("live chat records a command request and shows its real outcome", async ({
  page,
}) => {
  test.skip(!process.env.HIVE_UI_LIVE_URL, "Requires a real backend.");
  test.setTimeout(200000);
  const url = await prepareLive(page);
  await loginLive(page, url);
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
  await expect(page).toHaveURL(/\/login\/?$/);
});

test("live named worker and two-machine collaboration", async ({ page }) => {
  const workers = (process.env.HIVE_UI_REMOTE_WORKERS || "")
    .split(",")
    .map((worker) => worker.trim())
    .filter(Boolean);
  test.skip(
    !process.env.HIVE_UI_LIVE_URL || workers.length !== 2,
    "Requires a real backend and two comma-separated HIVE_UI_REMOTE_WORKERS.",
  );
  test.setTimeout(900000);
  const [firstWorker, secondWorker] = workers;
  const codexModel = lowerTierModel("HIVE_UI_CODEX_MODEL", "gpt-5.6-luna");
  const claudeModel = lowerTierModel("HIVE_UI_CLAUDE_MODEL", "haiku");
  const url = await prepareLive(page);
  await loginLive(page, url);

  await page.getByRole("button", { name: "New chat" }).click();
  const singlePrompt = `Live QA only. Delegate exactly one assignment to the codex agent on ${firstWorker} using model ${codexModel}. Set assignment workspace to ~/hive-workspaces/qa-low-codex-${Date.now()} (keep the literal tilde prefix). This is already a disposable QA workspace; Hive's own bookkeeping files are allowed. Execute the single shell command printf HIVE_REMOTE_WORKER_OK as a standalone tool invocation, require exit code zero, and report its exact output. Do not create or remove extra directories, edit project files, or combine this command with cleanup commands. Do not use the local machine, any other worker, agent, or model. If the exact requested assignment is unavailable, report that instead of substituting.`;
  await page.getByLabel("Message Hive").fill(singlePrompt);
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(".message.user")).toContainText(firstWorker);
  await expectLivePlan(page);
  const singlePlacement = await expectLivePlacement(page, [
    { device: firstWorker, agent: "codex", model: codexModel },
  ]);
  await expect(page.locator(".run-card")).toHaveCount(1);
  const singleRun = page.locator(".run-card").filter({
    has: page.locator("strong", { hasText: `codex on ${firstWorker}` }),
  });
  await expect(singleRun).toHaveCount(1, { timeout: 30000 });
  await completeLiveRun(page, singleRun);
  expect(await readLiveEvents(singleRun)).toEqual(expect.arrayContaining([
    expect.objectContaining({
      kind: "native",
      payload: expect.objectContaining({
        method: "item/completed",
        params: expect.objectContaining({
          item: expect.objectContaining({
            type: "commandExecution",
            exitCode: 0,
            aggregatedOutput: expect.stringContaining("HIVE_REMOTE_WORKER_OK"),
          }),
        }),
      }),
    }),
  ]));
  await expectRuntimeModels(page, singlePlacement);

  await page.getByRole("button", { name: "New chat" }).click();
  const collaborationPrompt = `Live QA only. Delegate exactly two collaborating assignments: codex on ${firstWorker} using model ${codexModel} and claude on ${secondWorker} using model ${claudeModel}. Set each assignment workspace to a unique child of ~/hive-workspaces/qa-low-peer-${Date.now()} (keep the literal tilde prefix). Each agent must send the other a peer question containing HIVE_TWO_MACHINE_HANDSHAKE, send a peer answer acknowledging the received question and quoting HIVE_TWO_MACHINE_HANDSHAKE, and then report completion without modifying persistent files. Do not use the local machine, any other worker, agent, or model. If an exact requested assignment is unavailable, report that instead of substituting.`;
  await page.getByLabel("Message Hive").fill(collaborationPrompt);
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(".message.user")).toContainText(secondWorker);
  await expectLivePlan(page);

  const peerPlacement = await expectLivePlacement(page, [
    { device: firstWorker, agent: "codex", model: codexModel },
    { device: secondWorker, agent: "claude", model: claudeModel },
  ]);
  const cards = page.locator(".run-card");
  await expect(cards).toHaveCount(2, { timeout: 30000 });
  const firstCard = cards.filter({
    has: page.locator("strong", { hasText: `codex on ${firstWorker}` }),
  });
  const secondCard = cards.filter({
    has: page.locator("strong", { hasText: `claude on ${secondWorker}` }),
  });
  await expect(firstCard).toHaveCount(1);
  await expect(secondCard).toHaveCount(1);
  await Promise.all([
    completeLiveRun(page, firstCard),
    completeLiveRun(page, secondCard),
  ]);
  expectPeerHandshake(await readLiveEvents(firstCard));
  expectPeerHandshake(await readLiveEvents(secondCard));
  await expectRuntimeModels(page, peerPlacement);
  await page.screenshot({
    path: "/tmp/hive-ui-live-two-machine-collaboration.png",
    fullPage: true,
  });
});

test("live named opencode worker", async ({ page }) => {
  const worker = process.env.HIVE_UI_OPENCODE_WORKER?.trim() || "";
  test.skip(
    !process.env.HIVE_UI_LIVE_URL || !worker,
    "Requires a real backend and HIVE_UI_OPENCODE_WORKER.",
  );
  const model = lowerTierModel("HIVE_UI_OPENCODE_MODEL", "zai-coding-plan/glm-5.3-flash");
  test.setTimeout(600000);
  const url = await prepareLive(page);
  await loginLive(page, url);

  await page.getByRole("button", { name: "New chat" }).click();
  await page
    .getByLabel("Message Hive")
    .fill(
      `Live QA only. Delegate exactly one assignment to the opencode agent on ${worker} using model ${model}. Set assignment workspace to ~/hive-workspaces/qa-low-opencode-${Date.now()} (keep the literal tilde prefix). This is already a disposable QA workspace; Hive's own bookkeeping files are allowed. Execute the single shell command printf HIVE_OPENCODE_WORKER_OK as a standalone tool invocation, require exit code zero, and report its exact output. Do not create or remove extra directories, edit project files, or combine this command with cleanup commands. Do not use the local machine, another worker, agent, or model. If the exact requested assignment is unavailable, report that instead of substituting.`,
    );
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(".message.user")).toContainText(worker);
  await expectLivePlan(page);
  const placement = await expectLivePlacement(page, [
    { device: worker, agent: "opencode", model },
  ]);
  await expect(page.locator(".run-card")).toHaveCount(1);
  const run = page.locator(".run-card").filter({
    has: page.locator("strong", { hasText: `opencode on ${worker}` }),
  });
  await expect(run).toHaveCount(1, { timeout: 30000 });
  await completeLiveRun(page, run);
  expect(await readLiveEvents(run)).toEqual(expect.arrayContaining([
    expect.objectContaining({
      kind: "native",
      payload: expect.objectContaining({
        parts: expect.arrayContaining([
          expect.objectContaining({
            type: "tool",
            tool: "bash",
            state: expect.objectContaining({
              status: "completed",
              output: expect.stringContaining("HIVE_OPENCODE_WORKER_OK"),
            }),
          }),
        ]),
      }),
    }),
  ]));
  await expectRuntimeModels(page, placement);
});

test.describe("delegated sessions", () => {
  const approval = (extra: Record<string, unknown> = {}) => ({
    id: "ap-1",
    fingerprint: "digest",
    consumed: 0,
    reason: "Needs review",
    action: JSON.stringify({ arguments: { commandActions: [{ command: "make deploy" }], cwd: "/ws" } }),
    ...extra,
  });
  const withRuns = (page: Page, runs: unknown[]) =>
    page.route(/\/api\/runs(\?.*)?$/, (route) => route.fulfill({ json: runs }));

  test("nav badge counts sessions waiting on a person", async ({ page }) => {
    await withRuns(page, [
      { ...run, state: "awaiting-approval" },
      { ...run, id: "run-2", state: "needs-setup" },
      { ...run, id: "run-3", state: "failed" },
      { ...run, id: "run-4", state: "working" },
    ]);
    await page.goto("/machines/");
    await expect(page.locator(".nav-count")).toHaveText("2");
  });

  test("nav badge is hidden when nothing needs attention or runs are unavailable", async ({ page }) => {
    await withRuns(page, [{ ...run, state: "working" }]);
    await page.goto("/machines/");
    await expect(page.getByRole("heading", { name: "Machines" })).toBeVisible();
    await expect(page.locator(".nav-count")).toHaveCount(0);
    await page.route(/\/api\/runs$/, (route) => route.fulfill({ status: 400, body: "Delegation unavailable" }));
    await page.goto("/incidents/");
    await expect(page.locator(".nav-count")).toHaveCount(0);
    await expect(page.locator('p[role="alert"]')).toHaveCount(0);
  });

  test("an approval on a disconnected agent explains it can't be answered", async ({ page }) => {
    await withRuns(page, [{ ...run, state: "disconnected", metadata: { approvals: [approval()] } }]);
    await page.goto("/session/?run=run-1");
    await expect(page.locator(".approval")).toContainText("make deploy");
    await expect(page.locator(".approval")).toContainText("approving now has no effect");
    await expect(page.getByRole("button", { name: "Approve" })).toHaveCount(0);
  });

  test("a saved decision waits for the agent instead of asking again", async ({ page }) => {
    await withRuns(page, [{ ...run, state: "awaiting-approval", metadata: { approvals: [approval({ decision: "stop" })] } }]);
    await page.goto("/session/?run=run-1");
    await expect(page.locator(".approval [role=status]")).toContainText("You chose deny");
    await expect(page.getByRole("button", { name: "Approve" })).toHaveCount(0);
  });

  test("consumed approvals are not shown as pending", async ({ page }) => {
    await withRuns(page, [{ ...run, metadata: { approvals: [approval({ consumed: 1 })] } }]);
    await page.goto("/session/?run=run-1");
    await expect(page.getByText("codex on worker-a", { exact: true })).toBeVisible();
    await expect(page.locator(".approval")).toHaveCount(0);
  });

  test("a rejected decision shows the server error and can be retried", async ({ page }) => {
    let calls = 0;
    await withRuns(page, [{ ...run, state: "awaiting-approval", metadata: { approvals: [approval()] } }]);
    await page.route("**/api/runs/run-1/decisions", (route) => {
      calls++;
      return calls === 1
        ? route.fulfill({ status: 400, body: "Approval fingerprint changed" })
        : route.fulfill({ json: { saved: true } });
    });
    await page.goto("/session/?run=run-1");
    await page.getByRole("button", { name: "Approve" }).click();
    await expect(page.locator(".approval [role=alert]")).toHaveText("Approval fingerprint changed");
    await page.getByRole("button", { name: "Approve" }).click();
    await expect.poll(() => calls).toBe(2);
    await expect(page.locator(".approval [role=alert]")).toHaveCount(0);
  });

  test("file-change approvals name the files instead of a command", async ({ page }) => {
    await withRuns(page, [
      {
        ...run,
        state: "awaiting-approval",
        metadata: {
          approvals: [
            approval({
              action: JSON.stringify({ tool: "item/fileChange/requestApproval", arguments: { itemId: "c" } }),
              details: { changes: [{ path: "/ws/app.py" }] },
            }),
          ],
        },
      },
    ]);
    await page.goto("/session/?run=run-1");
    await expect(page.locator(".approval strong").first()).toHaveText("codex wants to change files");
    await expect(page.locator(".approval pre")).toHaveText("Edit /ws/app.py");
  });

  test("session page handles a missing run id and a deleted run", async ({ page }) => {
    await withRuns(page, [run]);
    await page.goto("/session/");
    await expect(page.getByText("No session selected.")).toBeVisible();
    await page.goto("/session/?run=gone");
    await expect(page.getByText("This session no longer exists.")).toBeVisible();
  });

  test("session page reports a runs API failure", async ({ page }) => {
    await page.route(/\/api\/runs$/, (route) => route.fulfill({ status: 400, body: "Delegation unavailable" }));
    await page.goto("/session/?run=run-1");
    await expect(page.locator('main [role="alert"]')).toHaveText("Delegation unavailable");
  });

  test("kill asks first and deletes the run's tmux session on its device", async ({ page }) => {
    const deletes: string[] = [];
    await withRuns(page, [run]);
    await page.route("**/api/sessions/**", (route) => {
      deletes.push(new URL(route.request().url()).pathname + new URL(route.request().url()).search);
      return route.fulfill({ status: 204 });
    });
    await page.goto("/session/?run=run-1");
    page.once("dialog", (dialog) => dialog.dismiss());
    await page.getByRole("button", { name: "Kill session" }).click();
    expect(deletes).toEqual([]);
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: "Kill session" }).click();
    await expect.poll(() => deletes).toEqual(["/api/sessions/agent-1?host=worker-a"]);
  });

  test("kill failure is shown on the session page", async ({ page }) => {
    await withRuns(page, [run]);
    await page.route("**/api/sessions/**", (route) => route.fulfill({ status: 502, body: "SSH unavailable" }));
    await page.goto("/session/?run=run-1");
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: "Kill session" }).click();
    await expect(page.locator('main > [role="alert"]')).toHaveText("SSH unavailable");
  });

  test("same-task sessions link to each other and load their own activity", async ({ page }) => {
    const peer = { ...run, id: "run-2", assignment: { ...run.assignment, key: "claude-b", device: "worker-b", agent: "claude" } };
    await withRuns(page, [run, peer, { ...run, id: "run-9", task_id: "other-task" }]);
    await page.route("**/api/runs/*/events*", (route) => {
      const own = route.request().url().includes("/run-2/") ? "Peer answer ready." : "Codex asked a question.";
      return route.fulfill({
        json: new URL(route.request().url()).searchParams.get("tail")
          ? [{ seq: 1, kind: "native", payload: { method: "item/completed", params: { item: { type: "agentMessage", text: own } } } }]
          : [],
      });
    });
    await page.goto("/session/?run=run-1");
    await expect(page.getByText("Codex asked a question.")).toBeVisible();
    const siblings = page.locator(".sibling");
    await expect(siblings).toHaveCount(2);
    await siblings.filter({ hasText: "claude on worker-b" }).click();
    await expect(page).toHaveURL(/run=run-2/);
    await expect(page.getByText("Peer answer ready.")).toBeVisible();
    await expect(page.getByText("Codex asked a question.")).toHaveCount(0);
  });

  test("finished sessions load once and stop polling", async ({ page }) => {
    const requests: string[] = [];
    await withRuns(page, [{ ...run, state: "completed" }]);
    await page.route("**/api/runs/run-1/events*", (route) => {
      requests.push(new URL(route.request().url()).search);
      return route.fulfill({ json: [] });
    });
    await page.goto("/session/?run=run-1");
    await expect(page.getByText("No agent activity yet.")).toBeVisible();
    await page.waitForTimeout(4500);
    expect(requests).toEqual(["?tail=500"]);
  });

  test("Enter sends to the agent, Shift+Enter adds a line, superseded runs can't be messaged", async ({ page }) => {
    const sent: string[] = [];
    await withRuns(page, [run, { ...run, id: "run-2", state: "superseded" }]);
    await page.route("**/api/runs/run-1/messages", (route) => {
      sent.push(route.request().postDataJSON().text);
      return route.fulfill({ json: { saved: true } });
    });
    await page.goto("/session/?run=run-1");
    const input = page.getByLabel("Message codex on worker-a");
    await input.fill("line one");
    await input.press("Shift+Enter");
    await input.pressSequentially("line two");
    await input.press("Enter");
    await expect.poll(() => sent).toEqual(["line one\nline two"]);
    await expect(input).toHaveValue("");
    await page.goto("/session/?run=run-2");
    await expect(page.getByLabel("Message codex on worker-a")).toBeDisabled();
    await expect(page.getByRole("button", { name: "Send", exact: true })).toBeDisabled();
  });

  test("retry setup is offered for a run that never launched", async ({ page }) => {
    let retried = false;
    await withRuns(page, [{ ...run, state: "needs-setup", runner_path: null }]);
    await page.route("**/api/runs/run-1/retry-setup", (route) => {
      retried = true;
      return route.fulfill({ json: { saved: true } });
    });
    await page.goto("/session/?run=run-1");
    await expect(page.locator(".banner")).toContainText("The agent never launched on worker-a");
    await page.getByRole("button", { name: "Retry setup" }).click();
    await expect.poll(() => retried).toBe(true);
  });

  test("a Claude session renders its commands, tools and failures", async ({ page }) => {
    await withRuns(page, [{ ...run, assignment: { ...run.assignment, agent: "claude" } }]);
    await page.route("**/api/runs/run-1/events*", (route) =>
      route.fulfill({
        json: new URL(route.request().url()).searchParams.get("tail")
          ? [
              { seq: 1, kind: "native", payload: { type: "system", subtype: "init" } },
              { seq: 2, kind: "native", payload: { type: "assistant", message: { content: [{ type: "text", text: "Writing tests." }] } } },
              { seq: 3, kind: "native", payload: { type: "assistant", message: { content: [{ type: "tool_use", name: "Bash", input: { command: "python3 -m unittest" } }] } } },
              { seq: 4, kind: "native", payload: { type: "user", message: { content: [{ type: "tool_result", is_error: true, content: "1 test failed" }] } } },
              { seq: 5, kind: "native", payload: { type: "assistant", message: { content: [{ type: "tool_use", name: "Edit", input: { file_path: "a.py" } }] } } },
            ]
          : [],
      }),
    );
    await page.goto("/session/?run=run-1");
    await expect(page.locator(".t-agent")).toHaveText("Writing tests.");
    await expect(page.locator(".t-cmd summary").first()).toContainText("$ python3 -m unittest");
    await expect(page.locator(".t-error")).toHaveText("1 test failed");
    await expect(page.locator(".t-cmd summary").nth(1)).toContainText("Tool Edit");
  });

  test("a failed command opens its output; successful ones stay collapsed", async ({ page }) => {
    await withRuns(page, [{ ...run, state: "completed" }]);
    const command = (seq: number, exitCode: number, out: string) => ({
      seq,
      kind: "native",
      payload: { method: "item/completed", params: { item: { type: "commandExecution", commandActions: [{ command: `step${seq}` }], exitCode, aggregatedOutput: out } } },
    });
    await page.route("**/api/runs/run-1/events*", (route) =>
      route.fulfill({ json: [command(1, 0, "fine-output"), command(2, 3, "boom-output")] }),
    );
    await page.goto("/session/?run=run-1");
    await expect(page.getByText("boom-output")).toBeVisible();
    await expect(page.getByText("fine-output")).toBeHidden();
    await expect(page.locator(".exit.bad")).toHaveText("exit 3");
  });

  test("chat cards show the latest agent message and why a run failed", async ({ page }) => {
    await withRuns(page, [{ ...run, state: "failed" }]);
    await page.route("**/api/runs/run-1/events*", (route) =>
      route.fulfill({
        json: [
          { seq: 1, kind: "native", payload: { method: "item/completed", params: { item: { type: "agentMessage", text: "Starting the server." } } } },
          { seq: 2, kind: "error", payload: { message: "Port 8080 already in use" } },
        ],
      }),
    );
    await page.goto("/?chat=chat-1");
    const card = page.locator(".run-card");
    await expect(card.locator(".latest")).toContainText("Starting the server.");
    await expect(card.locator(".banner.bad")).toContainText("Port 8080 already in use");
    await expect(card.locator("[data-state]").first()).toHaveAttribute("data-state", "failed");
  });

  test("chat runs from a turn that isn't loaded still appear at the end", async ({ page }) => {
    await withRuns(page, [{ ...run, task_id: "older-turn" }]);
    await page.goto("/?chat=chat-1");
    await expect(page.locator(".chat-feed > .run-card")).toHaveCount(1);
    await expect(page.locator(".message .run-card")).toHaveCount(0);
  });

  test("sessions page shows running and queued groups and an empty state", async ({ page }) => {
    let list: unknown[] = [
      { ...session, name: "a", run: { ...run, state: "working" } },
      { ...session, name: "b", run: { ...run, id: "run-2", state: "waiting-for-peer" } },
      { ...session, name: "c", windows: 0, run: { ...run, id: "run-3", state: "queued" } },
    ];
    await page.route("**/api/sessions", (route) => route.fulfill({ json: list }));
    await page.goto("/sessions/");
    await expect(page.locator(".session-group", { hasText: "Running" }).locator(".session-card")).toHaveCount(2);
    await expect(page.locator(".session-group", { hasText: "Queued" }).locator(".session-card")).toHaveCount(1);
    await expect(page.locator(".session-group", { hasText: "Needs attention" })).toHaveCount(0);
    list = [];
    await page.reload();
    await expect(page.getByText("No agent sessions yet.", { exact: false })).toBeVisible();
    await expect(page.getByText("No terminal sessions. Start one above.")).toBeVisible();
  });
});

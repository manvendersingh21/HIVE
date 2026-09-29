import { test, expect, Page, Route } from "@playwright/test";

// Regressions from the frontend audit: malformed-but-legal API payloads must
// render, finished sessions must stop polling, and in-flight requests must not
// be applied to the wrong machine.

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
const teammate = {
  agent_id: "run-1",
  key: "codex-a",
  role: "codex-a",
  agent: "codex",
  model: null,
  device: "worker-a",
  owned_paths: ["src/**"],
  current_task: "Collaborate",
  status: "working",
  dependencies: [],
  last_seen: null,
  relay_fingerprint: "fp",
};
const audit = (entries: unknown[] = []) => ({
  mode: "Relay-attested (HACP Secure degraded mode)",
  chain_valid: true,
  total_entries: entries.length,
  head: { position: entries.length, digest: "" },
  entries,
});

type Handler = (route: Route, url: URL) => Promise<void> | void;

// Mocked API: `overrides` win by pathname; anything unknown is a loud 404.
async function backend(page: Page, overrides: Record<string, Handler> = {}) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const defaults: Record<string, unknown> = {
    "/api/capabilities": { chat: true, terminal: true, master_name: "local" },
    "/api/chats": [{ id: "chat-1", title: "Machine work", updated_at: "Today" }],
    "/api/chats/chat-1": { messages: [] },
    "/api/runs": [],
    "/api/runs/run-1/events": [],
    "/api/tasks/turn-1/team": [teammate],
    "/api/runs/run-1/audit": audit(),
    "/api/sessions": [],
    "/api/session-hosts": [{ host: "local", name: "local" }],
    "/api/settings/master-agent": {
      provider: "local",
      options: [{ id: "local", label: "Local", requires_api_key: false, configured: true }],
      local_model: "qwen3.5:9b",
      local_available: true,
    },
    "/api/settings/autonomy": { mode: "yolo" },
    "/api/fleet": [],
    "/api/fleet/ssh/key": { public_key: "ssh-ed25519 AAAA", path: "/k", config_included: true },
    "/api/containers": [],
    "/api/containers/hosts": ["local"],
  };
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    const override = overrides[url.pathname];
    if (override) return override(route, url);
    if (url.pathname in defaults) return route.fulfill({ json: defaults[url.pathname] });
    return route.fulfill({ status: 404, body: "Unexpected API request: " + url.pathname });
  });
  return errors;
}

const crashed = (page: Page) =>
  page.getByText("Application error: a client-side exception has occurred");

test("F-01: an awaiting_approval reply with an empty result renders without crashing", async ({ page }) => {
  const errors = await backend(page, {
    "/api/chats/chat-1": (route) =>
      route.fulfill({
        json: {
          messages: [
            {
              role: "assistant",
              content: "Review this command",
              status: "awaiting_approval",
              reply: {
                run: {
                  id: "plan-1",
                  steps: [{ id: 7, command: "rm -rf build", target: { kind: "remote", worker: "worker-a" } }],
                },
                result: {},
              },
            },
            {
              role: "assistant",
              content: "Second plan",
              status: "awaiting_approval",
              reply: {
                run: { id: "plan-2", steps: [{ id: 8 }] },
                result: { awaiting_approval: [8] },
              },
            },
          ],
        },
      }),
  });
  await page.goto("/?chat=chat-1");
  await expect(page.getByText("Review this command")).toBeVisible();
  const first = page.locator(".message.assistant").first();
  await expect(first.locator(".approval")).toHaveCount(0);
  await expect(first.locator("small")).toHaveText("Needs approval");
  // A step with no target or command still asks, with neutral wording.
  const second = page.locator(".message.assistant").nth(1).locator(".approval");
  await expect(second).toContainText("Hive wants to run a command on a machine");
  await expect(second.locator("pre.cmd")).toHaveText("(command unavailable)");
  await expect(crashed(page)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("F-02: relay audit entries with a null or missing detail render", async ({ page }) => {
  const entry = (position: number, detail: unknown) => ({
    digest: `digest-${position}`,
    record: {
      position,
      event: `event-${position}`,
      at: "Today",
      message_id: `msg-${position}`,
      seq: position,
      ...(detail === undefined ? {} : { detail }),
    },
  });
  const errors = await backend(page, {
    "/api/runs": (route) => route.fulfill({ json: [run] }),
    "/api/runs/run-1/audit": (route) =>
      route.fulfill({
        json: audit([entry(1, null), entry(2, undefined), entry(3, { reason: "Peer rejected" })]),
      }),
  });
  await page.goto("/session/?run=run-1");
  const panel = page.getByRole("region", { name: "Relay audit" });
  await expect(panel.getByRole("status")).toHaveText("Audit chain verified");
  await panel.getByText("Recent audit events (3)").click();
  await expect(panel.locator("li strong")).toHaveText(["event-1", "event-2", "event-3"]);
  await expect(panel.getByText("Peer rejected")).toBeVisible();
  await expect(crashed(page)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("F-03: teammates without owned paths or dependencies show fallbacks", async ({ page }) => {
  const { owned_paths: _paths, dependencies: _deps, ...bare } = teammate;
  const errors = await backend(page, {
    "/api/runs": (route) => route.fulfill({ json: [run] }),
    "/api/tasks/turn-1/team": (route) =>
      route.fulfill({
        json: [
          bare,
          { ...teammate, agent_id: "run-2", role: "reviewer", owned_paths: null, dependencies: null },
        ],
      }),
  });
  await page.goto("/session/?run=run-1");
  const team = page.getByRole("region", { name: "Team" });
  await expect(team.locator("article")).toHaveCount(2);
  for (const member of await team.locator("article").all()) {
    await expect(member.locator("dd").nth(0)).toHaveText("None assigned");
    await expect(member.locator("dd").nth(1)).toHaveText("None");
  }
  await expect(crashed(page)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test.describe("F-04: session polling", () => {
  function counting(page: Page, runState: () => unknown[], teamStatus = "completed") {
    const hits = { runs: 0, team: 0, audit: 0 };
    return {
      hits,
      ready: backend(page, {
        "/api/runs": (route, url) => {
          if (url.searchParams.get("task_of") === "run-1") hits.runs++;
          return route.fulfill({ json: url.searchParams.has("task_of") ? runState() : [] });
        },
        "/api/tasks/turn-1/team": (route) => {
          hits.team++;
          return route.fulfill({ json: [{ ...teammate, status: teamStatus }] });
        },
        "/api/runs/run-1/audit": (route) => {
          hits.audit++;
          return route.fulfill({ json: audit() });
        },
      }),
    };
  }

  test("a finished task loads run, team and audit once and stops polling", async ({ page }) => {
    const api = counting(page, () => [{ ...run, state: "completed" }]);
    await api.ready;
    await page.goto("/session/?run=run-1");
    await expect(page.getByRole("region", { name: "Team" }).locator("article")).toHaveCount(1);
    await expect(page.getByRole("region", { name: "Relay audit" }).getByRole("status")).toBeVisible();
    // Longer than both poll intervals (3s and 5s) plus slack.
    await page.waitForTimeout(6500);
    expect(api.hits).toEqual({ runs: 1, team: 1, audit: 1 });
  });

  test("polling stops once a live run reaches a terminal state", async ({ page }) => {
    let calls = 0;
    const api = counting(page, () => [{ ...run, state: ++calls < 2 ? "working" : "failed" }]);
    await api.ready;
    await page.goto("/session/?run=run-1");
    await expect.poll(() => api.hits.runs, { timeout: 10000 }).toBe(2);
    await page.waitForTimeout(500);
    const settled = { ...api.hits };
    await page.waitForTimeout(6500);
    expect(api.hits).toEqual(settled);
  });

  test("a live teammate keeps the page polling after this run finishes", async ({ page }) => {
    const api = counting(
      page,
      () => [
        { ...run, state: "completed" },
        { ...run, id: "run-2", state: "working" },
      ],
      "working",
    );
    await api.ready;
    await page.goto("/session/?run=run-1");
    await expect.poll(() => api.hits.runs, { timeout: 10000 }).toBeGreaterThanOrEqual(2);
    await expect.poll(() => api.hits.team, { timeout: 10000 }).toBeGreaterThanOrEqual(2);
    await expect.poll(() => api.hits.audit, { timeout: 12000 }).toBeGreaterThanOrEqual(2);
  });
});

test("F-09: the machine can't change while containers are listed, and a stale list is dropped", async ({ page }) => {
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  const errors = await backend(page, {
    "/api/containers/hosts": (route) => route.fulfill({ json: ["alpha", "beta"] }),
    "/api/containers/available": async (route, url) => {
      if (url.searchParams.get("host") === "alpha") await held;
      return route.fulfill({
        json: [{ container: `${url.searchParams.get("host")}-only`, image: "img", status: "Up", running: true }],
      });
    },
  });
  await page.goto("/settings/");
  const machine = page.getByLabel("Docker machine");
  await expect(machine).toHaveValue("alpha");
  await page.getByRole("button", { name: "List containers" }).click();
  await expect(page.getByRole("button", { name: "Listing…" })).toBeVisible();
  await expect(machine).toBeDisabled();
  // Even if the host changes anyway (e.g. a script), alpha's list must not
  // be offered under beta.
  await machine.evaluate((el: HTMLSelectElement) => {
    const set = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!;
    set.call(el, "beta");
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(machine).toHaveValue("beta");
  release();
  await expect(page.getByRole("button", { name: "List containers" })).toBeEnabled();
  await expect(page.getByText("alpha-only")).toHaveCount(0);
  await expect(machine).toBeEnabled();
  expect(errors).toEqual([]);
});

test("F-10: a non-JSON session-errors header is shown as text and hosts still load", async ({ page }) => {
  const errors = await backend(page, {
    "/api/sessions": (route) =>
      route.fulfill({ json: [], headers: { "x-hive-session-errors": "worker-b: unreachable" } }),
    "/api/session-hosts": (route) =>
      route.fulfill({
        json: [
          { host: "local", name: "local" },
          { host: "worker-z", name: "worker-z" },
        ],
      }),
  });
  await page.goto("/sessions/");
  await expect(page.getByRole("status")).toHaveText(
    "Some machines could not be listed: worker-b: unreachable",
  );
  await expect(page.getByLabel("Machine", { exact: true }).locator("option")).toHaveText([
    "local",
    "worker-z",
  ]);
  await expect(page.locator("main").getByRole("alert")).toHaveCount(0);
  expect(errors).toEqual([]);
});

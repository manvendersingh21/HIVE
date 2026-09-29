import { test, expect, Page } from "@playwright/test";

// Two runs of the same task, on different machines, so the session page can
// switch between siblings and teammates while staying on one route.
const runA = {
  id: "run-1",
  task_id: "turn-1",
  conversation_id: "chat-1",
  tmux_name: "agent-frontend",
  state: "working",
  runner_path: "/tmp/runner",
  assignment: {
    key: "codex-a",
    device: "worker-a",
    agent: "codex",
    objective: "Ship the frontend fixes",
    workspace: "~/hive-workspaces/frontend",
    dependencies: [],
    acceptance_criteria: ["No overflow"],
  },
  metadata: {},
};
const runB = {
  ...runA,
  id: "run-2",
  tmux_name: "agent-backend",
  assignment: {
    ...runA.assignment,
    key: "claude-b",
    device: "worker-b",
    agent: "claude",
  },
};
const audit = {
  mode: "Relay-attested (HACP Secure degraded mode)",
  chain_valid: true,
  entries: [],
  total_entries: 0,
  head: { position: 0, digest: "" },
};
// One unbroken token, so neither the browser nor a line break can split it.
const LONG =
  "worker0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000.internal";
const team = [
  {
    agent_id: runA.id,
    key: "frontend",
    role: "frontend",
    agent: "codex",
    model: null,
    device: "worker-a",
    owned_paths: [],
    current_task: "turn-1",
    status: "working",
    dependencies: [],
    last_seen: null,
    relay_fingerprint: "",
  },
  {
    agent_id: runB.id,
    key: "backend",
    role: "backend",
    agent: "claude",
    model: null,
    device: "worker-b",
    owned_paths: [],
    current_task: "turn-1",
    status: "working",
    dependencies: [],
    last_seen: null,
    relay_fingerprint: "",
  },
];

// The whole API these two pages read. Anything unlisted 404s loudly, so a test
// cannot pass on a page that silently failed to load its data.
async function sessionApi(page: Page, attention = 0) {
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    const { pathname } = url;
    if (pathname === "/api/capabilities")
      return route.fulfill({ json: { chat: true, terminal: true, master_name: "local" } });
    // The nav bar's attention badge only appears when the server reports runs
    // parked on a person; it is what pushed the topbar past a phone viewport.
    if (pathname === "/api/runs" && url.searchParams.has("state"))
      return route.fulfill({
        json: Array.from({ length: attention }, () => ({
          ...runA,
          state: "awaiting-approval",
        })),
      });
    if (pathname === "/api/runs") return route.fulfill({ json: [runA, runB] });
    if (/^\/api\/runs\/[^/]+\/events$/.test(pathname)) return route.fulfill({ json: [] });
    if (/^\/api\/runs\/[^/]+\/audit$/.test(pathname)) return route.fulfill({ json: audit });
    if (/^\/api\/tasks\/[^/]+\/team$/.test(pathname)) return route.fulfill({ json: team });
    if (pathname === "/api/chats")
      return route.fulfill({
        json: [
          { id: "chat-1", title: "Machine work", updated_at: "Today" },
          { id: "chat-2", title: "Deploy notes", updated_at: "Yesterday" },
        ],
      });
    if (/^\/api\/chats\/[^/]+$/.test(pathname)) {
      const id = decodeURIComponent(pathname.split("/").pop()!);
      return route.fulfill({
        json: { messages: [{ role: "assistant", content: `Opened ${id}` }] },
      });
    }
    if (pathname === "/api/session-hosts")
      return route.fulfill({ json: [{ host: "local", name: "local" }] });
    if (pathname === "/api/sessions") {
      // A long host and a long command with no break opportunity in them, as
      // a tailnet name and an absolute path really do produce. This is what
      // used to push the card past a phone viewport.
      return route.fulfill({
        json: [
          {
            name: "ui-test",
            host: LONG,
            windows: 1,
            attached: false,
            current_command: LONG,
            window_name: "",
          },
        ],
      });
    }
    return route.fulfill({
      status: 404,
      body: "Unexpected API request: " + pathname,
    });
  });
}

// Which run the page is actually showing: the composer's label names the agent
// and device of the run behind the transcript.
const showing = (page: Page, run: typeof runA) =>
  page.getByLabel(`Message ${run.assignment.agent} on ${run.assignment.device}`);

async function showRun(page: Page, run: typeof runA) {
  await expect(showing(page, run)).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`\\?run=${run.id}`));
}

test("F-05 session view follows the URL through Back, Forward and reload", async ({ page }) => {
  await sessionApi(page);
  await page.goto("/session/?run=run-1");
  await showRun(page, runA);

  // Switching sibling is a client-side navigation inside /session/, so the
  // page component is never remounted.
  await page.locator(".sibling", { hasText: "claude on worker-b" }).click();
  await showRun(page, runB);

  // Back must restore both the URL and the run it points at.
  await page.goBack();
  await showRun(page, runA);
  await page.goForward();
  await showRun(page, runB);

  // The same holds for a teammate picked from the roster.
  await page.getByRole("region", { name: "Team", exact: true })
    .getByRole("link", { name: "frontend", exact: true })
    .click();
  await showRun(page, runA);
  await page.goBack();
  await showRun(page, runB);

  // A reload rebuilds the page from the URL alone.
  await page.reload();
  await showRun(page, runB);
});

test("F-06 long commands and hosts do not overflow a 390px session card", async ({ page }) => {
  await sessionApi(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions/");
  const card = page.locator("article.card.row").filter({ hasText: "ui-test" });
  await expect(card).toBeVisible();
  await expect(card.locator(".muted")).toContainText(LONG);
  const width = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    inner: window.innerWidth,
  }));
  expect(width.scroll).toBeLessThanOrEqual(390);
});

test("F-07 the topbar with an attention badge fits a 390px viewport", async ({ page }) => {
  await sessionApi(page, 1);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  const badge = page.locator(".nav-count");
  await expect(badge).toHaveText("1");
  const width = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    inner: window.innerWidth,
  }));
  expect(width.scroll).toBeLessThanOrEqual(390);
});

test("F-08 the selected chat is kept in the URL and survives reload and Back", async ({ page }) => {
  await sessionApi(page);
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page).toHaveURL(/\?chat=chat-1$/);
  await expect(page.getByText("Opened chat-1")).toBeVisible();

  // Reload reads the chat back out of the URL.
  await page.reload();
  await expect(page).toHaveURL(/\?chat=chat-1$/);
  await expect(page.getByText("Opened chat-1")).toBeVisible();

  // Switching chats moves the deep link with it.
  await page.getByRole("button", { name: /Deploy notes/ }).click();
  await expect(page).toHaveURL(/\?chat=chat-2$/);
  await expect(page.getByText("Opened chat-2")).toBeVisible();
  await page.reload();
  await expect(page).toHaveURL(/\?chat=chat-2$/);
  await expect(page.getByText("Opened chat-2")).toBeVisible();

  // "New chat" clears the composer, so it must drop the stale deep link too.
  await page.getByRole("button", { name: "New chat" }).click();
  await expect(page).not.toHaveURL(/chat=/);
  await expect(page.getByText("Opened chat-2")).toHaveCount(0);
  await page.reload();
  await expect(page).not.toHaveURL(/chat=/);
  await expect(page.getByText("Opened chat-2")).toHaveCount(0);

  // Leaving and coming back with Back restores the chat the URL names.
  await page.getByRole("button", { name: /Deploy notes/ }).click();
  await expect(page).toHaveURL(/\?chat=chat-2$/);
  await page.getByRole("link", { name: "Sessions", exact: true }).click();
  await expect(page).toHaveURL(/\/sessions\/$/);
  await page.goBack();
  await expect(page).toHaveURL(/\?chat=chat-2$/);
  await expect(page.getByText("Opened chat-2")).toBeVisible();
});

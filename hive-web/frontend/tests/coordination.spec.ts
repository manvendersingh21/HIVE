import { expect, test } from "@playwright/test";

const frozen = "a".repeat(64);
const proposed = "b".repeat(64);
const contract = {
  contract: {
    contract_id: "contract:task:api",
    participants: ["urn:hacp:agent:a", "urn:hacp:agent:b"],
    state: "amending",
    revisions: [{ number: 1, digest: frozen, content: { agreement: "API v1: GET /health returns ready" } }],
  },
  proposed_digest: proposed,
  proposed_terms: { agreement: "API v2 pending review" },
};
const runs = ["a", "b"].map((id) => ({
  id, task_id: "task", conversation_id: "chat", tmux_name: `test-${id}`, state: "no_agreement",
  assignment: { key: id === "a" ? "api-owner" : "api-consumer", agent: "codex", device: `worker-${id}`,
    workspace: "~/hive-workspaces/test", objective: "Agree and verify the API", acceptance_criteria: ["health works"],
    owned_paths: [id === "a" ? "server/**" : "client/**"], max_rework: 2 },
  contracts: [contract],
  completion: { record: { verdict: "no_agreement", rework_rounds: 2, evidence: ["failed required file: health.json"] },
    measurements: [{ passed: false, detail: "health.json: required file missing" }] },
}));

test.beforeEach(async ({ page }) => {
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const data = path === "/api/runs" ? runs
      : path === "/api/capabilities" ? { chat: true, terminal: true, master_name: "local" }
      : path.endsWith("/audit") ? { entries: [], chain_valid: true, total_entries: 0 }
      : [];
    await route.fulfill({ json: data });
  });
});

test("both participant session pages show the same frozen contract and separate the pending amendment", async ({ page }) => {
  for (const id of ["a", "b"]) {
    await page.goto(`/session/?run=${id}`);
    const panel = page.getByRole("region", { name: "Interface contracts" });
    await expect(panel.getByText("Frozen revision 1")).toBeVisible();
    await expect(panel.getByText(frozen, { exact: true })).toBeVisible();
    await expect(panel.getByText("API v1: GET /health returns ready")).toBeVisible();
    await expect(panel.getByRole("link", { name: "api-owner" })).toHaveAttribute("href", "/session/?run=a");
    await expect(panel.getByRole("link", { name: "api-consumer" })).toHaveAttribute("href", "/session/?run=b");
    await expect(panel.getByText(proposed, { exact: true })).not.toBeVisible();
    await panel.getByText("Amendment pending both parties").click();
    await expect(panel.getByText(proposed, { exact: true })).toBeVisible();
    await expect(panel.getByText("API v2 pending review")).toBeVisible();
    await expect(panel.getByText(frozen, { exact: true })).toBeVisible();
    await expect(page.getByRole("textbox", { name: `Message codex on worker-${id}` })).toBeVisible();
  }
  await page.getByRole("region", { name: "Interface contracts" }).getByRole("link", { name: "api-owner" }).click();
  await expect(page).toHaveURL(/run=a$/);
  await expect(page.getByRole("textbox", { name: "Message codex on worker-a" })).toBeVisible();
  await expect(page.getByRole("textbox", { name: "Message codex on worker-b" })).toHaveCount(0);
});

test("terminal acceptance failure keeps measured evidence visible and closes the composer", async ({ page }) => {
  await page.goto("/session/?run=a");
  const evidence = page.getByRole("region", { name: "Acceptance evidence" });
  await expect(evidence.getByText("No agreement", { exact: true })).toBeVisible();
  await expect(evidence.getByText("Rework rounds: 2 / 2")).toBeVisible();
  await evidence.getByText("Check 1: failed").click();
  await expect(evidence.getByText("health.json: required file missing")).toBeVisible();
  await evidence.getByText("Verdict history").click();
  await expect(evidence.getByText("failed required file: health.json")).toBeVisible();
  await expect(page.locator("textarea")).toBeDisabled();
});

test("an unfrozen proposal is never labeled frozen", async ({ page }) => {
  await page.route("**/api/runs?*", (route) => route.fulfill({ json: [{ ...runs[0],
    contracts: [{ ...contract, contract: { ...contract.contract, state: "proposed", revisions: [] } }],
  }] }));
  await page.goto("/session/?run=a");
  const panel = page.getByRole("region", { name: "Interface contracts" });
  await expect(panel.getByText("Proposal awaiting agreement")).toBeVisible();
  await expect(panel.getByText(/Frozen revision/)).toHaveCount(0);
  await expect(panel.getByText(frozen, { exact: true })).toHaveCount(0);
});

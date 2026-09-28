import { test, expect, Page } from "@playwright/test";

type Chat = { id: string; title: string; updated_at: string };

// A small stateful stand-in for the chat API: DELETE really removes the chat,
// so a list reload (if the page did one) would show the truth.
async function backend(page: Page, onDelete: (id: string) => { status: number; body?: string }) {
  let chats: Chat[] = [
    { id: "chat-1", title: "Machine work", updated_at: "Today" },
    { id: "chat-2", title: "Deploy notes", updated_at: "Yesterday" },
  ];
  const deletes: string[] = [];
  let listLoads = 0;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/capabilities")
      return route.fulfill({ json: { chat: true, terminal: true, master_name: "local" } });
    if (path === "/api/chats" && request.method() === "GET") {
      listLoads++;
      return route.fulfill({ json: chats });
    }
    if (path === "/api/runs") return route.fulfill({ json: [] });
    const match = path.match(/^\/api\/chats\/([^/]+)$/);
    if (match) {
      const id = decodeURIComponent(match[1]);
      if (request.method() === "DELETE") {
        deletes.push(id);
        const result = onDelete(id);
        if (result.status === 204) chats = chats.filter((c) => c.id !== id);
        return route.fulfill({ status: result.status, body: result.body ?? "" });
      }
      if (!chats.some((c) => c.id === id))
        return route.fulfill({ status: 404, body: "Chat not found" });
      return route.fulfill({
        json: { messages: [{ role: "user", content: `Opened ${id}` }] },
      });
    }
    return route.fulfill({ status: 404, body: "Unexpected API request: " + path });
  });
  return { deletes, listLoads: () => listLoads };
}

const row = (page: Page, id: string) => page.locator(`.chat-row[data-chat-id="${id}"]`);

test("deleting a chat asks inline, then removes it without reloading", async ({ page }) => {
  let dialogs = 0;
  page.on("dialog", (d) => {
    dialogs++;
    void d.dismiss();
  });
  const api = await backend(page, () => ({ status: 204 }));
  await page.goto("/");
  await expect(page.getByRole("button", { name: /Machine work/ })).toBeVisible();
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("Opened chat-1")).toBeVisible();
  // Mark the document: a full page reload would lose this.
  await page.evaluate(() => ((window as unknown as { marker: number }).marker = 42));

  // First click only asks; nothing is sent yet.
  await row(page, "chat-1").getByRole("button", { name: "Delete chat" }).click();
  const confirm = row(page, "chat-1").getByRole("group", { name: "Confirm delete chat" });
  await expect(confirm).toContainText("Delete this chat?");
  expect(api.deletes).toEqual([]);

  const loadsBefore = api.listLoads();
  const sent = page.waitForRequest(
    (r) => r.method() === "DELETE" && r.url().endsWith("/api/chats/chat-1"),
  );
  await confirm.getByRole("button", { name: "Delete", exact: true }).click();
  await sent;

  await expect(row(page, "chat-1")).toHaveCount(0);
  await expect(page.getByRole("button", { name: /Machine work/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /Deploy notes/ })).toBeVisible();
  // The open conversation was the deleted one, so the view is cleared.
  await expect(page.getByText("Opened chat-1")).toHaveCount(0);
  expect(api.deletes).toEqual(["chat-1"]);
  expect(api.listLoads()).toBe(loadsBefore);
  expect(await page.evaluate(() => (window as unknown as { marker?: number }).marker)).toBe(42);
  expect(dialogs).toBe(0);
});

test("cancelling the inline confirmation keeps the chat and sends nothing", async ({ page }) => {
  const api = await backend(page, () => ({ status: 204 }));
  await page.goto("/");
  await expect(page.getByRole("button", { name: /Machine work/ })).toBeVisible();
  await row(page, "chat-2").getByRole("button", { name: "Delete chat" }).click();
  const confirm = row(page, "chat-2").getByRole("group", { name: "Confirm delete chat" });
  await confirm.getByRole("button", { name: "Cancel" }).click();
  await expect(confirm).toHaveCount(0);
  await expect(page.getByRole("button", { name: /Deploy notes/ })).toBeVisible();
  await expect(row(page, "chat-2").getByRole("button", { name: "Delete chat" })).toBeVisible();
  expect(api.deletes).toEqual([]);
});

test("deleting another chat keeps the open conversation", async ({ page }) => {
  await backend(page, () => ({ status: 204 }));
  await page.goto("/");
  await page.getByRole("button", { name: /Machine work/ }).click();
  await expect(page.getByText("Opened chat-1")).toBeVisible();
  await row(page, "chat-2").getByRole("button", { name: "Delete chat" }).click();
  await row(page, "chat-2").getByRole("button", { name: "Delete", exact: true }).click();
  await expect(row(page, "chat-2")).toHaveCount(0);
  await expect(page.getByText("Opened chat-1")).toBeVisible();
  await expect(page.getByRole("button", { name: /Machine work/ })).toBeVisible();
});

test("a chat with live runs is kept and the server's reason is shown", async ({ page }) => {
  const reason =
    "This chat still has 1 delegated run in progress. Stop or finish it before deleting the chat.";
  const api = await backend(page, () => ({ status: 409, body: reason }));
  await page.goto("/");
  await expect(page.getByRole("button", { name: /Machine work/ })).toBeVisible();
  await row(page, "chat-1").getByRole("button", { name: "Delete chat" }).click();
  await row(page, "chat-1").getByRole("button", { name: "Delete", exact: true }).click();
  await expect(row(page, "chat-1").getByRole("alert")).toHaveText(reason);
  await expect(page.getByRole("button", { name: /Machine work/ })).toBeVisible();
  expect(api.deletes).toEqual(["chat-1"]);
  // Cancelling dismisses the error and restores the normal row.
  await row(page, "chat-1").getByRole("button", { name: "Cancel" }).click();
  await expect(row(page, "chat-1").getByRole("alert")).toHaveCount(0);
  await expect(row(page, "chat-1").getByRole("button", { name: "Delete chat" })).toBeVisible();
});

import { expect, test } from "@playwright/test";
import type { SavedReply } from "@/features/profile/saved-replies";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("account saved replies can be edited, inserted at the cursor marker, and deleted", async ({
  page,
}) => {
  await mockPullRequest(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: { issuer: "fixture", subject: "alice", name: "Alice" },
        csrf: "csrf",
      },
    }),
  );
  await page.route("**/api/profile", (route) =>
    route.fulfill({ json: { profile: null } }),
  );
  let reply: SavedReply | undefined;
  const writes: { id: string; version: number }[] = [];
  await page.route(
    /\/api\/profile\/saved-replies(?:\/[^/]+)?$/,
    async (route) => {
      const request = route.request();
      const method = request.method();
      if (method === "GET") {
        return route.fulfill({ json: { items: reply ? [reply] : [] } });
      }
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      const id = new URL(request.url()).pathname.split("/").at(-1)!;
      const input = request.postDataJSON();
      expect(input.version).toBe(reply?.version ?? 0);
      if (reply) expect(id).toBe(reply.id);
      writes.push({ id, version: input.version });
      if (method === "DELETE") {
        reply = undefined;
        return route.fulfill({ json: null });
      }
      expect(method).toBe("PUT");
      reply = {
        id,
        version: input.version + 1,
        title: input.title,
        body: input.body,
      };
      return route.fulfill({ json: reply });
    },
  );

  await page.goto("/-/profile");
  await page.getByRole("link", { name: "Saved replies", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/saved-replies$/);
  await selectDarkTheme(page);
  await expect(page.getByText("No saved replies yet.")).toBeVisible();
  await page.getByRole("button", { name: "New saved reply" }).click();
  const title = page.getByRole("textbox", { name: "Title", exact: true });
  const body = page.getByRole("textbox", { name: "Reply", exact: true });
  await title.fill("Triage");
  await body.fill("Thanks for the report!");
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save reply", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Triage" })).toBeVisible();

  await page.getByRole("button", { name: "Edit Triage" }).click();
  await body.fill("Thanks for %cursor%!");
  await page.getByRole("button", { name: "Save reply", exact: true }).click();
  await expect(page.getByText("Thanks for %cursor%!")).toBeVisible();

  await page.goto("/team/project/issues/new");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await editor.fill("Context\n\n/saved-replies");
  await expect(
    page.getByRole("option", { name: /Saved replies/ }),
  ).toBeVisible();
  await editor.press("Enter");
  await expect(
    page.getByRole("option", { name: "Triage", exact: true }),
  ).toBeVisible();
  await editor.press("Tab");
  await expect(editor).toHaveValue("Context\n\nThanks for !\n\n");
  await page.keyboard.insertText("the report");
  await expect(editor).toHaveValue("Context\n\nThanks for the report!\n\n");

  await page.goto("/-/settings/saved-replies");
  await page.getByRole("button", { name: "Delete Triage" }).click();
  await page.getByRole("button", { name: "Delete reply", exact: true }).click();
  await expect(page.getByText("No saved replies yet.")).toBeVisible();
  expect(writes.map(({ version }) => version)).toEqual([0, 1, 2]);
  expect(new Set(writes.map(({ id }) => id)).size).toBe(1);
});

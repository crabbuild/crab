import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("bulk unsubscribe keeps the selection across pages and only retries unconfirmed changes", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const project = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const library = {
    id: "38046d07-3fde-4422-87cc-93b492aa40ab",
    owner: "team",
    name: "library",
  };
  const issue = { kind: "issue", number: 1 };
  const pull = { kind: "pull", number: 1 };
  const selected = [
    { repository_id: project.id, subject: issue, version: 4 },
    { repository_id: project.id, subject: pull, version: 7 },
    { repository_id: library.id, subject: issue, version: 2 },
  ];
  const rows = [
    {
      repository: project,
      subject: issue,
      title: "First quiet issue",
      state: "open",
      preference: { mode: "follow", version: 4 },
      muted: false,
    },
    {
      repository: project,
      subject: pull,
      title: "Newer pull preference",
      state: "open",
      preference: { mode: "follow", version: 7 },
      muted: false,
    },
    {
      repository: library,
      subject: issue,
      title: "Other repository issue",
      state: "closed",
      preference: { mode: "follow", version: 2 },
      muted: false,
    },
  ];
  let writes = 0;
  let complete = false;

  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: { name: "Bob", subject: "bob", issuer: "https://issuer.test" },
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/unread",
    (route) => route.fulfill({ json: { unread: false } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/subscriptions",
    (route) => {
      const after = new URL(route.request().url()).searchParams.get("after");
      if (complete)
        return route.fulfill({
          json: {
            items: [
              { ...rows[1], preference: { mode: "state_changes", version: 8 } },
            ],
            next: null,
          },
        });
      if (after) {
        expect(after).toBe(project.id);
        return route.fulfill({ json: { items: rows.slice(2), next: null } });
      }
      return route.fulfill({
        json: { items: rows.slice(0, 2), next: project.id },
      });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/subscriptions/unsubscribe",
    (route) => {
      writes++;
      const request = route.request();
      expect(request.method()).toBe("POST");
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      expect(request.postDataJSON()).toEqual({
        items: writes < 3 ? selected : selected.slice(2),
      });
      if (writes === 1) return route.abort("failed");
      if (writes === 3) {
        complete = true;
        return route.fulfill({
          json: { items: [{ ...selected[2], status: "unavailable" }] },
        });
      }
      return route.fulfill({
        json: {
          items: [
            { ...selected[0], status: "unsubscribed" },
            { ...selected[1], status: "changed" },
            { ...selected[2], status: "unconfirmed" },
          ],
        },
      });
    },
  );

  await page.goto("/-/notifications/subscriptions");
  await page
    .getByRole("checkbox", { name: "Select all loaded subscriptions" })
    .click();
  await page.getByRole("button", { name: "Load more subscriptions" }).click();
  await expect(
    page.getByRole("checkbox", { name: "Select team/project issue #1" }),
  ).toBeChecked();
  await page
    .getByRole("checkbox", { name: "Select team/library issue #1" })
    .click();
  await page.getByRole("button", { name: "Unsubscribe selected" }).click();
  const dialog = page.getByRole("dialog", {
    name: "Unsubscribe from discussions",
  });
  await expect(dialog.getByText("3 selected.")).toBeVisible();
  expect(writes).toBe(0);
  await expectNoAccessibilityViolations(page);

  await dialog
    .getByRole("button", { name: "Unsubscribe", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  const retry = dialog.getByRole("button", { name: "Retry unconfirmed" });
  await retry.click();
  await expect(dialog.getByText("Kept 1 newer subscription.")).toBeVisible();
  await expect(dialog.getByText("1 result is unconfirmed.")).toBeVisible();
  await retry.click();
  await expect(
    dialog.getByText("Unsubscribed from 1 discussion."),
  ).toBeVisible();
  await expect(dialog.getByText("1 discussion is unavailable.")).toBeVisible();
  await expect(retry).toBeHidden();

  await dialog.getByRole("button", { name: "Close" }).click();
  await expect(
    page.getByRole("link", { name: "First quiet issue" }),
  ).toBeHidden();
  await expect(
    page.getByRole("link", { name: "Other repository issue" }),
  ).toBeHidden();
  await expect(
    page.getByRole("link", { name: "Newer pull preference" }),
  ).toBeVisible();
  await expect(page.getByText(/State changes only/)).toBeVisible();
  await expect(
    page.getByRole("group", { name: "Discussion subscriptions" }),
  ).toBeFocused();
});

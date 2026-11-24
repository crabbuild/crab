import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("saved filters combine criteria and recover from a stale edit on mobile", async ({
  page,
}) => {
  await page.setViewportSize({ width: 360, height: 780 });
  await page.emulateMedia({ colorScheme: "dark" });
  const { lifecycle } = await mockPullRequest(page);
  const organization = "00000000-0000-4000-8000-000000000075";
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route("**/api/repos", (route) =>
    route.fulfill({
      json: {
        repositories: [
          {
            id: lifecycle.repositoryId,
            owner: "team",
            name: "project",
            organization_id: organization,
          },
        ],
      },
    }),
  );
  await page.route("**/api/organizations", (route) =>
    route.fulfill({
      json: {
        items: [{ organization: { id: organization, name: "Platform" } }],
      },
    }),
  );
  const criteria = {
    view: "read",
    organization,
    reason: "review_requested",
    kind: "pull",
    author: "me",
  };
  let revision = 0;
  let saved:
    | { id: string; name: string; criteria: typeof criteria }
    | undefined;
  await page.route("**/api/notifications/filters", (route) => {
    if (route.request().method() === "PUT") {
      const { change, revision: expected } = route.request().postDataJSON();
      expect(route.request().headers()["x-csrf-token"]).toBe("fixture-csrf");
      if (expected !== revision)
        return route.fulfill({
          status: 409,
          json: { error: { code: "conflict", message: "The record changed." } },
        });
      expect(change.kind).toBe("save");
      expect(change.filter.criteria).toEqual(criteria);
      saved = change.filter;
      revision += 1;
    }
    return route.fulfill({ json: { revision, items: saved ? [saved] : [] } });
  });
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: false } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => route.fulfill({ json: { items: [], next: null } }),
  );

  await page.goto("/-/notifications");
  await page.getByRole("button", { name: "Manage filters" }).click();
  const dialog = page.getByRole("dialog", {
    name: "Saved notification filters",
  });
  await dialog.getByRole("button", { name: "New filter" }).click();
  await expect(dialog.getByLabel("Filter name")).toBeFocused();
  await dialog.getByLabel("Filter name").fill("Platform PR reviews");
  await dialog.getByLabel("View", { exact: true }).selectOption("read");
  await dialog
    .getByLabel("Organization", { exact: true })
    .selectOption({ label: "Platform" });
  await dialog
    .getByLabel("Reason", { exact: true })
    .selectOption("review_requested");
  await dialog.getByLabel("Notification type").selectOption("pull");
  await dialog.getByRole("button", { name: "Me", exact: true }).click();
  await expectNoAccessibilityViolations(page);
  const filteredQuery = page.waitForRequest((request) => {
    const { pathname, searchParams } = new URL(request.url());
    return (
      pathname === "/api/notifications" &&
      searchParams.get("organization") === organization &&
      searchParams.get("read") === "true" &&
      searchParams.get("kind") === "pull" &&
      searchParams.get("author") === "me" &&
      searchParams.get("reason") === "review_requested"
    );
  });
  await dialog
    .getByRole("button", { name: "Save filter", exact: true })
    .click();
  await filteredQuery;
  await expect(dialog).toHaveCount(0);
  await expect(page.getByLabel("Saved filter", { exact: true })).toHaveValue(
    saved!.id,
  );

  await page.getByRole("button", { name: "Manage filters" }).click();
  await dialog
    .getByRole("button", { name: "Edit Platform PR reviews" })
    .click();
  saved = { ...saved!, name: "Shared review queue" };
  revision += 1;
  await dialog.getByLabel("Filter name").fill("Local edit");
  await dialog
    .getByRole("button", { name: "Save filter", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toHaveText("The record changed.");
  await dialog.getByRole("button", { name: "Reload filters" }).click();
  await expect(
    dialog.getByRole("button", { name: "Edit Shared review queue" }),
  ).toBeVisible();
});

import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("subscriptions show quiet follows across pages and save changes using the repository ID", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const subject = { kind: "pull", number: 1 };
  let preference = { mode: "follow", version: 4 };
  let lost = true;
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
    (url) => url.pathname === "/api/notifications",
    (route) => route.fulfill({ json: { items: [], next: null } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/unread",
    (route) => route.fulfill({ json: { unread: false } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/subscriptions",
    (route) => {
      const url = new URL(route.request().url());
      const kind = url.searchParams.get("kind");
      const after = url.searchParams.get("after");
      if (!kind && !after)
        return route.fulfill({
          json: { items: [], next: `${repository.id}:issue:32` },
        });
      if (after) expect(after).toBe(`${repository.id}:issue:32`);
      if (kind) expect(kind).toBe("pull");
      return route.fulfill({
        json: {
          items:
            preference.mode === "unsubscribed"
              ? []
              : [
                  {
                    repository,
                    subject,
                    title: "A quiet pull",
                    state: "open",
                    preference,
                    muted: true,
                  },
                ],
          next: null,
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/subscription",
    (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(repository.id);
      if (request.method() === "GET")
        return route.fulfill({ json: preference });
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const input = request.postDataJSON();
      if (input.mode === "state_changes") {
        expect(input).toEqual({ mode: "state_changes", version: 4 });
        preference = { mode: "state_changes", version: 5 };
        if (lost) {
          lost = false;
          return route.abort("failed");
        }
      } else {
        expect(input).toEqual({ mode: "unsubscribed", version: 5 });
        preference = { mode: "unsubscribed", version: 6 };
      }
      return route.fulfill({ json: preference });
    },
  );

  await page.goto("/-/notifications");
  await page.getByRole("link", { name: "Subscriptions", exact: true }).click();
  await expect(page.getByText("No subscriptions found so far.")).toBeVisible();
  await page.getByRole("button", { name: "Load more subscriptions" }).click();
  const link = page.getByRole("link", { name: "A quiet pull" });
  await expect(link).toHaveAttribute(
    "href",
    `/team/project/pull/1?repository_id=${repository.id}`,
  );
  await expect(page.getByText(/Repository muted/)).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.getByLabel("Discussion type").selectOption("pull");
  await expect(page).toHaveURL(/kind=pull$/);
  await expect(link).toBeVisible();
  const settings = page.getByRole("button", {
    name: "Notification settings for team/project pull request #1",
  });
  const dialog = page.getByRole("dialog", {
    name: "Pull request notifications",
  });
  const choice = dialog.getByLabel("Notify me about");
  const save = dialog.getByRole("button", { name: "Save preference" });
  await settings.click();
  await choice.selectOption("state_changes");
  await save.click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  await save.click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText(/State changes only/)).toBeVisible();
  await settings.click();
  await choice.selectOption("unsubscribed");
  await save.click();
  await expect(link).toBeHidden();
  await expect(
    page.getByRole("group", { name: "Discussion subscriptions" }),
  ).toBeFocused();
  await expect(
    page.getByText("No discussion subscriptions match this view."),
  ).toBeVisible();
});

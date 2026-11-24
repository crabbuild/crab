import { chooseSelectOption } from "./select";
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
  await chooseSelectOption(page.getByLabel("Discussion type"), "pull");
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
  await chooseSelectOption(choice, "state_changes");
  await save.click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  await save.click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText(/State changes only/)).toBeVisible();
  await settings.click();
  await chooseSelectOption(choice, "unsubscribed");
  await save.click();
  await expect(link).toBeHidden();
  await expect(
    page.getByRole("group", { name: "Discussion subscriptions" }),
  ).toBeFocused();
  await expect(
    page.getByText("No discussion subscriptions match this view."),
  ).toBeVisible();
});

test("subscriptions filter by reason and keep the requested time order across pages", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const make = (
    number: number,
    title: string,
    origin: { reason: string; subscribed_at_ms: number } | null,
  ) => ({
    repository,
    subject: { kind: "issue", number },
    title,
    state: "open",
    muted: false,
    preference: { mode: "follow", version: 1 },
    origin,
  });
  const first = make(1, "Mentioned follow", {
    reason: "mention",
    subscribed_at_ms: 1_700_000_000_000,
  });
  const second = make(2, "Manual follow", {
    reason: "manual",
    subscribed_at_ms: 1_800_000_000_000,
  });
  const older = make(3, "Earlier follow", null);
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
      const { searchParams } = new URL(route.request().url());
      const reason = searchParams.get("reason");
      const sort = searchParams.get("sort");
      const after = searchParams.get("after");
      if (reason === "mention") {
        expect(sort).toBe("oldest");
        expect(after).toBeNull();
        return route.fulfill({ json: { items: [first], next: null } });
      }
      if (sort === "recent" && !after)
        return route.fulfill({
          json: { items: [second, first], next: "undated" },
        });
      if (sort === "recent" && after === "undated")
        return route.fulfill({ json: { items: [older], next: null } });
      return route.fulfill({
        json: { items: [first, second, older], next: null },
      });
    },
  );
  await page.goto("/-/notifications/subscriptions");
  const sort = page.getByLabel("Sort subscriptions");
  const reason = page.getByLabel("Subscription reason");
  const list = page.getByRole("group", { name: "Discussion subscriptions" });
  const titles = () => list.getByRole("link");
  await chooseSelectOption(sort, "recent");
  await expect(titles()).toHaveText(["Manual follow", "Mentioned follow"]);
  await expect(list.getByText("Manually subscribed")).toBeVisible();
  await page.getByRole("button", { name: "Load more subscriptions" }).click();
  await expect(titles()).toHaveText([
    "Manual follow",
    "Mentioned follow",
    "Earlier follow",
  ]);
  await expect(list.getByText("Source unavailable")).toBeVisible();
  await chooseSelectOption(sort, "oldest");
  await expect(titles()).toHaveText([
    "Mentioned follow",
    "Manual follow",
    "Earlier follow",
  ]);
  await chooseSelectOption(reason, "mention");
  await expect(titles()).toHaveText(["Mentioned follow"]);
  await expect(page).toHaveURL(/sort=oldest&reason=mention$/);
  await expectNoAccessibilityViolations(page);
  await page.reload();
  await expect(reason).toHaveText("Mention");
  await expect(sort).toHaveText("Least recently subscribed");
  await expect(titles()).toHaveText(["Mentioned follow"]);
});

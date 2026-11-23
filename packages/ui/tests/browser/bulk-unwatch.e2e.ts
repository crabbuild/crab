import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("unwatch all previews an owner's full selection and retries the same versions after a lost reply", async ({
  page,
}) => {
  await mockPullRequest(page);
  const watches = Array.from({ length: 52 }, (_, index) => ({
    repository: {
      id: `e1255599-7563-45f3-af57-${String(index).padStart(12, "0")}`,
      owner: "team",
      name: `repo-${String(index).padStart(2, "0")}`,
    },
    preference: { mode: "issues", version: 7, releases: true },
  }));
  const other = {
    repository: {
      id: "97bcc993-c8be-4393-ac18-82b80c0b6dff",
      owner: "another",
      name: "repo",
    },
    preference: { mode: "direct", version: 1, releases: true },
  };
  let submissions = 0;
  let applied = false;
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
    (url) => url.pathname === "/api/notifications/watched",
    (route) => {
      const { searchParams } = new URL(route.request().url());
      if (!searchParams.has("owner"))
        return route.fulfill({
          json: {
            items: applied ? [other, watches[51]] : [other, watches[0]],
            next: null,
          },
        });
      expect(searchParams.get("owner")).toBe("team");
      expect(searchParams.get("limit")).toBe("50");
      const after = searchParams.get("after");
      if (after === "team/repo-50")
        return route.fulfill({
          json: { items: watches.slice(50), next: null },
        });
      if (after === "team/repo-49")
        return route.fulfill({ json: { items: [], next: "team/repo-50" } });
      expect(after).toBeNull();
      return route.fulfill({
        json: { items: watches.slice(0, 50), next: "team/repo-49" },
      });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/notifications/watched/unwatch",
    (route) => {
      submissions++;
      const request = route.request();
      expect(request.method()).toBe("POST");
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const last = submissions === 3;
      const expected = last ? watches.slice(50) : watches.slice(0, 50);
      const items = expected.map(({ repository }) => ({
        id: repository.id,
        version: 7,
      }));
      expect(request.postDataJSON()).toEqual({ owner: "team", items });
      if (submissions === 1) return route.abort("failed");
      if (last) applied = true;
      return route.fulfill({
        json: {
          items: items.map(({ id }, index) => ({
            id,
            status: last
              ? index === 0
                ? "unavailable"
                : "changed"
              : "unwatched",
          })),
        },
      });
    },
  );

  await page.goto("/-/notifications/watched");
  await page.getByRole("button", { name: "Unwatch all" }).click();
  const dialog = page.getByRole("dialog", {
    name: "Unwatch repositories by owner",
  });
  await dialog.getByLabel("Repository owner").selectOption("team");
  await expect(
    dialog.getByText("52 watched repositories owned by team."),
  ).toBeVisible();
  expect(submissions).toBe(0);
  await expectNoAccessibilityViolations(page);
  await dialog.getByRole("button", { name: "Unwatch 52 repositories" }).click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  await dialog.getByRole("button", { name: "Retry unconfirmed" }).click();
  await expect(
    dialog.getByText("Stopped watching 50 repositories."),
  ).toBeVisible();
  await expect(dialog.getByText("Kept 1 newer watch setting.")).toBeVisible();
  await expect(dialog.getByText("1 repository is unavailable.")).toBeVisible();
  await dialog.getByRole("button", { name: "Close" }).click();
  await expect(page.getByRole("link", { name: "team/repo-00" })).toBeHidden();
  await expect(page.getByRole("link", { name: "team/repo-51" })).toBeVisible();
  await expect(page.getByRole("link", { name: "another/repo" })).toBeVisible();
});

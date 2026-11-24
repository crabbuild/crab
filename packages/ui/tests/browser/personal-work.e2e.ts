import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("personal work filters, pages, and recovers a changed access scope on mobile", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const id = "00000000-0000-4000-8000-00000000000a";
  await page.route(
    (url) => url.pathname === "/api/session",
    (route) =>
      route.fulfill({
        json: {
          authenticated: true,
          mode: "local",
          user: null,
          csrf: "fixture",
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/personal-work",
    (route) => {
      const params = new URL(route.request().url()).searchParams;
      if (params.get("after")) {
        return route.fulfill({
          status: 409,
          json: {
            error: { code: "page_changed", message: "This list changed" },
          },
        });
      }
      const kind = params.get("kind");
      const relation = params.get("relation");
      return route.fulfill({
        json: {
          items: [
            {
              repository: { id, owner: "team", name: "service" },
              kind,
              title:
                kind === "issue"
                  ? "Update the runbook"
                  : relation === "review_requested"
                    ? "Review the deployment"
                    : "Deploy the service",
              number: 7,
              draft: false,
              state: "open",
              created_at: 1_700_000_000_000,
              updated_at: 1_700_000_000_000,
            },
          ],
          next: "fixture-cursor",
        },
      });
    },
  );

  await page.goto("/-/work");
  await expect(page.getByRole("heading", { name: "My work" })).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Deploy the service" }),
  ).toHaveAttribute("href", `/team/service/pull/7?repository_id=${id}`);
  await page.getByRole("link", { name: "Review requests" }).click();
  await expect(page).toHaveURL(/relation=review_requested/);
  await expect(
    page.getByRole("link", { name: "Review the deployment" }),
  ).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Work state" }),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Next page" }).click();
  await expect(page.getByRole("alert")).toContainText("This list changed");
  await page.getByRole("link", { name: "First page" }).click();
  await expect(
    page.getByRole("link", { name: "Review the deployment" }),
  ).toBeVisible();
  await page.getByRole("link", { name: "Issues", exact: true }).click();
  await expect(page).toHaveURL(/kind=issue&relation=authored/);
  await expect(
    page.getByRole("link", { name: "Update the runbook" }),
  ).toHaveAttribute("href", `/team/service/issues/7?repository_id=${id}`);
  await expectNoAccessibilityViolations(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});

test("workspace search shares its query, sort, and scope across pages", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const id = "00000000-0000-4000-8000-00000000000a";
  await page.route(
    (url) => url.pathname === "/api/session",
    (route) =>
      route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/discussion-search",
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const issue = {
        repository: { id, owner: "team", name: "service" },
        kind: "issue",
        title: "Release checklist",
        number: 1,
        draft: false,
        state: "open",
        created_at: 1_700_000_000_000,
        updated_at: 1_710_000_000_000,
      };
      const later = query.has("after");
      return route.fulfill({
        json: {
          items: later
            ? [
                {
                  ...issue,
                  kind: "pull",
                  title: "Release script",
                  draft: true,
                },
              ]
            : [issue],
          next: later ? null : "next-release-page",
        },
      });
    },
  );
  await page.goto("/-/discussion-search");
  await page.getByRole("searchbox").fill("release is:open");
  await page.getByLabel("Repository scope").fill("team/service");
  await page.getByLabel("Sort").selectOption("updated");
  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(
    /q=release\+is%3Aopen&scope=team%2Fservice&sort=updated/,
  );
  await expect(
    page.getByRole("link", { name: "Release checklist" }),
  ).toHaveAttribute("href", `/team/service/issues/1?repository_id=${id}`);
  await expect(
    page.getByText("Open · Updated", { exact: false }),
  ).toBeVisible();
  await page.getByRole("link", { name: "Next page" }).click();
  await expect(
    page.getByRole("link", { name: "Release script" }),
  ).toHaveAttribute("href", `/team/service/pull/1?repository_id=${id}`);
  await page.reload();
  await expect(page.getByRole("searchbox")).toHaveValue("release is:open");
  await expect(page.getByLabel("Repository scope")).toHaveValue("team/service");
  await expect(
    page.getByRole("link", { name: "Release script" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});

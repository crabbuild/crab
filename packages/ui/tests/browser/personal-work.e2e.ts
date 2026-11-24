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
  await page.getByRole("link", { name: "Issues" }).click();
  await expect(page).toHaveURL(/kind=issue&relation=authored/);
  await expect(
    page.getByRole("link", { name: "Update the runbook" }),
  ).toHaveAttribute("href", `/team/service/issues/7?repository_id=${id}`);
  await expectNoAccessibilityViolations(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});

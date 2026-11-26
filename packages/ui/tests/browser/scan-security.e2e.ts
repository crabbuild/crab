import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";
import { chooseSelectOption } from "./select";

test("repository Security recovers a stale history page on mobile", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await mockPullRequest(page);
  await page.route(
    (url) => url.pathname.endsWith("/code-scanning/history"),
    (route) => {
      const params = new URL(route.request().url()).searchParams;
      if (params.has("cursor")) {
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "page_changed",
              message: "Start at the latest commits.",
            },
          },
        });
      }
      const branch = params.get("branch") ?? "refs/heads/main";
      return route.fulfill({
        json: {
          branch,
          tip: (branch === "refs/heads/main" ? "a" : "b").repeat(40),
          tip_reports: 0,
          commits_examined: 30,
          items: [],
          next: "next-history-page",
        },
      });
    },
  );

  await page.goto("/team/project/security");
  await expect(
    page.getByRole("heading", { name: "Code scanning" }),
  ).toBeVisible();
  await expect(
    page.getByText("No scan reports in the 30 commits on this page."),
  ).toBeVisible();
  await page.getByRole("link", { name: "Older commits" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Start at the latest commits.",
  );
  await page.getByRole("link", { name: "Latest commits" }).click();
  await expect(page).not.toHaveURL(/scan_page/);
  await expect(page.getByRole("link", { name: "aaaaaaa" })).toBeVisible();
  await chooseSelectOption(
    page.getByLabel("Branch"),
    "refs/heads/feature/docs",
  );
  await expect(page.getByRole("link", { name: "bbbbbbb" })).toBeVisible();
  await expectNoAccessibilityViolations(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});

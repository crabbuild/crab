import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import { emptyPullRollup } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  await mockPullRequest(page);
  const rows = Array.from({ length: 4 }, (_, index) => ({
    number: index + 1,
    kind: "pull",
    title: [
      "Passing validation",
      "Changes need attention",
      "Validation is running",
      "Deleted source branch",
    ][index],
    state: "open",
    draft: index === 2,
    version: 1,
    author: { name: "Alice", profile: null },
    created_at: 1700000000000,
    updated_at: 1700000000000,
    labels: [],
    assignees: [],
    base_ref: "refs/heads/main",
    head_ref: "refs/heads/feature",
    comment_count: 1,
    reaction_count: 0,
    rollup: emptyPullRollup(index + 1, index === 3 ? null : "b".repeat(40)),
  }));
  rows[0].rollup.checks = {
    success: 4,
    neutral: 1,
    skipped: 1,
    pending: 0,
    failed: 0,
  };
  rows[0].rollup.reviews = {
    approved: 2,
    changes_requested: 0,
    requested: 0,
    outdated: 0,
  };
  rows[1].rollup.checks = {
    success: 1,
    neutral: 0,
    skipped: 0,
    pending: 1,
    failed: 2,
  };
  rows[1].rollup.reviews = {
    approved: 1,
    changes_requested: 1,
    requested: 0,
    outdated: 0,
  };
  rows[2].rollup.checks = {
    success: 0,
    neutral: 0,
    skipped: 0,
    pending: 2,
    failed: 0,
  };
  rows[2].rollup.reviews = {
    approved: 0,
    changes_requested: 0,
    requested: 2,
    outdated: 0,
  };
  const model = { rows, failed: false, reads: [] as string[] };
  await page.route(
    (url) => /^\/api\/repos\/team\/project\/(issues|pulls)$/.test(url.pathname),
    (route) => {
      model.reads.push(route.request().url());
      if (model.failed)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Unable to verify source refs" } },
        });
      return route.fulfill({
        json: {
          repository_id: "00000000-0000-4000-8000-000000000099",
          can_bulk_edit: true,
          items: model.rows,
          next: null,
          counts: { open: 4, closed: 0, merged: 0 },
          matching: 4,
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          navigation: {
            kind: "state",
            selected: "open",
            open: "is:open",
            closed: "is:closed",
          },
        },
      });
    },
  );
  return model;
}

for (const view of ["issues", "pulls"])
  test(`${view} rows show check and review counts for the source commit and open the Checks tab`, async ({
    page,
  }) => {
    const model = await fixture(page);
    await page.goto(`/team/project/${view}?q=is%3Apr+is%3Aopen&powerbar=1`);
    const rows = page.locator(".issue-list li");
    await expect(rows).toHaveCount(4);
    const check = rows.nth(0).getByRole("link", {
      name: "6 of 6 checks completed without failure on pull request #1",
    });
    await check.focus();
    await expect(check).toBeFocused();
    await expect(page.getByRole("tooltip")).toContainText(
      "4 successful, 1 neutral, 1 skipped, 0 pending, 0 failed",
    );
    await expect(page.getByRole("tooltip")).toContainText(
      "Source commit " + "b".repeat(40),
    );
    await expect(
      rows.nth(0).getByRole("link", { name: "Approved on pull request #1" }),
    ).toBeVisible();
    await expect(
      rows.nth(1).getByRole("link", {
        name: "1 of 4 checks completed without failure on pull request #2",
      }),
    ).toBeVisible();
    await expect(
      rows
        .nth(1)
        .getByRole("link", { name: "Changes requested on pull request #2" }),
    ).toBeVisible();
    await expect(
      rows.nth(2).getByRole("link", {
        name: "0 of 2 checks completed without failure on pull request #3",
      }),
    ).toBeVisible();
    await expect(
      rows
        .nth(2)
        .getByRole("link", { name: "Review requested on pull request #3" }),
    ).toBeVisible();
    await expect(rows.nth(3)).toContainText("Source branch unavailable");
    await expect(
      rows.nth(3).getByRole("link", {
        name: /checks completed without failure|Approved/,
      }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("group", {
        name:
          view === "issues"
            ? "Issues request widget"
            : "Pull requests request widget",
        exact: true,
      }),
    ).toContainText("6/6");
    expect(model.reads).toHaveLength(1);
    await check.focus();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/pull\/1\/checks\?powerbar=1$/);
    await expect(
      page.getByRole("region", { name: "Checks", exact: true }),
    ).toBeVisible();
  });

test("head advances invalidate old approvals and failures withdraw the list instead of preserving green badges", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/pulls?q=is%3Aopen");
  await expect(
    page.getByRole("link", { name: "Approved on pull request #1" }),
  ).toBeVisible();
  model.rows[0].rollup = emptyPullRollup(1, "c".repeat(40));
  model.rows[0].rollup.reviews!.outdated = 2;
  await page.reload();
  await expect(
    page.getByRole("link", { name: "Reviews outdated on pull request #1" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Approved on pull request #1" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", {
      name: "6 of 6 checks completed without failure on pull request #1",
    }),
  ).toHaveCount(0);
  model.failed = true;
  await page.reload();
  await expect(page.getByRole("alert")).toContainText(
    "Unable to verify source refs",
  );
  await expect(page.locator(".issue-list li")).toHaveCount(0);
  model.failed = false;
  await page.getByRole("button", { name: "Try again", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "Reviews outdated on pull request #1" }),
  ).toBeVisible();
  expect(new URL(page.url()).searchParams.get("q")).toBe("is:open");
});

test("compact rows, check tooltips and review summaries fit mobile in both themes", async ({
  page,
}) => {
  await fixture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/pulls");
  await expect(page.locator(".issue-list li")).toHaveCount(4);
  await page.getByRole("radio", { name: "Compact", exact: true }).click();
  await expect(page.locator(".issue-list")).toHaveAttribute(
    "data-density",
    "compact",
  );
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  const review = page.getByRole("link", {
    name: "Changes requested on pull request #2",
  });
  await review.focus();
  await expect(page.getByRole("tooltip")).toContainText(
    "1 approvals, 1 changes requested",
  );
  await page.keyboard.press("Escape");
  await expect(review).toBeFocused();
  await expect(
    page.getByRole("group", {
      name: "Pull requests request widget",
      exact: true,
    }),
  ).toHaveCount(0);
});

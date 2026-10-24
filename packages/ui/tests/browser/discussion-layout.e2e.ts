import { emptyPullRollup } from "./fixtures";
import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
  contrastRatio,
} from "./accessibility";

async function fixture(page: Page) {
  await mockPullRequest(page);
  const reads: string[] = [];
  const shared = {
    number: 1,
    title: "Keep navigation predictable",
    state: "open",
    author: { name: "Alice", profile: null },
    created_at: 1700000000000,
    comment_count: 3,
    reaction_count: 0,
    labels: [{ id: 1, name: "Help wanted", color: "008672" }],
    assignees: [
      {
        id: "00000000-0000-4000-8000-000000000001",
        name: "Alice",
        profile: null,
      },
    ],
  };
  for (const kind of ["issues", "pulls"]) {
    await page.route(`**/api/repos/team/project/${kind}?**`, (route) => {
      reads.push(route.request().url());
      const issue = { ...shared, kind: "issue" };
      const pull = {
        ...shared,
        kind: "pull",
        rollup: emptyPullRollup(),
        title: "Review navigation",
        draft: true,
        base_ref: "refs/heads/main",
        head_ref: "refs/heads/feature/navigation",
      };
      return route.fulfill({
        json: {
          items: kind === "issues" ? [issue, pull] : [pull],
          next: null,
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          matching: kind === "issues" ? 2 : 1,
          counts: { open: 2, closed: 0, merged: 0 },
          navigation: {
            kind: "state",
            selected: "open",
            open: "is:open",
            closed: "is:closed",
          },
        },
      });
    });
  }
  return reads;
}

test("link buttons retain the same readable control colors and geometry as native buttons", async ({
  page,
}) => {
  await fixture(page);
  await page.goto("/team/project/pulls");
  const labels = page
    .locator(".section-heading")
    .getByRole("link", { name: "Labels", exact: true });
  const create = page.getByRole("button", {
    name: "New pull request",
    exact: true,
  });
  for (const theme of ["Light", "Dark"]) {
    await page.getByRole("button", { name: theme, exact: true }).click();
    const colors = await labels.evaluate((node) => ({
      foreground: getComputedStyle(node).color,
      background: getComputedStyle(node).backgroundColor,
      body: getComputedStyle(document.body).color,
    }));
    expect(colors.foreground).toBe(colors.body);
    expect(
      contrastRatio(colors.foreground, colors.background),
    ).toBeGreaterThanOrEqual(4.5);
    await labels.hover();
    await expect(labels).toHaveCSS("text-decoration-line", "none");
    expect((await labels.boundingBox())!.height).toBe(
      (await create.boundingBox())!.height,
    );
    await expectNoAccessibilityViolations(page);
  }
});

test("density changes both discussion kinds without fetching, rewriting the query or losing metadata", async ({
  page,
}) => {
  const reads = await fixture(page);
  await page.goto(
    "/team/project/issues?q=is%3Aopen&sort=comments&direction=asc&powerbar=1",
  );
  const row = page.locator(".issue-list li").first();
  await expect(row).toContainText("Keep navigation predictable");
  const comfortable = (await row.boundingBox())!.height;
  const url = page.url();
  const before = reads.length;
  const group = page.getByRole("radiogroup", { name: "List density" });
  await group.getByRole("radio", { name: "Comfortable", exact: true }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    group.getByRole("radio", { name: "Compact", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Space");
  await expect(
    group.getByRole("radio", { name: "Compact", exact: true }),
  ).toHaveAttribute("aria-checked", "true");
  const selectedColor = await group
    .getByRole("radio", { name: "Compact", exact: true })
    .evaluate((element) => getComputedStyle(element).backgroundColor);
  const unselectedColor = await group
    .getByRole("radio", { name: "Comfortable", exact: true })
    .evaluate((element) => getComputedStyle(element).backgroundColor);
  expect(selectedColor).not.toBe(unselectedColor);
  await expect(page.locator(".issue-list")).toHaveAttribute(
    "data-density",
    "compact",
  );
  expect((await row.boundingBox())!.height).toBeLessThan(comfortable - 12);
  expect(page.url()).toBe(url);
  expect(reads.length).toBe(before);
  await expect(row.getByText("Help wanted", { exact: true })).toBeVisible();
  await expect(
    row.getByRole("img", { name: "Assigned to Alice" }),
  ).toBeVisible();
  await expect(row.getByRole("link", { name: "3 comments" })).toBeVisible();
  await expect(row).toContainText("by Alice");
  await expect(page.locator(".issue-list li").nth(1)).toContainText(
    "feature/navigation",
  );
  await page.reload();
  await expect(page.locator(".issue-list")).toHaveAttribute(
    "data-density",
    "compact",
  );
  await page
    .locator(".repo-header")
    .getByRole("link", { name: "Pull requests", exact: true })
    .click();
  await expect(page.locator(".issue-list")).toHaveAttribute(
    "data-density",
    "compact",
  );
  await expect(
    page.getByRole("complementary", { name: "Pull request navigation" }),
  ).toBeVisible();
  await page.getByRole("radio", { name: "Comfortable", exact: true }).click();
  await expect(page.locator(".issue-list")).toHaveAttribute(
    "data-density",
    "comfortable",
  );
});

test("workspace collapse preserves named icon navigation, focus and reload across issue and PR lists", async ({
  page,
}) => {
  await fixture(page);
  await page.goto("/team/project/issues?powerbar=1");
  const workspace = page.locator(".discussion-workspace");
  const aside = page.getByRole("complementary", { name: "Issue navigation" });
  const width = (await aside.boundingBox())!.width;
  const button = page.getByRole("button", {
    name: "Collapse sidebar",
    exact: true,
  });
  const bounds = (await button.boundingBox())!;
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(
    page.viewportSize()!.height,
  );
  await button.focus();
  await page.keyboard.press("Enter");
  const expand = page.getByRole("button", {
    name: "Expand sidebar",
    exact: true,
  });
  await expect(expand).toBeFocused();
  await expect(expand).toHaveAttribute("aria-expanded", "false");
  await expect(workspace).toHaveAttribute("data-sidebar", "collapsed");
  expect((await aside.boundingBox())!.width).toBeLessThan(width / 2);
  const milestones = aside.getByRole("link", {
    name: "Milestones",
    exact: true,
  });
  await expect(milestones).toBeVisible();
  await milestones.hover();
  await expect(
    page.getByRole("tooltip", { name: "Milestones", exact: true }),
  ).toBeVisible();
  await milestones.click();
  await expect(page).toHaveURL(/milestones\?powerbar=1$/);
  await expect(workspace).toHaveAttribute("data-sidebar", "collapsed");
  await page.reload();
  await expect(workspace).toHaveAttribute("data-sidebar", "collapsed");
  await page
    .locator(".repo-header")
    .getByRole("link", { name: "Pull requests", exact: true })
    .click();
  const pulls = page.getByRole("complementary", {
    name: "Pull request navigation",
  });
  await expect(
    pulls.getByRole("link", { name: "Pull requests", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(workspace).toHaveAttribute("data-sidebar", "collapsed");
  await expand.click();
  await expect(workspace).toHaveAttribute("data-sidebar", "expanded");
  await expect(
    page.getByRole("button", { name: "Collapse sidebar", exact: true }),
  ).toBeFocused();
  await page
    .locator(".issue-list")
    .getByRole("link", { name: "Review navigation", exact: true })
    .click();
  await expect(
    page.getByRole("complementary", { name: "Pull request navigation" }),
  ).toHaveCount(0);
});

for (const view of ["issues", "pulls"] as const) {
  test(`${view} compact and collapsed layouts stay accessible on mobile and when browser storage is denied`, async ({
    page,
  }) => {
    await page.addInitScript(() => {
      const original = Storage.prototype.getItem;
      Storage.prototype.getItem = function (key) {
        if (key.startsWith("crab-discussion-"))
          throw new DOMException("Denied", "SecurityError");
        return original.call(this, key);
      };
      const write = Storage.prototype.setItem;
      Storage.prototype.setItem = function (key, value) {
        if (key.startsWith("crab-discussion-"))
          throw new DOMException("Denied", "QuotaExceededError");
        return write.call(this, key, value);
      };
    });
    await fixture(page);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/team/project/${view}`);
    await selectDarkTheme(page);
    await page.getByRole("radio", { name: "Compact", exact: true }).click();
    await page
      .getByRole("button", { name: "Collapse sidebar", exact: true })
      .click();
    await expect(page.locator(".issue-list")).toHaveAttribute(
      "data-density",
      "compact",
    );
    await expect(page.locator(".discussion-workspace")).toHaveAttribute(
      "data-sidebar",
      "collapsed",
    );
    await page.mouse.move(0, 0);
    await page.keyboard.press("Tab");
    await page.keyboard.press("Escape");
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await page
      .getByRole("button", { name: "Expand sidebar", exact: true })
      .click();
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await page.mouse.move(0, 0);
    await page.keyboard.press("Tab");
    await page.keyboard.press("Escape");
    await expectNoAccessibilityViolations(page);
    await expect(
      page.getByRole("link", {
        name:
          view === "issues"
            ? "Keep navigation predictable"
            : "Review navigation",
        exact: true,
      }),
    ).toBeVisible();
  });
}

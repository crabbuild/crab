import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import { emptyPullRollup } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

for (const view of ["issues", "pulls"] as const) {
  test(`${view} rows show lifecycle dates, update relative time, and expose exact time on focus`, async ({
    page,
  }) => {
    const now = new Date("2026-10-22T18:00:00Z").getTime();
    await page.clock.install({ time: now });
    await mockPullRequest(page);
    const items = ["open", "closed", "merged", "closed"].map(
      (state, index) => ({
        kind: index === 1 && view === "issues" ? "issue" : "pull",
        number: index + 1,
        title: [
          "Draft proposal",
          "Closed report",
          "Merged feature",
          "Historical closure",
        ][index],
        state,
        state_reason: state === "closed" ? "not_planned" : null,
        draft: index === 0,
        version: 1,
        author: { name: "Alice", profile: null },
        created_at: now - 120000,
        closed_at: index === 3 ? null : state === "open" ? null : now - 600000,
        merged_at: state === "merged" ? now - 600000 : null,
        // A later metadata edit must never become the closure time.
        updated_at: now,
        labels: [],
        assignees: [],
        comment_count: 0,
        reaction_count: 0,
        base_ref: "refs/heads/main",
        head_ref: "refs/heads/feature",
        rollup: emptyPullRollup(index + 1, "b".repeat(40)),
      }),
    );
    await page.route(
      (url) => url.pathname === `/api/repos/team/project/${view}`,
      (route) =>
        route.fulfill({
          json: {
            items,
            next: null,
            counts: { open: 1, closed: 2, merged: 1 },
          },
        }),
    );
    await page.goto(`/team/project/${view}`);
    const rows = page.locator(".issue-list li");
    await expect(rows).toHaveCount(4);
    await expect(rows.nth(0)).toContainText(
      "#1 opened 2 minutes ago by Alice · Draft",
    );
    await expect(rows.nth(1)).toContainText(
      "#2 by Alice was closed 10 minutes ago",
    );
    await expect(rows.nth(2)).toContainText(
      "#3 by Alice was merged 10 minutes ago",
    );
    await expect(rows.nth(3)).toContainText(
      "#4 by Alice was closed at an unknown time",
    );
    await expect(rows.nth(3).locator("relative-time")).toHaveCount(0);
    const time = rows.nth(1).locator("relative-time");
    await expect(time).toHaveAttribute(
      "datetime",
      new Date(now - 600000).toISOString(),
    );
    await time.focus();
    await expect(page.getByRole("tooltip")).toContainText("Oct 22, 2026");
    await page.keyboard.press("Escape");
    await time.blur();
    await page.clock.fastForward(120000);
    await expect(rows.nth(0)).toContainText("4 minutes ago");
    await page.setViewportSize({ width: 390, height: 844 });
    await selectDarkTheme(page);
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  });
}

test("old discussion dates use calendar dates and change when a row is reopened", async ({
  page,
}) => {
  await page.clock.install({ time: new Date("2026-10-22T18:00:00Z") });
  await mockPullRequest(page);
  const item = {
    kind: "pull",
    number: 1,
    title: "Old proposal",
    state: "closed",
    draft: false,
    version: 1,
    author: { name: "Alice", profile: null },
    created_at: Date.parse("2026-02-10T12:00:00Z"),
    closed_at: Date.parse("2026-03-12T12:00:00Z"),
    merged_at: null,
    updated_at: Date.now(),
    labels: [],
    assignees: [],
    comment_count: 0,
    reaction_count: 0,
    base_ref: "refs/heads/main",
    head_ref: "refs/heads/feature",
    rollup: emptyPullRollup(1, "b".repeat(40)),
  };
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls",
    (route) =>
      route.fulfill({
        json: {
          items: [item],
          next: null,
          counts: { open: 0, closed: 1, merged: 0 },
        },
      }),
  );
  await page.goto("/team/project/pulls");
  const row = page.locator(".issue-list li");
  await expect(row).toContainText("was closed on Mar 12");
  item.state = "open";
  await page.reload();
  await expect(row).toContainText("opened on Feb 10");
  await expect(row).not.toContainText("was closed");
});

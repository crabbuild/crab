import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

test("merge guidance follows current eligibility and protection despite a stale repository catalog", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const state = { ready: true, pending: false, protected: false };
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...fixture.pull(),
          can_merge: false,
          can_manage: false,
          merge_pending: state.pending
            ? {
                request_id: "recorded-merge",
                author: { name: "Alice", profile: null },
                method: "merge_commit",
                pull_version: 1,
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
                message: "Original merge",
                created_at: 1700000000000,
              }
            : null,
          merge_requirements: {
            protected: state.protected,
            required_approvals: state.ready ? 0 : 1,
            approvals: 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: [],
            satisfied: state.ready,
          },
        },
      }),
  );
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  await expect(panel).toContainText("Merge requirements are satisfied");
  await expect(panel).toContainText("Write access is required to merge.");
  await expect(panel.locator(".protected-branch-note")).toHaveCount(0);
  await expect(
    panel.getByRole("button", {
      name: /Merge pull request|Confirm merge|Retry merge/,
    }),
  ).toHaveCount(0);
  await expectNoAccessibilityViolations(page);

  state.ready = false;
  state.protected = true;
  await page.reload();
  await expect(panel).toContainText("Merging is blocked");
  await expect(panel).toContainText("1 more approving review is required.");
  await expect(panel).toContainText(
    "The requirements above must be satisfied before this pull request can be merged.",
  );
  await expect(panel.locator(".protected-branch-note")).toContainText(
    "main is protected.",
  );

  state.pending = true;
  await page.reload();
  await expect(panel).toContainText(
    "Write access is required to recover the original merge.",
  );
  await expect(panel.getByRole("button", { name: "Retry merge" })).toHaveCount(
    0,
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

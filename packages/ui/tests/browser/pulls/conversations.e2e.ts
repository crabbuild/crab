import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "../accessibility";
import { mockPullRequest } from "./fixture";

test("resolve an outdated conversation from the merge requirement", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  fixture.threads[0].outdated = true;
  fixture.threads.push({
    ...fixture.threads[0],
    number: 2,
    body: "Already addressed",
    resolved: true,
  });
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => {
      const unresolved = fixture.threads.filter(
        (thread) => !thread.resolved,
      ).length;
      const pull = fixture.pull();
      return route.fulfill({
        json: {
          ...pull,
          can_merge: unresolved === 0,
          merge_requirements: {
            ...pull.merge_requirements,
            required_approvals: 0,
            checks: [],
            checks_satisfied: true,
            unresolved_conversations: unresolved,
            satisfied: unresolved === 0,
          },
        },
      });
    },
  );

  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  await expect(panel).toContainText("1 unresolved conversation");
  await expect(
    panel.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toHaveCount(0);
  await expectNoAccessibilityViolations(page);
  await panel
    .getByRole("link", { name: "View unresolved conversations" })
    .click();
  await expect(page).toHaveURL(/\/pull\/1\/files\?threads=unresolved$/);
  await expect(
    page.getByRole("heading", { name: "Unresolved review conversations" }),
  ).toBeVisible();
  await expect(page.locator("#review-thread-2")).toHaveCount(0);
  await page
    .locator("#review-thread-1")
    .getByRole("button", { name: "Resolve", exact: true })
    .click();
  await expect(page.locator("#review-thread-1")).toHaveCount(0);
  await page.getByRole("link", { name: "Conversation", exact: true }).click();
  await expect(panel).toContainText("All review conversations resolved.");
  await expect(
    panel.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toBeVisible();
});

import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("triage closes and reopens another author's issue without showing description edits or Git write controls", async ({
  page,
}) => {
  await mockPullRequest(page);
  let canTriage = true;
  let issue = {
    number: 1,
    title: "Investigate reported behavior",
    author: { name: "Alice", profile: null },
    body: "Original report",
    state: "open",
    version: 1,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    can_edit: false,
    can_close: true,
    can_label: true,
    can_assign: true,
    labels: [],
    assignees: [],
    reactions: emptyReactions(),
  };
  const writes: unknown[] = [];
  await page.route("**/api/repos", (route) =>
    route.fulfill({
      json: {
        repositories: [
          {
            owner: "team",
            name: "project",
            description: "",
            access: "read",
            can_admin: false,
            archived: false,
            archive_version: 0,
            protection_version: 0,
            protected_branches: [],
          },
        ],
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/issues/1",
    async (route) => {
      if (route.request().method() === "PATCH") {
        const input = route.request().postDataJSON();
        expect(input.version).toBe(issue.version);
        expect(Object.keys(input).sort()).toEqual(["state", "version"]);
        writes.push(input);
        issue = { ...issue, state: input.state, version: issue.version + 1 };
      }
      return route.fulfill({
        json: {
          ...issue,
          can_close: canTriage,
          can_label: canTriage,
          can_assign: canTriage,
        },
      });
    },
  );
  await page.route("**/api/repos/team/project/issues/1/comments?**", (route) =>
    route.fulfill({ json: { items: [], next: null } }),
  );
  await page.goto("/team/project/issues/1?powerbar=1");
  await expect(
    page.getByRole("heading", { name: "Investigate reported behavior #1" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Edit issue", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Close issue", exact: true }).click();
  await expect(page.locator(".issue-state.closed")).toHaveText("Closed");
  await page.reload();
  await page.getByRole("button", { name: "Reopen issue", exact: true }).click();
  await expect(page.locator(".issue-state.open")).toHaveText("Open");
  expect(writes).toHaveLength(2);
  await expect(page.getByLabel("Edit labels", { exact: true })).toBeVisible();
  await expect(
    page.getByLabel("Edit assignees", { exact: true }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  canTriage = false;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Close issue", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByLabel("Edit labels", { exact: true })).toHaveCount(0);
  await expect(page.getByLabel("Edit assignees", { exact: true })).toHaveCount(
    0,
  );
  expect(writes).toHaveLength(2);
});

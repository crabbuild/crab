import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import { emptyReactions } from "./fixtures";
import { expectNoAccessibilityViolations } from "./accessibility";
import type { Issue } from "@/features/issues/model";

function record(): Issue {
  return {
    number: 1,
    title: "Issue lifecycle",
    body: "Report",
    state: "open",
    state_reason: null,
    closed_at: null,
    author: { name: "Alice", profile: null },
    version: 1,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    can_edit: true,
    can_close: true,
    can_label: false,
    can_assign: false,
    labels: [],
    assignees: [],
    reactions: emptyReactions(),
  };
}

test("close reasons, stale changes and reopening retain the comment draft", async ({
  page,
}) => {
  await mockPullRequest(page);
  let issue = record();
  const writes: object[] = [];
  let conflict = false;
  await page.route(
    (url) =>
      url.pathname.replace(/\/state$/, "") ===
      "/api/repos/team/project/issues/1",
    async (route) => {
      if (route.request().method() === "POST") {
        const input = route.request().postDataJSON();
        writes.push(input);
        if (conflict) {
          conflict = false;
          return route.fulfill({
            status: 409,
            headers: { "X-Crab-Write-Outcome": "rejected" },
            json: {
              error: { code: "conflict", message: "Issue changed elsewhere" },
            },
          });
        }
        expect(input.version).toBe(issue.version);
        issue = {
          ...issue,
          state: input.state,
          state_reason: input.state === "closed" ? input.reason : "reopened",
          closed_at:
            input.state === "closed"
              ? (issue.closed_at ?? 1700000000100)
              : null,
          version: issue.version + 1,
        };
      }
      await route.fulfill({
        json: route.request().method() === "POST" ? { comment: null } : issue,
      });
    },
  );
  await page.route("**/issues/1/milestone", (route) =>
    route.fulfill({ json: { milestone: null, version: 0, can_manage: false } }),
  );
  await page.goto("/team/project/issues/1");
  await expect(
    page.getByRole("heading", { name: "Issue lifecycle #1" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Choose close reason" }).click();
  await page.getByRole("menuitemradio", { name: /Not planned/ }).click();
  expect(writes).toHaveLength(0);
  await page
    .getByRole("button", { name: "Close as not planned", exact: true })
    .click();
  await expect(page.locator(".issue-state")).toHaveText(
    "Closed as not planned",
  );
  await page
    .getByLabel("Comment", { exact: true })
    .fill("Keep this unfinished comment");
  await page.getByRole("button", { name: "Change close reason" }).click();
  await page.getByRole("menuitemradio", { name: /Completed/ }).click();
  await expect(page.locator(".issue-state.closed")).toHaveText("Closed");
  await expect(page.getByLabel("Comment", { exact: true })).toHaveValue(
    "Keep this unfinished comment",
  );
  await page.getByLabel("Comment", { exact: true }).fill("");
  conflict = true;
  await page.getByRole("button", { name: "Reopen issue", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Issue changed elsewhere",
  );
  await expect(page.locator(".issue-state.closed")).toHaveText("Closed");
  await page
    .getByRole("button", { name: "Refresh issue before retrying" })
    .click();
  await page.getByRole("button", { name: "Reopen issue", exact: true }).click();
  await expect(page.locator(".issue-state.open")).toHaveText("Open");
  expect(issue.closed_at).toBeNull();
  await expectNoAccessibilityViolations(page);
});

test("a delayed close response cannot replace a newer refreshed issue", async ({
  page,
}) => {
  await mockPullRequest(page);
  let issue = record();
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let begun!: () => void;
  const started = new Promise<void>((resolve) => {
    begun = resolve;
  });
  await page.route(
    (url) =>
      url.pathname.replace(/\/state$/, "") ===
      "/api/repos/team/project/issues/1",
    async (route) => {
      if (route.request().method() === "POST") {
        const older: Issue = {
          ...issue,
          state: "closed",
          state_reason: "completed",
          closed_at: 1700000000100,
          version: 2,
        };
        issue = { ...older, title: "A newer title", version: 3 };
        begun();
        await gate;
        return route.fulfill({ json: { comment: null } });
      }
      await route.fulfill({
        json: route.request().method() === "POST" ? { comment: null } : issue,
      });
    },
  );
  await page.route("**/issues/1/milestone", (route) =>
    route.fulfill({ json: { milestone: null, version: 0, can_manage: false } }),
  );
  await page.goto("/team/project/issues/1");
  await page.getByRole("button", { name: "Close issue", exact: true }).click();
  await started;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "A newer title #1" }),
  ).toBeVisible();
  release();
  await expect(
    page.getByRole("button", { name: "Reopen issue", exact: true }),
  ).toBeEnabled();
  await expect(
    page.getByRole("heading", { name: "A newer title #1" }),
  ).toBeVisible();
});

import { expect, test } from "@playwright/test";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("reactions persist on refresh, stay scoped, and ignore older snapshots", async ({
  page,
}) => {
  let issueReactions = emptyReactions();
  let commentReactions = emptyReactions();
  let revision = 1;
  let staleRefresh = false;
  let failNextReaction = false;
  let archived = false;
  const writes: Array<{ path: string; kind: string; active: boolean }> = [];
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path.endsWith("/links"))
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    if (path.endsWith("/activity"))
      return route.fulfill({ json: { items: [], next: null } });
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "test-csrf",
          user: { issuer: "test", subject: "alice", name: "Alice" },
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "",
              access: "read",
              archive_version: 0,
              archived,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    if (path.includes("/reactions/")) {
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
      if (failNextReaction) {
        failNextReaction = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "Reaction storage unavailable" } },
        });
      }
      const input = request.postDataJSON() as {
        kind: keyof typeof issueReactions.groups;
        active: boolean;
      };
      writes.push({ path, ...input });
      const current = path.endsWith("/comments/1")
        ? commentReactions
        : issueReactions;
      const next = {
        revision: ++revision,
        groups: {
          ...current.groups,
          [input.kind]: { count: input.active ? 1 : 0, reacted: input.active },
        },
      };
      if (path.endsWith("/comments/1")) commentReactions = next;
      else issueReactions = next;
      return route.fulfill({ json: next });
    }
    if (path === "/api/repos/team/project/issues/1")
      return route.fulfill({
        json: {
          number: 1,
          title: "Discuss reactions",
          body: "Issue body",
          author: { name: "Alice", profile: null },
          version: 1,
          state: "open",
          created_at: 1700000000000,
          updated_at: 1700000000000,
          can_edit: false,
          can_close: false,
          can_label: false,
          can_assign: false,
          labels: [],
          assignees: [],
          reactions: staleRefresh ? emptyReactions() : issueReactions,
        },
      });
    if (path === "/api/repos/team/project/issues/1/comments")
      return route.fulfill({
        json: {
          items: [
            {
              number: 1,
              body: "Comment body",
              author: { name: "Bob", profile: null },
              version: 1,
              created_at: 1700000001000,
              updated_at: 1700000001000,
              can_edit: false,
              reactions: commentReactions,
            },
          ],
          next: null,
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected endpoint ${path}` } },
    });
  });
  await page.goto("/team/project/issues/1");
  const issue = page.locator("article.discussion-card").first();
  await issue
    .getByRole("button", { name: "Add reaction", exact: true })
    .click();
  await page
    .getByRole("menuitemcheckbox", { name: "Thumbs up", exact: true })
    .click();
  const thumb = issue.getByRole("button", {
    name: "Thumbs up: 1 reaction",
    exact: true,
  });
  await expect(thumb).toHaveAttribute("aria-pressed", "true");
  staleRefresh = true;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Refresh", exact: true }),
  ).toBeEnabled();
  await page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
      ),
  );
  await expect(thumb).toHaveAttribute("aria-pressed", "true");
  staleRefresh = false;
  await page.reload();
  await expect(thumb).toHaveAttribute("aria-pressed", "true");
  const comment = page.locator("#issuecomment-1");
  await comment
    .getByRole("button", { name: "Add reaction", exact: true })
    .click();
  await page
    .getByRole("menuitemcheckbox", { name: "Rocket", exact: true })
    .click();
  await expect(
    comment.getByRole("button", { name: "Rocket: 1 reaction", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  await expect(
    issue.getByRole("button", { name: "Rocket: 1 reaction", exact: true }),
  ).toHaveCount(0);
  await thumb.click();
  await expect(thumb).toHaveCount(0);
  expect(writes.map((write) => write.path)).toEqual([
    "/api/repos/team/project/reactions/issues/1",
    "/api/repos/team/project/reactions/issues/1/comments/1",
    "/api/repos/team/project/reactions/issues/1",
  ]);
  failNextReaction = true;
  await issue
    .getByRole("button", { name: "Add reaction", exact: true })
    .click();
  await page
    .getByRole("menuitemcheckbox", { name: "Heart", exact: true })
    .click();
  await expect(issue.getByRole("alert")).toHaveText(
    "Reaction storage unavailable",
  );
  await expect(
    issue.getByRole("button", { name: "Heart: 1 reaction", exact: true }),
  ).toHaveCount(0);
  await issue
    .getByRole("button", { name: "Add reaction", exact: true })
    .click();
  await page
    .getByRole("menuitemcheckbox", { name: "Heart", exact: true })
    .click();
  await expect(issue.getByRole("alert")).toHaveCount(0);
  await expect(
    issue.getByRole("button", { name: "Heart: 1 reaction", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  archived = true;
  await page.reload();
  await expect(
    page.getByRole("button", { name: "Add reaction", exact: true }),
  ).toHaveCount(0);
  await expect(
    issue.getByRole("button", { name: "Heart: 1 reaction", exact: true }),
  ).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
});

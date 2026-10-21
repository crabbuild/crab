import { expect, test, type Page } from "@playwright/test";
import { emptyReactions, emptyReviewRequests } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function setup(page: Page) {
  let active = false;
  let version = 0;
  let access = "write";
  let archived = false;
  let fail = false;
  const unexpected: string[] = [];
  const record = {
    number: 1,
    author: "Alice",
    body: "Related work",
    state: "open",
    version: 1,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    can_edit: false,
    can_close: false,
    can_label: false,
    can_assign: false,
    labels: [],
    assignees: [],
    reactions: emptyReactions(),
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          user: { issuer: "test", subject: "alice", name: "Alice" },
          csrf: "test-csrf",
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
              access,
              archived,
              can_admin: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (path.endsWith("/refs"))
      return route.fulfill({ json: { head: null, refs: [], generation: 1 } });
    if (path.endsWith("/linked-issues/1")) {
      if (request.method() === "PUT") {
        expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
        if (fail) {
          fail = false;
          return route.fulfill({
            status: 503,
            json: { error: { message: "Link storage unavailable" } },
          });
        }
        const input = request.postDataJSON();
        if (input.version !== version)
          return route.fulfill({
            status: 409,
            json: {
              error: { code: "conflict", message: "Linked work changed" },
            },
          });
        active = input.active;
        version++;
      }
      return route.fulfill({ json: { active, version } });
    }
    if (path.endsWith("/review-requests"))
      return route.fulfill({ json: emptyReviewRequests() });
    if (path.endsWith("/links")) {
      const pull = path.includes("/pulls/");
      return route.fulfill({
        json: {
          items: active
            ? [
                {
                  work: {
                    subject: { kind: pull ? "issue" : "pull", number: 1 },
                    title: pull ? "Related issue" : "Related pull",
                    state: "open",
                  },
                  version,
                },
              ]
            : [],
          next: null,
          can_manage: access === "write",
        },
      });
    }
    if (path.endsWith("/activity"))
      return route.fulfill({
        json: {
          items: version
            ? [
                {
                  number: version,
                  target: { kind: "pull", number: 1 },
                  title: "Related pull",
                  actor: "Alice",
                  action: {
                    kind: active ? "issue_linked" : "issue_unlinked",
                    number: 1,
                    title: "Related issue",
                  },
                  created_at: 1700000000000,
                },
              ]
            : [],
          next: null,
        },
      });
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    if (path.endsWith("/comments") || path.endsWith("/reviews"))
      return route.fulfill({ json: { items: [], next: null } });
    if (path.endsWith("/issues/1"))
      return route.fulfill({ json: { ...record, title: "Related issue" } });
    if (path.endsWith("/pulls/1"))
      return route.fulfill({
        json: {
          ...record,
          draft: false,
          title: "Related pull",
          base_ref: "refs/heads/main",
          head_ref: "refs/heads/feature",
          base_oid: "a".repeat(40),
          head_oid: "b".repeat(40),
          original_base_oid: "a".repeat(40),
          original_head_oid: "b".repeat(40),
          can_manage: false,
          can_decide: false,
          can_merge: false,
          branches_available: true,
          merge: null,
          merge_pending: null,
          merge_requirements: {
            protected: false,
            required_approvals: 0,
            approvals: 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: [],
            satisfied: true,
          },
        },
      });
    unexpected.push(`${request.method()} ${path}`);
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected ${path}` } },
    });
  });
  return {
    failNext: () => {
      fail = true;
    },
    race: () => {
      version++;
    },
    readOnly: () => {
      access = "read";
    },
    archive: () => {
      archived = true;
    },
    unexpected,
  };
}

test("linking and unlinking related work updates both pages and retains failed drafts", async ({
  page,
}) => {
  const state = await setup(page);
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", {
    name: "Linked issues",
    exact: true,
  });
  await panel.getByLabel("Issue number").fill("1");
  await expect(
    panel.getByRole("button", { name: "Link issue", exact: true }),
  ).toBeEnabled();
  state.failNext();
  await panel.getByRole("button", { name: "Link issue", exact: true }).click();
  await expect(panel.getByRole("alert")).toHaveText("Link storage unavailable");
  await expect(panel.getByLabel("Issue number")).toHaveValue("1");
  await panel.getByRole("button", { name: "Link issue", exact: true }).click();
  await expect(
    panel.getByRole("link", { name: "#1 Related issue" }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Activity", exact: true }),
  ).toContainText("linked issue Related issue on #1 Related pull");
  await page.reload();
  await panel.getByRole("link", { name: "#1 Related issue" }).click();
  await expect(page).toHaveURL(/\/issues\/1$/);
  const backlinks = page.getByRole("region", {
    name: "Linked pull requests",
    exact: true,
  });
  await expect(
    backlinks.getByRole("link", { name: "#1 Related pull" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  await backlinks
    .getByRole("button", { name: "Unlink pull request #1" })
    .click();
  await expect(backlinks).toContainText("No linked pull requests.");
  await page.goto("/team/project/pull/1");
  await expect(panel).toContainText("No linked issues.");
  await expect(
    page.getByRole("region", { name: "Activity", exact: true }),
  ).toContainText("unlinked issue");
  expect(state.unexpected).toEqual([]);
});

test("link conflicts require refresh and read-only or archived views keep backlinks without write controls", async ({
  page,
}) => {
  const state = await setup(page);
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", {
    name: "Linked issues",
    exact: true,
  });
  await panel.getByLabel("Issue number").fill("1");
  await expect(
    panel.getByRole("button", { name: "Link issue", exact: true }),
  ).toBeEnabled();
  state.race();
  await panel.getByRole("button", { name: "Link issue", exact: true }).click();
  await expect(panel.getByRole("alert")).toHaveText("Linked work changed");
  await expect(panel.getByLabel("Issue number")).toHaveValue("1");
  await panel.getByRole("button", { name: "Reload current links" }).click();
  await expect(
    panel.getByRole("button", { name: "Link issue", exact: true }),
  ).toBeEnabled();
  await panel.getByRole("button", { name: "Link issue", exact: true }).click();
  await expect(
    panel.getByRole("link", { name: "#1 Related issue" }),
  ).toBeVisible();
  state.readOnly();
  await page.reload();
  await expect(
    panel.getByRole("link", { name: "#1 Related issue" }),
  ).toBeVisible();
  await expect(panel.getByRole("button")).toHaveCount(0);
  state.archive();
  await page.reload();
  await expect(
    panel.getByRole("link", { name: "#1 Related issue" }),
  ).toBeVisible();
  await expect(panel.getByRole("button")).toHaveCount(0);
  expect(state.unexpected).toEqual([]);
});

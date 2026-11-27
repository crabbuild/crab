import { emptyPullRollup, fulfillIdleReads } from "./fixtures";
import { conversationPage } from "./conversation";
import { expect, test, type Page } from "@playwright/test";
import { emptyReactions, emptyReviewers } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function setup(
  page: Page,
  initialDraft: boolean,
  withCodeOwners = false,
) {
  let draft = initialDraft;
  let version = 1;
  let title = "Draft proposal";
  let body = "Work in progress";
  let failed = false;
  let canManage = true;
  let archived = false;
  let pending = false;
  let published = false;
  let merged = false;
  let codeOwnersRequested = false;
  const unexpected: string[] = [];
  const writes: Array<{ version: number; draft: boolean }> = [];
  const base = "a".repeat(40),
    head = "b".repeat(40);
  const events: Array<Record<string, unknown>> = [];
  const pull = () => ({
    number: 1,
    title,
    body,
    draft,
    version,
    state: merged ? "merged" : "open",
    author: { name: "Alice", profile: null },
    created_at: 1700000000000,
    closed_at: merged ? 1700000060000 : null,
    merged_at: merged ? 1700000060000 : null,
    updated_at: 1700000000000,
    base_ref: "refs/heads/main",
    head_ref: "refs/heads/feature",
    base_oid: base,
    head_oid: head,
    original_base_oid: base,
    original_head_oid: head,
    labels: [],
    assignees: [],
    can_edit: false,
    can_label: false,
    can_assign: false,
    can_manage: canManage,
    can_decide: false,
    can_merge: !draft && canManage && !merged,
    branches_available: true,
    merge: merged
      ? {
          author: { name: "Alice", profile: null },
          method: "fast_forward",
          commit_oid: head,
          message: "",
          created_at: 1700000000000,
        }
      : null,
    merge_pending: pending
      ? {
          request_id: "recorded-merge",
          author: { name: "Alice", profile: null },
          method: "fast_forward",
          pull_version: version,
          base_oid: base,
          head_oid: head,
          message: "",
          created_at: 1700000000000,
        }
      : null,
    reactions: emptyReactions(),
    merge_methods: ["merge_commit", "squash", "rebase", "fast_forward"],
    merge_requirements: {
      protected: false,
      required_approvals: 0,
      approvals: 0,
      changes_requested: 0,
      checks_satisfied: true,
      checks: [],
      satisfied: true,
    },
  });
  await page.route("**/api/**", async (route) => {
    if (await fulfillIdleReads(route)) return;
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (
      request.method() === "GET" &&
      /^\/api\/repos\/team\/project\/(issues|pulls)\/\d+\/projects$/.test(path)
    )
      return route.fulfill({ json: { items: [], can_manage: canManage } });
    if (path.endsWith("/assignees"))
      return route.fulfill({
        json: { items: [], can_manage: canManage && !archived },
      });
    // Branch reads around the pull page; none affects readiness.
    if (request.method() === "GET") {
      if (path.endsWith("/pulls/open-comparison"))
        return route.fulfill({ json: { pull_number: null } });
      if (path.endsWith("/pulls/templates"))
        return route.fulfill({
          json: { default: null, items: [], truncated: false, source: null },
        });
      if (path.endsWith("/pulls/recent-pushes"))
        return route.fulfill({
          json: { base_ref: "refs/heads/main", items: [] },
        });
      if (path.endsWith("/head-branch"))
        return route.fulfill({ json: { branch: null } });
    }
    if (path.endsWith("/conversation"))
      return conversationPage(route, { activity: events, head });
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          user: { issuer: "test", subject: "alice", name: "Alice" },
          csrf: "csrf",
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              id: "00000000-0000-4000-8000-000000000099",
              owner: "team",
              name: "project",
              description: "",
              access: canManage ? "write" : "read",
              archived,
              archive_version: 0,
              can_admin: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (path.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: base },
          refs: [
            { name: "refs/heads/main", oid: base },
            { name: "refs/heads/feature", oid: head },
          ],
          generation: 1,
        },
      });
    if (path.endsWith("/changes"))
      return route.fulfill({
        json: { base, base_tip: base, commit: head, changes: [] },
      });
    if (path.endsWith("/merge")) {
      expect(request.method()).toBe("POST");
      expect(archived && pending).toBe(true);
      expect(request.postDataJSON().request_id).toBe("recorded-merge");
      pending = false;
      if (!published)
        return route.fulfill({
          status: 403,
          json: {
            error: {
              code: "repository_archived",
              message: "Repository is archived",
            },
          },
        });
      merged = true;
      return route.fulfill({ json: pull() });
    }
    if (path.endsWith("/readiness")) {
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      const input = request.postDataJSON() as {
        version: number;
        draft: boolean;
      };
      writes.push(input);
      if (failed) {
        failed = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "Readiness storage unavailable" } },
        });
      }
      if (input.version !== version)
        return route.fulfill({
          status: 409,
          json: { error: { code: "conflict", message: "Readiness changed" } },
        });
      draft = input.draft;
      version++;
      events.unshift({
        number: version,
        target: { kind: "pull", number: 1 },
        title,
        actor: { name: "Alice", profile: null },
        action: { kind: draft ? "drafted" : "ready_for_review" },
        created_at: 1700000000000,
      });
      if (withCodeOwners && !draft && !codeOwnersRequested) {
        codeOwnersRequested = true;
        events.unshift({
          number: 100 + version,
          target: { kind: "pull", number: 1 },
          title,
          actor: { name: "Alice", profile: null },
          action: { kind: "code_owner_reviews_requested" },
          created_at: 1700000000000,
        });
      }
      return route.fulfill({ json: pull() });
    }
    if (path.endsWith("/pulls")) {
      if (request.method() === "POST") {
        const input = request.postDataJSON() as {
          title: string;
          body: string;
          draft: boolean;
          expected: { base_oid: string; head_oid: string };
        };
        expect(input.draft).toBe(true);
        expect(input.expected).toEqual({ base_oid: base, head_oid: head });
        draft = input.draft;
        title = input.title;
        body = input.body;
        version = 1;
        return route.fulfill({ status: 201, json: pull() });
      }
      return route.fulfill({
        json: {
          items: [
            {
              ...pull(),
              kind: "pull",
              rollup: emptyPullRollup(),
              comment_count: 0,
              reaction_count: 0,
            },
          ],
          next: null,
          counts: { open: 1, closed: 0, merged: 0 },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          matching: 1,
          navigation: {
            kind: "state",
            selected: "open",
            open: "is:open",
            closed: "is:closed",
          },
        },
      });
    }
    if (path.endsWith("/pulls/1")) return route.fulfill({ json: pull() });
    if (path.endsWith("/comments") || path.endsWith("/reviews"))
      return route.fulfill({ json: { items: [], next: null } });
    if (path.endsWith("/activity"))
      return route.fulfill({ json: { items: events, next: null } });
    if (path.endsWith("/reviewers"))
      return route.fulfill({
        json: codeOwnersRequested
          ? {
              ...emptyReviewers(),
              version,
              items: [
                {
                  id: "b".repeat(64),
                  reviewer: { name: "Bob", profile: null },
                  candidate_id: "01900000-0000-7000-8000-000000000002",
                  request: {
                    id: "b".repeat(64),
                    requested_by: { name: "Alice", profile: null },
                    state: { kind: "requested" },
                    code_owner: true,
                  },
                  review: null,
                  decision: null,
                },
              ],
              teams: [
                {
                  id: "01900000-0000-7000-8000-000000000003",
                  name: "Release Engineering",
                  state: { kind: "requested" },
                  can_request: true,
                  code_owner: true,
                },
              ],
            }
          : emptyReviewers(),
      });
    if (path.endsWith("/links"))
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    unexpected.push(`${request.method()} ${path}`);
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected ${path}` } },
    });
  });
  return {
    fail: () => {
      failed = true;
    },
    race: () => {
      version++;
    },
    readOnly: () => {
      canManage = false;
    },
    archive: () => {
      archived = true;
    },
    admittedMerge: (alreadyPublished: boolean) => {
      pending = true;
      archived = true;
      published = alreadyPublished;
    },
    unexpected,
    writes,
  };
}

test("marking ready shows the saved automatic code owner requests and activity", async ({
  page,
}) => {
  const state = await setup(page, true, true);
  await page.goto("/team/project/pull/1");
  const reviewers = page.getByRole("region", {
    name: "Reviewers",
    exact: true,
  });
  await expect(reviewers.getByText("No reviews yet.")).toBeVisible();
  await page
    .getByRole("button", { name: "Mark ready for review", exact: true })
    .click();
  await expect(
    reviewers.getByText("Code owner review requested", { exact: true }),
  ).toBeVisible();
  await expect(
    reviewers.getByText("Team code owner review requested", { exact: true }),
  ).toBeVisible();
  const history = page.getByRole("region", {
    name: "Conversation history",
    exact: true,
  });
  await expect(
    history.getByText("requested code owner reviews on", { exact: false }),
  ).toHaveCount(1);
  await page.reload();
  await expect(
    reviewers.getByText("Release Engineering", { exact: true }),
  ).toBeVisible();
  await expect(
    history.getByText("requested code owner reviews on", { exact: false }),
  ).toHaveCount(1);
  expect(state.writes).toEqual([{ version: 1, draft: false }]);
  expect(state.unexpected).toEqual([]);
});

test("draft creation, reload, readiness and conversion expose truthful merge controls", async ({
  page,
}) => {
  const state = await setup(page, false);
  await page.goto("/team/project/compare/main...feature");
  await page.getByLabel("Title", { exact: true }).fill("Draft proposal");
  await page.getByRole("button", { name: "Choose pull request mode" }).click();
  await page
    .getByRole("menuitemradio", { name: "Create draft pull request" })
    .click();
  await page
    .getByRole("button", { name: "Create draft pull request", exact: true })
    .click();
  await expect(page).toHaveURL(/\/pull\/1$/);
  await expect(page.locator(".pull-state")).toHaveText("Draft");
  await expect(
    page.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toHaveCount(0);
  await page.reload();
  await expect(
    page.getByRole("region", { name: "Merge status" }),
  ).toContainText("This pull request is a draft");
  state.fail();
  await page
    .getByRole("button", { name: "Mark ready for review", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Readiness storage unavailable",
  );
  await expect(page.locator(".pull-state")).toHaveText("Draft");
  await page
    .getByRole("button", { name: "Mark ready for review", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Open");
  await expect(
    page.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Conversation history", exact: true }),
  ).toContainText("marked ready for review");
  await page
    .getByRole("button", { name: "Convert to draft", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Draft");
  await page.goto("/team/project/pulls");
  await expect(
    page.getByRole("img", { name: "draft", exact: true }),
  ).toBeVisible();
  await page.getByRole("link", { name: "Draft proposal", exact: true }).click();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  expect(state.unexpected).toEqual([]);
});

test("conflicts require a reload and unauthorized or archived views cannot convert readiness", async ({
  page,
}) => {
  const state = await setup(page, false);
  await page.goto("/team/project/pull/1");
  await expect(
    page.getByRole("button", { name: "Convert to draft", exact: true }),
  ).toBeVisible();
  state.race();
  await page
    .getByRole("button", { name: "Convert to draft", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText("Readiness changed");
  expect(state.writes).toHaveLength(1);
  await page
    .getByRole("button", { name: "Reload pull request", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Convert to draft", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Draft");
  state.readOnly();
  await page.reload();
  await expect(
    page.getByRole("region", { name: "Merge status" }),
  ).toContainText("This pull request is a draft");
  await expect(
    page.getByRole("button", { name: "Mark ready for review", exact: true }),
  ).toHaveCount(0);
  state.archive();
  await page.reload();
  await expect(page.locator(".pull-state")).toHaveText("Draft");
  await expect(
    page.getByRole("button", { name: "Mark ready for review", exact: true }),
  ).toHaveCount(0);
  expect(state.unexpected).toEqual([]);
});

for (const published of [true, false]) {
  test(`archived merge recovery ${published ? "confirms publication" : "refreshes aborted admission"}`, async ({
    page,
  }) => {
    const state = await setup(page, false);
    state.admittedMerge(published);
    await page.goto("/team/project/pull/1");
    const merge = page.getByRole("region", {
      name: "Merge status",
      exact: true,
    });
    await merge
      .getByRole("button", { name: "Retry merge", exact: true })
      .click();
    if (published) {
      await expect(merge).toContainText("Pull request merged");
      await expect(page.locator(".pull-state")).toHaveText("Merged");
    } else {
      await expect(merge.getByRole("alert")).toContainText(
        "Repository is archived",
      );
      await merge
        .getByRole("button", { name: "Refresh merge status", exact: true })
        .click();
      await expect(merge).toContainText("This repository is archived");
      await expect(merge.getByRole("button")).toHaveCount(0);
    }
    await expect(
      page.getByRole("button", { name: "Convert to draft", exact: true }),
    ).toHaveCount(0);
    await expectNoAccessibilityViolations(page);
    expect(state.unexpected).toEqual([]);
  });
}

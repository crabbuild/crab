import { expect, test, type Page } from "@playwright/test";
import { emptyReactions, emptyReviewRequests } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function setup(page: Page, initialDraft: boolean) {
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
    author: "Alice",
    created_at: 1700000000000,
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
          author: "Alice",
          method: "fast_forward",
          commit_oid: head,
          message: "",
          created_at: 1700000000000,
        }
      : null,
    merge_pending: pending
      ? {
          request_id: "recorded-merge",
          author: "Alice",
          method: "fast_forward",
          pull_version: version,
          base_oid: base,
          head_oid: head,
          message: "",
          created_at: 1700000000000,
        }
      : null,
    reactions: emptyReactions(),
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
    const request = route.request();
    const path = new URL(request.url()).pathname;
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
      return route.fulfill({ json: { base, commit: head, changes: [] } });
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
        actor: "Alice",
        action: { kind: draft ? "drafted" : "ready_for_review" },
        created_at: 1700000000000,
      });
      return route.fulfill({ json: pull() });
    }
    if (path.endsWith("/pulls")) {
      if (request.method() === "POST") {
        const input = request.postDataJSON() as {
          title: string;
          body: string;
          draft: boolean;
        };
        expect(input.draft).toBe(true);
        draft = input.draft;
        title = input.title;
        body = input.body;
        version = 1;
        return route.fulfill({ status: 201, json: pull() });
      }
      return route.fulfill({ json: { items: [pull()], next: null } });
    }
    if (path.endsWith("/pulls/1")) return route.fulfill({ json: pull() });
    if (path.endsWith("/comments") || path.endsWith("/reviews"))
      return route.fulfill({ json: { items: [], next: null } });
    if (path.endsWith("/activity"))
      return route.fulfill({ json: { items: events, next: null } });
    if (path.endsWith("/review-requests"))
      return route.fulfill({ json: emptyReviewRequests() });
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

test("draft creation, reload, readiness and conversion expose truthful merge controls", async ({
  page,
}) => {
  const state = await setup(page, false);
  await page.goto("/team/project/compare/main...feature");
  await page.getByLabel("Title", { exact: true }).fill("Draft proposal");
  await page.getByRole("checkbox", { name: "Create as draft" }).check();
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
    page.getByRole("region", { name: "Activity", exact: true }),
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

import { expect, test, type Page } from "@playwright/test";
import type {
  PullComment,
  PullReview,
} from "../../../src/features/pull-requests/model";
import { mockPullRequest } from "./fixture";
import { emptyReactions } from "../fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

async function historyFixture(page: Page) {
  await mockPullRequest(page);
  const comments: PullComment[] = Array.from({ length: 65 }, (_, index) => ({
    number: index + 1,
    author: { name: "Alice", profile: null },
    body: `Conversation comment ${index + 1}`,
    reactions: emptyReactions(),
    version: 1,
    created_at: 1_700_000_000_000 + index * 2000,
    updated_at: 1_700_000_000_000 + index * 2000,
    can_edit: true,
  }));
  const reviews: PullReview[] = comments.map((comment) => ({
    ...comment,
    author: { name: "Bob", profile: null },
    body: `Submitted review ${comment.number}`,
    created_at: comment.created_at + 1000,
    state: "approved",
    current: true,
    commit_oid: "b".repeat(40),
  }));
  const reads: string[] = [];
  const writes: Array<{ body: string; request_id: string }> = [];
  const submissions = new Map<string, PullComment>();
  let reviewHead: string | null = "b".repeat(40);
  let failEarlier = true,
    loseReply = true;
  const root = "/api/repos/team/project";
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname,
      method = route.request().method();
    if (path === `${root}/issues/1`)
      return route.fulfill({
        json: {
          ...comments[0],
          title: "Issue history",
          state: "open",
          labels: [],
          assignees: [],
          can_close: false,
          can_label: false,
          can_assign: false,
        },
      });
    if (path === `${root}/pulls/1/reviewers`)
      return route.fulfill({
        json: {
          items: [
            {
              id: "b".repeat(64),
              reviewer: { name: "Bob", profile: null },
              candidate_id: null,
              request: null,
              review: {
                number: 1,
                state: "approved",
                current: true,
                commit_oid: "b".repeat(40),
              },
              decision: {
                number: 1,
                state: "approved",
                current: true,
                commit_oid: "b".repeat(40),
              },
            },
          ],
          next: null,
          version: 1,
          can_manage: false,
          candidates: [],
        },
      });
    const match =
      /^\/api\/repos\/team\/project\/(pulls|issues)\/1\/(comments|reviews)(?:\/(\d+))?$/.exec(
        path,
      );
    if (!match) return route.fallback();
    const source = match[2] === "comments" ? comments : reviews;
    if (method === "GET") {
      reads.push(`${path}${url.search}`);
      if (match[3]) {
        const item = source.find((item) => item.number === Number(match[3]));
        // The canonical point record is newer than the historical list snapshot.
        return item
          ? route.fulfill({
              json: { ...item, body: `${item.body} (canonical)`, version: 2 },
            })
          : route.fulfill({
              status: 404,
              json: { error: { message: "Contribution not found" } },
            });
      }
      const before = Number(
        url.searchParams.get("before") ?? Number.MAX_SAFE_INTEGER,
      );
      if (
        match[2] === "comments" &&
        url.searchParams.has("before") &&
        failEarlier
      ) {
        failEarlier = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "History temporarily unavailable" } },
        });
      }
      const available = source.filter((item) => item.number < before).reverse(),
        items = available.slice(0, 30);
      return route.fulfill({
        json: {
          items,
          next: available.length > 30 ? items.at(-1)!.number : null,
          ...(match[2] === "reviews" ? { head_oid: reviewHead } : {}),
        },
      });
    }
    if (method === "POST" && match[2] === "comments") {
      const input = route.request().postDataJSON() as {
        body: string;
        request_id: string;
      };
      writes.push(input);
      let saved = submissions.get(input.request_id);
      if (!saved) {
        saved = {
          ...comments.at(-1)!,
          number: comments.length + 1,
          body: input.body,
          created_at: Date.now(),
          version: 1,
        };
        comments.push(saved);
        submissions.set(input.request_id, saved);
      }
      expect(saved.body).toBe(input.body);
      if (loseReply) {
        loseReply = false;
        return route.fulfill({
          status: 503,
          json: {
            error: {
              message: "Reply lost after saving. Retry this submission.",
            },
          },
        });
      }
      return route.fulfill({ status: 201, json: saved });
    }
    throw new Error(`Unexpected history mutation: ${method} ${path}`);
  });
  return {
    reads,
    writes,
    comments,
    head: (oid: string | null) => {
      reviewHead = oid;
    },
  };
}

test("all PR comments and reviews remain reachable when an older page fails", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1?powerbar=1");
  const timeline = page.locator(".pull-timeline");
  await expect(timeline.locator("article")).toHaveCount(60);
  await page.getByRole("button", { name: "Load earlier comments" }).click();
  const failure = page
    .getByRole("alert")
    .filter({ hasText: "History temporarily unavailable" });
  await expect(failure).toBeVisible();
  await expect(timeline.locator("article")).toHaveCount(60);
  await page.getByRole("button", { name: "Load earlier reviews" }).click();
  await expect(timeline.locator("article")).toHaveCount(90);
  await failure.getByRole("button", { name: "Try again" }).click();
  await expect(timeline.locator("article")).toHaveCount(120);
  await page.getByRole("button", { name: "Load earlier comments" }).click();
  await page.getByRole("button", { name: "Load earlier reviews" }).click();
  await expect(timeline.locator("article")).toHaveCount(130);
  await expect(
    page.getByRole("button", { name: /Load earlier (comments|reviews)/ }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Refresh conversation" }).click();
  await expect(timeline.locator("article")).toHaveCount(130);
  expect(
    await timeline
      .locator("article")
      .evaluateAll((nodes) => nodes.map((node) => node.id)),
  ).toEqual(
    Array.from({ length: 65 }, (_, i) => [
      `issuecomment-${i + 1}`,
      `pullrequestreview-${i + 1}`,
    ]).flat(),
  );
  for (const [head, label] of [
    ["c".repeat(40), "Outdated"],
    [null, "Head unavailable"],
  ] as const) {
    model.head(head);
    await page.getByRole("button", { name: "Refresh conversation" }).click();
    await expect(timeline.locator(".review-outdated")).toHaveCount(65);
    await expect(
      timeline.locator(".review-outdated").filter({ hasText: label }),
    ).toHaveCount(65);
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

test("a reviewer decision opens its exact old review without walking history pages", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1?powerbar=1");
  await expect(page.locator(".review-event")).toHaveCount(30);
  const link = page.getByRole("link", {
    name: "Open review #1 by Bob",
    exact: true,
  });
  await expect(link).toHaveAttribute(
    "href",
    "/team/project/pull/1#pullrequestreview-1",
  );
  await link.click();
  await expect(page).toHaveURL(/\?powerbar=1#pullrequestreview-1$/);
  const target = page.locator("#pullrequestreview-1");
  await expect(target).toBeFocused();
  await expect(target).toContainText("Submitted review 1 (canonical)");
  await expect(page.locator(".review-event")).toHaveCount(31);
  expect(model.reads.filter((read) => read.includes("before="))).toHaveLength(
    0,
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload();
  await expect(target).toBeFocused();
  await expect(target).toBeInViewport();
  await expect(page.locator(".review-event")).toHaveCount(31);
  await page
    .getByRole("link", { name: "Comment #65 permalink", exact: true })
    .click();
  await expect(page.locator("#issuecomment-65")).toBeFocused();
  await page.goBack();
  await expect(target).toBeFocused();
  await page.goto("/team/project/pull/1#pullrequestreview-999");
  const failure = page
    .getByRole("alert")
    .filter({ hasText: "Contribution not found" });
  await expect(failure).toContainText("Unable to open this contribution");
  await expect(page.locator(".review-event")).toHaveCount(30);
  await failure.getByRole("button", { name: "Try again" }).click();
  await expect(failure).toBeVisible();
});

test("comment drafts and retry identities survive tabs and metadata refreshes", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1");
  const editor = page.getByRole("textbox", { name: "Comment", exact: true });
  const text = "Keep this comment while inspecting the checks";
  await editor.fill(text);
  await page.getByRole("link", { name: "Checks", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "ci/test", exact: true }),
  ).toBeVisible();
  await page.getByRole("link", { name: "Conversation", exact: true }).click();
  await expect(editor).toHaveValue(text);
  await page
    .getByRole("button", { name: "Close pull request", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Closed");
  await expect(editor).toHaveValue(text);
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let block = true;
  await page.route(
    "**/api/repos/team/project/pulls/1/comments?**",
    async (route) => {
      if (route.request().method() === "POST" && block) {
        block = false;
        await gate;
      }
      await route.fallback();
    },
  );
  try {
    await page.getByRole("button", { name: "Comment", exact: true }).click();
    await expect(editor).toBeDisabled();
    await page.getByRole("link", { name: "Checks", exact: true }).click();
    await page.getByRole("link", { name: "Conversation", exact: true }).click();
    await expect(editor).toHaveValue(text);
    await expect(editor).toBeDisabled();
  } finally {
    release();
  }
  await expect(page.getByRole("alert")).toContainText(
    "Reply lost after saving",
  );
  await page.getByRole("link", { name: "Checks", exact: true }).click();
  await page.getByRole("link", { name: "Conversation", exact: true }).click();
  await expect(editor).toHaveValue(text);
  await expect(page.getByRole("alert")).toContainText(
    "Reply lost after saving",
  );
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(editor).toHaveValue("");
  await expect(page.locator("#issuecomment-66")).toContainText(text);
  expect(model.writes).toHaveLength(2);
  expect(model.writes[0]).toEqual(model.writes[1]);
  expect(
    model.comments.filter((comment) => comment.body === text),
  ).toHaveLength(1);
  await editor.fill(text);
  await page
    .locator(".pull-comment-form")
    .getByRole("tab", { name: "Preview", exact: true })
    .click();
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.locator("#issuecomment-67")).toContainText(text);
  await expect(
    page
      .locator(".pull-comment-form")
      .getByRole("tab", { name: "Write", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  expect(model.writes[2].request_id).not.toBe(model.writes[0].request_id);
});

test("issue permalinks retain canonical edits when an older list page arrives", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/issues/1#issuecomment-1");
  const target = page.locator("#issuecomment-1");
  await expect(target).toBeFocused();
  await expect(target).toContainText("Conversation comment 1 (canonical)");
  expect(model.reads.filter((read) => read.includes("before="))).toHaveLength(
    0,
  );
  await page.getByRole("button", { name: "Load earlier comments" }).click();
  await page
    .getByRole("alert")
    .filter({ hasText: "History temporarily unavailable" })
    .getByRole("button", { name: "Try again" })
    .click();
  await page.getByRole("button", { name: "Load earlier comments" }).click();
  await expect(page.locator(".issue-comments article")).toHaveCount(65);
  await expect(target).toHaveCount(1);
  await expect(target).toContainText("Conversation comment 1 (canonical)");
  await expect(target).toContainText("edited");
  await expect(
    target.getByRole("link", { name: "Comment #1 permalink" }),
  ).toHaveAttribute("href", "/team/project/issues/1#issuecomment-1");
  await expectNoAccessibilityViolations(page);
});

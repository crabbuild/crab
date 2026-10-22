import { expect, test, type Page } from "@playwright/test";
import type {
  Comment,
  Review,
} from "../../../src/components/discussion/conversation/model";
import { mockPullRequest } from "./fixture";
import { conversationPage } from "../conversation";
import { emptyReactions } from "../fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

async function historyFixture(page: Page) {
  await mockPullRequest(page);
  const comments: Comment[] = Array.from({ length: 65 }, (_, index) => ({
    number: index + 1,
    author: { name: "Alice", profile: null },
    body: `Conversation comment ${index + 1}`,
    reactions: emptyReactions(),
    version: 1,
    created_at: 1_700_000_000_000 + index * 2000,
    updated_at: 1_700_000_000_000 + index * 2000,
    can_edit: true,
  }));
  const reviews: Review[] = comments.map((comment) => ({
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
  const submissions = new Map<string, Comment>();
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
    if (
      path === `${root}/pulls/1/conversation` ||
      path === `${root}/issues/1/conversation`
    ) {
      reads.push(`${path}${url.search}`);
      if (url.searchParams.has("before") && failEarlier) {
        failEarlier = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "History temporarily unavailable" } },
        });
      }
      const focus = url.searchParams.get("focus");
      const canonical = <T extends Comment>(items: T[]) =>
        items.map((item) =>
          focus ===
          `${path.includes("/pulls/") && "state" in item ? "review" : "comment"}:${item.number}`
            ? { ...item, body: `${item.body} (canonical)`, version: 2 }
            : item,
        );
      return conversationPage(route, {
        comments: canonical(comments),
        reviews: path.includes("/pulls/") ? canonical(reviews) : [],
        head: reviewHead,
      });
    }
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
      throw new Error(`Unexpected split history read: ${path}`);
    }
    if (method === "PATCH" && match[2] === "comments" && match[3]) {
      const item = comments.find((item) => item.number === Number(match[3]));
      if (!item) throw new Error("Missing edit source");
      const input = route.request().postDataJSON() as {
        body: string;
        version: number;
      };
      if (input.version !== item.version)
        return route.fulfill({
          status: 409,
          json: {
            error: { code: "conflict", message: "Newer content was saved" },
          },
        });
      Object.assign(item, {
        body: input.body,
        version: item.version + 1,
        updated_at: Date.now(),
      });
      return route.fulfill({ json: item });
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

test("one chronological history remains reachable after an earlier page fails", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1?powerbar=1");
  const timeline = page.locator(".pull-timeline"),
    cards = timeline.locator("article:visible");
  await expect(cards).toHaveCount(30);
  await page.getByRole("button", { name: "Load earlier activity" }).click();
  const failure = page
    .getByRole("alert")
    .filter({ hasText: "History temporarily unavailable" });
  await expect(failure).toBeVisible();
  await expect(cards).toHaveCount(30);
  await failure.getByRole("button", { name: "Try again" }).click();
  await expect(cards).toHaveCount(60);
  for (const count of [90, 120, 130]) {
    await page.getByRole("button", { name: "Load earlier activity" }).click();
    await expect(cards).toHaveCount(count);
  }
  await expect(
    page.getByRole("button", { name: "Load earlier activity" }),
  ).toHaveCount(0);
  expect(
    await cards.evaluateAll((nodes) => nodes.map((node) => node.id)),
  ).toEqual(
    Array.from({ length: 65 }, (_, i) => [
      `issuecomment-${i + 1}`,
      `pullrequestreview-${i + 1}`,
    ]).flat(),
  );
  expect(model.reads.filter((read) => read.includes("before="))).toHaveLength(
    5,
  );
  expect(model.reads.at(1)).toBe(model.reads.at(2));
  for (const [head, label] of [
    ["c".repeat(40), "Outdated"],
    [null, "Head unavailable"],
  ] as const) {
    model.head(head);
    await page.getByRole("button", { name: "Refresh conversation" }).click();
    await expect(cards).toHaveCount(30);
    await expect(
      timeline.locator(".review-outdated:visible").filter({ hasText: label }),
    ).toHaveCount(15);
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

test("review permalinks seek one native window, then page forward and survive back/reload", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1?powerbar=1");
  const link = page.getByRole("link", {
    name: "Open review #1 by Bob",
    exact: true,
  });
  await expect(link).toHaveAttribute(
    "href",
    "/team/project/pull/1#pullrequestreview-1",
  );
  await link.click();
  const target = page.locator("#pullrequestreview-1");
  await expect(page).toHaveURL(/\?powerbar=1#pullrequestreview-1$/);
  await expect(target).toBeFocused();
  await expect(target).toContainText("Submitted review 1 (canonical)");
  await expect(page.locator(".pull-timeline article:visible")).toHaveCount(2);
  expect(model.reads.filter((read) => read.includes("before="))).toHaveLength(
    0,
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload();
  await expect(target).toBeFocused();
  await expect(target).toBeInViewport();
  await page.getByRole("button", { name: "Load newer activity" }).click();
  await expect(page.locator(".pull-timeline article:visible")).toHaveCount(32);
  await page
    .getByRole("link", { name: "Comment #16 permalink", exact: true })
    .click();
  await expect(page.locator("#issuecomment-16")).toBeFocused();
  await page.goBack();
  await expect(target).toBeFocused();
  await page.goto("/team/project/pull/1#pullrequestreview-999");
  const failure = page
    .getByRole("alert")
    .filter({ hasText: "Contribution not found" });
  await expect(failure).toContainText("Unable to load conversation");
  await failure.getByRole("button", { name: "Try again" }).click();
  await expect(failure).toBeVisible();
});

test("comment drafts and retry identities survive tabs, metadata refresh and a lost response", async ({
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

test("issue focus and subsequent windows retain a newer canonical contribution", async ({
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
  for (const count of [31, 61, 65]) {
    await page.getByRole("button", { name: "Load newer activity" }).click();
    await expect(page.locator(".issue-comments article:visible")).toHaveCount(
      count,
    );
  }
  await expect(target).toHaveCount(1);
  await expect(target).toContainText("Conversation comment 1 (canonical)");
  await expect(target).toContainText("edited");
  await expect(
    target.getByRole("link", { name: "Comment #1 permalink" }),
  ).toHaveAttribute("href", "/team/project/issues/1#issuecomment-1");
  await expectNoAccessibilityViolations(page);
});

test("a pending card edit completes while another PR tab is visible", async ({
  page,
}) => {
  await historyFixture(page);
  await page.goto("/team/project/pull/1");
  const card = page.locator("#issuecomment-65");
  await card.getByRole("button", { name: "Edit comment" }).click();
  const editor = card.getByRole("textbox", {
    name: "Edit comment",
    exact: true,
  });
  await editor.fill("Edited while inspecting checks");
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route(
    "**/api/repos/team/project/pulls/1/comments/65?**",
    async (route) => {
      if (route.request().method() === "PATCH") await gate;
      await route.fallback();
    },
  );
  const saved = page.waitForResponse(
    (response) =>
      response.url().includes("/comments/65?") &&
      response.request().method() === "PATCH",
  );
  try {
    await card
      .getByRole("button", { name: "Save comment", exact: true })
      .click();
    await expect(editor).toBeDisabled();
    await page.getByRole("link", { name: "Checks", exact: true }).click();
  } finally {
    release();
  }
  await saved;
  await page.getByRole("link", { name: "Conversation", exact: true }).click();
  await expect(card).toContainText("Edited while inspecting checks");
  await expect(
    card.getByRole("button", { name: "Edit comment" }),
  ).toBeEnabled();
  await expect(card.getByRole("textbox")).toHaveCount(0);
});

test("posting from an old focused window offers an explicit link without changing location", async ({
  page,
}) => {
  await historyFixture(page);
  await page.goto("/team/project/pull/1#issuecomment-1");
  await expect(page.locator("#issuecomment-1")).toBeFocused();
  await page
    .getByRole("textbox", { name: "Comment", exact: true })
    .fill("New reply from an old window");
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Reply lost after saving",
  );
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Comment posted." }),
  ).toBeVisible();
  await expect(page).toHaveURL(/#issuecomment-1$/);
  await expect(page.locator(".pull-timeline article:visible")).toHaveCount(1);
  await page.getByRole("link", { name: "View comment", exact: true }).click();
  await expect(page.locator("#issuecomment-66")).toBeFocused();
});

test("a cached edit survives focused windows and requires explicit review of a newer version", async ({
  page,
}) => {
  const model = await historyFixture(page);
  await page.goto("/team/project/pull/1");
  const card = page.locator("#issuecomment-65");
  await card.getByRole("button", { name: "Edit comment" }).click();
  const editor = card.getByRole("textbox", {
    name: "Edit comment",
    exact: true,
  });
  await editor.fill("My draft across history windows");
  await page
    .getByRole("link", { name: "Open review #1 by Bob", exact: true })
    .click();
  await expect(page.locator("#pullrequestreview-1")).toBeFocused();
  await expect(card).not.toBeVisible();
  await page.goBack();
  await expect(editor).toHaveValue("My draft across history windows");
  Object.assign(model.comments[64], { body: "A concurrent edit", version: 2 });
  await page.getByRole("button", { name: "Refresh conversation" }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Loading conversation" }),
  ).toHaveCount(0);
  await expect(editor).toHaveValue("My draft across history windows");
  const save = card.getByRole("button", { name: "Save comment", exact: true });
  await save.click();
  const conflict = card.getByRole("region", { name: "Review newer content" });
  await expect(conflict).toContainText("A concurrent edit");
  await expect(save).toBeDisabled();
  await expect(editor).toHaveValue("My draft across history windows");
  await conflict
    .getByRole("button", { name: "Continue with my draft" })
    .click();
  await save.click();
  await expect(card).toContainText("My draft across history windows");
  await expect(card.getByRole("textbox")).toHaveCount(0);
  expect(model.comments[64].version).toBe(3);
});

test("hidden conversation portals do not cover another PR tab", async ({
  page,
}) => {
  await historyFixture(page);
  await page.goto("/team/project/pull/1/checks");
  for (const selector of [".pull-conversation > article", "#issuecomment-65"]) {
    await page.getByRole("link", { name: "Conversation", exact: true }).click();
    await page
      .locator(selector)
      .getByRole("button", { name: "Add reaction", exact: true })
      .click();
    await expect(page.getByRole("menu")).toBeVisible();
    await page.goBack();
    await expect(
      page.getByRole("heading", { name: "ci/test", exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(
      page.locator('[data-slot="dropdown-menu-content"]'),
    ).toHaveCount(0);
    await expect(
      page.getByRole("link", { name: "Files changed", exact: true }),
    ).toBeVisible();
  }
});

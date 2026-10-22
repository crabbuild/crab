import { expect, test, type Page } from "@playwright/test";
import type { Comment } from "@/components/discussion/conversation/model";
import type { Issue } from "@/features/issues/model";
import { mockPullRequest } from "./pulls/fixture";
import { conversationPage } from "./conversation";
import { emptyReactions } from "./fixtures";
import { expectNoAccessibilityViolations } from "./accessibility";

async function fixture(page: Page, kind: "issues" | "pulls") {
  const pull = await mockPullRequest(page);
  const root = `/api/repos/team/project/${kind}/1`;
  let issue: Issue = {
    number: 1,
    title: "Atomic state action",
    body: "Description",
    state: "open",
    state_reason: null,
    closed_at: null,
    version: 1,
    author: { name: "Alice", profile: null },
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
  const comments: Comment[] = [];
  const submissions = new Map<string, Comment | null>();
  const writes: Array<{
    request_id: string;
    version: number;
    state: "open" | "closed";
    body?: string;
    reason?: "completed" | "not_planned";
  }> = [];
  let lose: number | false = false,
    reject = false;
  await page.route(
    (url) => url.pathname === `${root}/state`,
    async (route) => {
      const input = route.request().postDataJSON();
      writes.push(input);
      if (reject) {
        reject = false;
        return route.fulfill({
          status: 409,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: {
            error: { code: "conflict", message: "A newer version was saved" },
          },
        });
      }
      let comment = submissions.get(input.request_id);
      if (comment === undefined) {
        expect(input.version).toBe(issue.version);
        comment = input.body
          ? {
              number: comments.length + 1,
              body: input.body,
              author: issue.author,
              version: 1,
              created_at: 1700000001000,
              updated_at: 1700000001000,
              can_edit: true,
              reactions: emptyReactions(),
            }
          : null;
        if (comment) comments.push(comment);
        submissions.set(input.request_id, comment);
        issue = {
          ...issue,
          state: input.state,
          state_reason:
            input.state === "closed"
              ? (input.reason ?? "completed")
              : "reopened",
          closed_at: input.state === "closed" ? 1700000001000 : null,
          version: issue.version + 1,
        };
        pull.lifecycle.state = input.state;
      }
      if (lose) {
        const status = lose;
        lose = false;
        return route.fulfill({
          status,
          json: { error: { message: "Reply lost after the action committed" } },
        });
      }
      return route.fulfill({ json: { comment } });
    },
  );
  if (kind === "issues")
    await page.route(
      (url) => url.pathname === root,
      (route) => route.fulfill({ json: issue }),
    );
  await page.route(
    (url) => url.pathname === `${root}/conversation`,
    (route) => conversationPage(route, { comments }),
  );
  await page.goto(`/team/project/${kind === "pulls" ? "pull" : "issues"}/1`);
  await expect(
    page.getByRole("textbox", { name: "Comment", exact: true }),
  ).toBeVisible();
  return {
    comments,
    writes,
    lose(status = 503) {
      lose = status;
    },
    reject() {
      reject = true;
    },
  };
}
for (const kind of ["issues", "pulls"] as const) {
  test(`${kind}: close with comment commits once and clears only the submitted draft`, async ({
    page,
  }) => {
    const model = await fixture(page, kind);
    const editor = page.getByRole("textbox", { name: "Comment", exact: true });
    await editor.fill("Closing with context");
    await page
      .getByRole("button", { name: "Close with comment", exact: true })
      .click();
    await expect(
      page.locator(kind === "issues" ? ".issue-state" : ".pull-state"),
    ).toHaveText("Closed");
    await expect(editor).toHaveValue("");
    await expect(page.locator("#issuecomment-1")).toContainText(
      "Closing with context",
    );
    expect(model.writes).toHaveLength(1);
    expect(model.comments).toHaveLength(1);
    await expectNoAccessibilityViolations(page);
  });
  test(`${kind}: uncertain actions retry the original ID, version and text through refresh or tabs`, async ({
    page,
  }) => {
    const model = await fixture(page, kind);
    const editor = page.getByRole("textbox", { name: "Comment", exact: true });
    await editor.fill("Original closing context");
    model.lose();
    await page
      .getByRole("button", { name: "Close with comment", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Retry original action" }),
    ).toBeVisible();
    await editor.fill("A newer unsent draft");
    if (kind === "pulls") {
      await page.getByRole("link", { name: "Checks", exact: true }).click();
      await page
        .getByRole("link", { name: "Conversation", exact: true })
        .click();
    } else {
      await page.getByRole("button", { name: "Refresh", exact: true }).click();
      await expect(page.locator(".issue-state")).toHaveText("Closed");
    }
    await page.getByRole("button", { name: "Retry original action" }).click();
    await expect(
      page.getByRole("button", { name: "Retry original action" }),
    ).toHaveCount(0);
    await expect(editor).toHaveValue("A newer unsent draft");
    expect(model.writes).toHaveLength(2);
    expect(model.writes[1]).toEqual(model.writes[0]);
    expect(model.comments).toHaveLength(1);
    await expect(page.locator("#issuecomment-1")).toContainText(
      "Original closing context",
    );
  });
}
test("rejected issue actions retain drafts and explicit refresh permits a revised action", async ({
  page,
}) => {
  const model = await fixture(page, "issues");
  const editor = page.getByRole("textbox", { name: "Comment", exact: true });
  await editor.fill("Keep this context");
  model.reject();
  await page
    .getByRole("button", { name: "Close with comment", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "A newer version was saved",
  );
  await expect(editor).toHaveValue("Keep this context");
  expect(model.comments).toHaveLength(0);
  await page
    .getByRole("button", { name: "Refresh issue before retrying" })
    .click();
  await page
    .getByRole("button", { name: "Close with comment", exact: true })
    .click();
  await expect(editor).toHaveValue("");
  expect(model.comments).toHaveLength(1);
});

test("a permission error after commit retains the original state intent", async ({
  page,
}) => {
  const model = await fixture(page, "issues");
  const editor = page.getByRole("textbox", { name: "Comment", exact: true });
  await editor.fill("Saved before permission changed");
  model.lose(403);
  await page
    .getByRole("button", { name: "Close with comment", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry original action" }),
  ).toBeVisible();
  await editor.fill("Newer private draft");
  await page.getByRole("button", { name: "Retry original action" }).click();
  await expect(
    page.getByRole("button", { name: "Retry original action" }),
  ).toHaveCount(0);
  expect(model.writes[1]).toEqual(model.writes[0]);
  expect(model.comments).toHaveLength(1);
  await expect(editor).toHaveValue("Newer private draft");
});

test("a delayed menu focus return preserves a newer editor focus", async ({
  page,
}) => {
  const now = new Date("2026-10-22T12:00:00Z");
  await page.clock.install({ time: now });
  await fixture(page, "issues");
  await page.getByRole("button", { name: "Choose close reason" }).click();
  await page.clock.pauseAt(
    await page.evaluate(() => new Date(Date.now() + 100).toISOString()),
  );
  await page.getByRole("menuitemradio", { name: /Not planned/ }).click();
  await expect(page.getByRole("menu")).toHaveCount(0);
  const editor = page.getByRole("textbox", { name: "Comment", exact: true });
  await editor.fill("Keep typing here");
  await expect(editor).toBeFocused();
  await page.clock.runFor(1);
  await expect(editor).toBeFocused();
  await page.keyboard.type("!");
  await expect(editor).toHaveValue("Keep typing here!");
});

for (const [name, body] of [
  ["UTF-8 body", "界".repeat(23000)],
  ["encoded envelope", "\\".repeat(50000) + "context"],
])
  test(`an oversized ${name} can be revised before creating a state intent`, async ({
    page,
  }) => {
    const model = await fixture(page, "issues");
    const editor = page.getByRole("textbox", { name: "Comment", exact: true });
    await editor.fill(body);
    await page
      .getByRole("button", { name: "Close with comment", exact: true })
      .click();
    await expect(page.getByRole("alert")).toContainText(
      "This comment is too long",
    );
    expect(model.writes).toHaveLength(0);
    await editor.fill("Fits the request");
    await page
      .getByRole("button", { name: "Close with comment", exact: true })
      .click();
    await expect(editor).toHaveValue("");
    expect(model.writes).toHaveLength(1);
  });

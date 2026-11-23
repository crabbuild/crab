import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

test("pull creation and review navigation preserve GitHub-style controls", async ({
  page,
}) => {
  const { react } = await mockPullRequest(page);
  await page.goto("/team/project/pulls");
  await expect(
    page.getByRole("link", { name: "Improve the README", exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  const search = page.getByRole("search", { name: "Search pulls" });
  await search.getByRole("combobox").fill("missing workflow");
  await search.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/q=missing\+workflow/);
  await expect(
    page.getByRole("heading", {
      name: "No pulls match “missing workflow”",
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "0 results", exact: true }),
  ).toBeVisible();
  await search.getByRole("button", { name: "Clear", exact: true }).click();
  await expect(page).not.toHaveURL(/q=/);
  await expect(
    page.getByRole("link", { name: "Improve the README", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "New pull request", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Compare changes" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await expect(page.getByLabel("base:")).toHaveValue("refs/heads/main");
  await expect(page.getByLabel("compare:")).toHaveValue(
    "refs/heads/feature/docs",
  );
  await page.getByLabel("Title", { exact: true }).fill("Document the feature");
  const pullForm = page.getByRole("form", { name: "Create discussion" });
  const description = pullForm.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await description.fill("This explains the new behavior.");
  await description.evaluate((input: HTMLTextAreaElement) =>
    input.setSelectionRange(18, 30),
  );
  await pullForm
    .getByRole("button", { name: "Add bold text", exact: true })
    .click();
  await expect(description).toHaveValue("This explains the **new behavior**.");
  await pullForm.getByRole("tab", { name: "Preview", exact: true }).click();
  await expect(
    pullForm.getByRole("tabpanel", { name: "Preview", exact: true }),
  ).toContainText("This explains the new behavior.");
  await page
    .getByRole("button", { name: "Create pull request", exact: true })
    .click();
  await expect(page).toHaveURL(/\/pull\/2$/);
  await expect(
    page.getByRole("heading", { name: /Document the feature #2/ }),
  ).toBeVisible();
  await expect(page.locator(".pull-summary")).toContainText(
    "feature/docs into main",
  );
  await react(page.locator(".pull-conversation > article.discussion-card"));
  await page.reload();
  await expect(
    page
      .locator(".pull-conversation > article.discussion-card")
      .getByRole("button", { name: "Heart: 1 reaction", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator(".timing")).toHaveCount(0);
  await expect(page.locator(".timing-widget")).toHaveCount(0);
  const mergePosition = await page.locator(".pull-merge-note").boundingBox();
  const commentPosition = await page
    .locator(".pull-comment-form")
    .boundingBox();
  expect(mergePosition!.y + mergePosition!.height).toBeLessThan(
    commentPosition!.y,
  );
  await page.goto("/team/project/pull/2?powerbar=1");
  for (const component of ["Pull request", "Conversation"]) {
    const timing = page.locator(
      `details[aria-label="${component} request timing"]`,
    );
    await expect(timing.locator("summary")).toContainText(/\d+\.\d ms/);
    await expect(timing.locator("..")).toHaveClass("timing-widget");
    await timing.locator("summary").click();
    await expect(timing.locator("code").first()).toContainText(
      "/api/repos/team/project/pulls/2",
    );
  }
  await expect(
    page.locator('details[aria-label="Pull request request timing"]'),
  ).toContainText("handler;dur=12.5");
  const pullWidget = page.getByRole("group", {
    name: "Pull request request widget",
    exact: true,
  });
  await expect(
    pullWidget.getByRole("heading", { name: /Document the feature #2/ }),
  ).toBeVisible();
  await expect(pullWidget.locator(".pull-merge-note")).toBeVisible();
  await expect(
    pullWidget.getByRole("textbox", { name: "Comment", exact: true }),
  ).toBeVisible();
  const widgetBounds = (await pullWidget.boundingBox())!;
  const mergeBounds = (await pullWidget
    .locator(".pull-merge-note")
    .boundingBox())!;
  expect(mergeBounds.y + mergeBounds.height).toBeLessThanOrEqual(
    widgetBounds.y + widgetBounds.height,
  );
  await expect(page.locator(".pull-merge-note")).toContainText(
    "main is protected",
  );
  await expect(page.locator(".pull-merge-note")).toContainText(
    "1 more approving review is required",
  );
  await expect(page.locator(".required-checks")).toContainText(
    "Required checks are waiting",
  );
  await expect(page.locator(".required-checks")).toContainText(
    "Expected — Waiting for status to be reported.",
  );
  await expect(
    page.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toHaveCount(0);
  await expectNoAccessibilityViolations(page);

  await page.getByRole("link", { name: "Checks", exact: true }).click();
  await expect(page).toHaveURL(/powerbar=1/);
  await page.goto(page.url().replace("?powerbar=1", "?powerbar=0"));
  await expect(page.locator(".timing")).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "ci/test" })).toBeVisible();
  await expect(page.locator(".check-run-content")).toContainText(
    "The required test suite passed.",
  );
  await expect(page.locator(".check-annotations")).toContainText(
    "src/lib.rs:42–44",
  );
  await page.getByText("Build and test", { exact: true }).click();
  await expect(page.locator(".check-steps pre")).toContainText(
    "<script>alert(1)</script>",
  );
  await expect(page.locator(".check-steps script")).toHaveCount(0);
  await expectNoAccessibilityViolations(page);

  await page
    .getByRole("navigation", { name: "Pull request" })
    .getByRole("link", { name: "Commits", exact: true })
    .click();
  await expect(
    page.getByRole("link", { name: "Document remote browsing", exact: true }),
  ).toBeVisible();
  await expect(page.locator(".pull-commits .commit-list")).toContainText(
    "Alice committed",
  );
  await expect(page.locator(".pull-commits .commit-list")).not.toContainText(
    "Explain the workflow",
  );
  await expectNoAccessibilityViolations(page);
});

for (const { address, missing, replacement } of [
  {
    address: "missing...feature%2Fdocs",
    missing: "base:",
    replacement: "refs/heads/main",
  },
  {
    address: "main...missing",
    missing: "compare:",
    replacement: "refs/heads/feature/docs",
  },
]) {
  test(`an unavailable ${missing.slice(0, -1)} branch needs an explicit replacement`, async ({
    page,
  }) => {
    await mockPullRequest(page);
    await page.goto(`/team/project/compare/${address}`);

    await expect(
      page.getByRole("heading", {
        name: "A branch in this comparison is unavailable",
      }),
    ).toBeVisible();
    const selector = page.getByLabel(missing);
    await expect(selector).toHaveValue("");
    await expect(
      page.getByRole("button", { name: "Create pull request", exact: true }),
    ).toHaveCount(0);

    await selector.selectOption(replacement);
    await expect(
      page.getByText("1 changed file", { exact: true }),
    ).toBeVisible();
  });
}

test("a rejected stale comparison reloads before submitting a fresh pull request", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  await page.goto("/team/project/compare/main...feature%2Fdocs");
  const title = page.getByRole("textbox", { name: "Title", exact: true });
  const description = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  const submit = page.getByRole("button", {
    name: "Create pull request",
    exact: true,
  });
  await title.fill("Keep this draft");
  await description.fill("Discuss the current branch.");
  await expect(submit).toBeEnabled();

  const { base_oid, head_oid } = fixture.pull();
  fixture.lifecycle.head = "e".repeat(40);
  await submit.click();
  await expect(page.getByRole("alert")).toContainText("A branch moved");
  await expect(submit).toBeDisabled();
  await page
    .getByRole("button", { name: "Review inputs", exact: true })
    .click();
  await expect(title).toHaveValue("Keep this draft");
  await expect(description).toHaveValue("Discuss the current branch.");
  await submit.click();

  await expect(page).toHaveURL(/\/pull\/2$/);
  const [initial, retried] = fixture.pullSubmissions;
  expect(fixture.pullSubmissions).toHaveLength(2);
  expect(initial.expected).toEqual({ base_oid, head_oid });
  expect(retried).toEqual({
    ...initial,
    request_id: retried.request_id,
    expected: { base_oid, head_oid: fixture.lifecycle.head },
  });
  expect(retried.request_id).not.toBe(initial.request_id);
});

test("viewed files survive reload and clear when the file changes", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const path = "1".repeat(64);
  let revision = "2".repeat(64);
  let saved = { version: 0, files: {} as Record<string, string> };
  await page.route("**/api/repos/team/project/changes?**", (route) => {
    const { base_oid, head_oid } = fixture.pull();
    return route.fulfill({
      json: {
        base: fixture.mergeBase,
        base_tip: base_oid,
        commit: head_oid,
        changes: [
          {
            path: "README.md",
            path_hex: "524541444d452e6d64",
            kind: "Modified",
            old: null,
            new: null,
            view_id: path + revision,
          },
        ],
      },
    });
  });
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/viewed",
    (route) => {
      if (route.request().method() === "PUT") {
        expect(route.request().postDataJSON()).toEqual({
          version: saved.version,
          file: path + revision,
          viewed: true,
        });
        saved = { version: saved.version + 1, files: { [path]: revision } };
      }
      return route.fulfill({ json: saved });
    },
  );
  await page.goto("/team/project/pull/1/files");
  const viewed = page.getByRole("checkbox", { name: "Viewed README.md" });
  await expect(viewed).toBeVisible();
  await expect(viewed).not.toBeChecked();
  await viewed.check();
  await expect(page.getByText("1 of 1 files viewed")).toBeVisible();
  const filter = page.getByRole("searchbox", { name: "Filter changed files" });
  const hideViewed = page.getByRole("checkbox", { name: "Hide viewed" });
  await filter.fill("README");
  await hideViewed.check();
  await expect(
    page.getByText("No changed files match these filters."),
  ).toBeVisible();
  await expect(page.locator(".diff-panel")).toHaveCount(0);
  await page.getByRole("button", { name: "Jump to diff", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Open review thread #1" }),
  ).toBeFocused();
  await expect(hideViewed).toBeChecked();
  await expect(filter).toHaveValue("README");
  await page.setViewportSize({ width: 390, height: 844 });
  await filter.fill("missing");
  await expect(page.getByText("0 of 1 files shown")).toBeVisible();
  await page.getByRole("button", { name: "Clear filters" }).click();
  await expect(hideViewed).not.toBeChecked();
  await expect(viewed).toBeChecked();
  await page.reload();
  await expect(viewed).toBeChecked();
  revision = "3".repeat(64);
  fixture.lifecycle.head = "e".repeat(40);
  await page.reload();
  await expect(viewed).not.toBeChecked();
  await expect(page.getByText("0 of 1 files viewed")).toBeVisible();
});

test("pending review comments survive reload and retry the same submission", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const resource = "/api/repos/team/project/pulls/1/pending-review";
  const comments: Record<string, unknown>[] = [];
  let version: string | null = null;
  const submissions: object[] = [];
  await page.route(
    (url) => url.pathname === resource,
    (route) => {
      if (route.request().method() === "PUT") {
        const input = route.request().postDataJSON();
        expect(input.expected).toBe(version);
        expect(input.edit.action).toBe("save");
        expect(input.edit.comment).toMatchObject({
          base_oid: fixture.mergeBase,
          head_oid: fixture.pull().head_oid,
          path_hex: "524541444d452e6d64",
          side: "new",
          start_line: 1,
          end_line: 1,
        });
        version = input.request_id;
        const next = {
          ...input.edit.comment,
          id: input.edit.replace ?? input.request_id,
          path: "README.md",
        };
        if (input.edit.replace) {
          expect(input.edit.replace).toBe(comments[0].id);
          comments[0] = next;
        } else {
          comments.push(next);
        }
      }
      return route.fulfill({ json: { version, comments } });
    },
  );
  await page.route(
    (url) => url.pathname === `${resource}/submit`,
    (route) => {
      const input = route.request().postDataJSON();
      submissions.push(input);
      if (submissions.length === 1) {
        expect(input).toMatchObject({
          expected: version,
          body: "",
          state: "commented",
        });
        expect(comments[0].body).toBe("Edited private comment");
        version = input.request_id;
        comments.length = 0;
        return route.abort("failed");
      }
      expect(input).toEqual(submissions[0]);
      return route.fulfill({ json: { review: 1, threads: [2] } });
    },
  );
  await page.goto("/team/project/pull/1/files");
  await page
    .locator(
      '.diff-panel [data-column-number="1"][data-line-type="change-addition"]',
    )
    .click();
  const inline = page.getByRole("form", { name: "Comment on selected lines" });
  await inline
    .getByRole("textbox", { name: "Comment", exact: true })
    .fill("Private draft comment");
  await inline.getByRole("button", { name: "Add to review" }).click();
  const pending = page.getByRole("region", { name: "Pending review comments" });
  await expect(pending.getByText("Private draft comment")).toBeVisible();
  await expect(page.locator(".inline-review-list")).not.toContainText(
    "Private draft comment",
  );
  await page.reload();
  await expect(pending.getByText("Private draft comment")).toBeVisible();
  await pending.getByRole("button", { name: "Edit", exact: true }).click();
  await pending
    .getByRole("textbox", { name: "Pending comment", exact: true })
    .fill("Edited private comment");
  await pending.getByRole("button", { name: "Save comment" }).click();
  await expect(pending.getByText("Edited private comment")).toBeVisible();

  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Submit review", exact: true })
    .click();
  await expect(
    page.getByText("The response was lost.", { exact: false }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Retry review submission" }).click();
  await expect(page).toHaveURL(/\/pull\/1$/);
  await page.getByRole("link", { name: "Files changed" }).click();
  await expect(pending).toHaveCount(0);
  expect(submissions).toHaveLength(2);
});

test("selected suggestions commit together on the inspected head", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  fixture.threads.push({
    ...fixture.threads[0],
    number: 2,
    body: "Another suggestion",
    suggested_text: "Replace line two",
    start_line: 2,
    end_line: 2,
  });
  let commits = 0;
  await page.route(
    (url) => url.pathname.endsWith("/pulls/1/suggestions"),
    (route) => {
      expect(route.request().postDataJSON()).toEqual({
        expected_head: fixture.pull().head_oid,
        message: "Polish the reviewed files",
        threads: [
          { number: 1, version: 1 },
          { number: 2, version: 1 },
        ],
      });
      commits += 1;
      for (const thread of fixture.threads) thread.outdated = true;
      return route.fulfill({ json: { commit: "d".repeat(40) } });
    },
  );
  await page.goto("/team/project/pull/1/files");
  await page
    .getByRole("checkbox", { name: "Select suggestion from thread #1" })
    .check();
  await page
    .getByRole("checkbox", { name: "Select suggestion from thread #2" })
    .check();
  await page
    .getByRole("button", { name: "Commit suggestions (2)", exact: true })
    .click();
  const commitDialog = page.getByRole("dialog", {
    name: "Commit 2 suggestions",
  });
  await expect(commitDialog).toContainText("team/project");
  await commitDialog
    .getByLabel("Commit message")
    .fill("Polish the reviewed files");
  await commitDialog.getByRole("button", { name: "Commit changes" }).click();
  await expect(page.getByText("Committed suggestions in")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Commit suggestions", exact: false }),
  ).toHaveCount(0);
  expect(commits).toBe(1);
});

test("review conversations and ambiguous merge recovery preserve the pull workflow", async ({
  page,
}) => {
  const { react, reactionWrites, lifecycle } = await mockPullRequest(page);
  await page.goto("/team/project/compare/main...feature%2Fdocs");
  await page.getByLabel("Title", { exact: true }).fill("Document the feature");
  await page
    .getByRole("textbox", { name: "Description", exact: true })
    .fill("This explains the **new behavior**.");
  await page
    .getByRole("button", { name: "Create pull request", exact: true })
    .click();
  await react(page.locator(".pull-conversation > article.discussion-card"));
  await page.getByRole("link", { name: "Files changed", exact: true }).click();
  await expect(page.getByText("1 changed file", { exact: true })).toBeVisible();
  const changedFiles = page.locator(
    'file-tree-container[aria-label="Changed files"]',
  );
  await expect(
    changedFiles.getByRole("treeitem", { name: "README.md", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(
    page.getByText("Review workspace", { exact: true }),
  ).toBeVisible();
  await expect(page.locator(".change-tree-header")).toContainText("1 file");
  await expect(page.locator(".diff-panel")).toContainText("New content");
  await expect(
    page.getByRole("button", { name: "Open review thread #1" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Jump to diff", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Jump to diff", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Open review thread #1" }),
  ).toBeFocused();
  await expect(
    page.getByRole("button", { name: "Apply suggestion" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Light", exact: true }).click();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await react(page.locator(".review-thread-card"));
  await page.getByRole("button", { name: "Apply suggestion" }).click();
  await page
    .getByRole("dialog", { name: "Commit suggestion", exact: true })
    .getByRole("button", { name: "Commit changes" })
    .click();
  await page.locator(".inline-review-outdated > summary").click();
  await expect(page.locator(".inline-review-outdated")).toContainText(
    "Outdated conversations (1)",
  );
  await expect(page.locator(".review-thread-card")).toContainText(
    "Consider a clearer opening.",
  );
  await page
    .locator(".review-thread-card")
    .getByRole("button", { name: "Resolve", exact: true })
    .click();
  await expect(page.locator(".review-thread-card")).toContainText("Resolved");
  await expect(
    page.locator(".review-thread-card .review-thread-details"),
  ).not.toHaveAttribute("open", "");
  await page
    .locator(".inline-review-outdated")
    .evaluate((node) => ((node as HTMLDetailsElement).open = true));
  await page.locator(".review-thread-card .review-thread-summary").click();
  await page
    .locator(".review-thread-card")
    .getByRole("textbox", { name: "Reply", exact: true })
    .fill("I will update the wording.");
  await page
    .locator(".review-thread-card")
    .getByRole("button", { name: "Reply", exact: true })
    .click();
  await expect(page.locator(".review-thread-card")).toContainText(
    "I will update the wording.",
  );
  await react(page.locator(".review-reply"));
  await expectNoAccessibilityViolations(page);

  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Review summary", exact: true })
    .fill("Ready to merge.");
  await page.getByLabel("Approve").check();
  await page.getByRole("button", { name: "Submit review" }).click();
  await expect(page.locator(".review-event")).toContainText(
    "Bob approved these changes",
  );
  await expect(page.locator(".required-checks")).toContainText(
    "All required checks have passed",
  );
  await expect(
    page.getByRole("button", { name: "Merge pull request", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Select merge method", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("textbox", { name: "Comment", exact: true })
    .fill("Verified in the browser.");
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.locator(".discussion-thread")).toContainText(
    "Verified in the browser.",
  );
  await react(page.locator(".review-event"));
  await react(page.locator(".discussion-thread article.discussion-card"));
  expect(reactionWrites).toEqual([
    "pulls/2",
    "pulls/2/threads/1",
    "pulls/2/threads/1/replies/1",
    "pulls/2/reviews/1",
    "pulls/2/comments/1",
  ]);
  await page
    .getByRole("button", { name: "Close pull request", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Closed");

  lifecycle.branchesAvailable = false;
  await page.reload();
  await expect(
    page.getByText(/original commit IDs remain recorded/),
  ).toBeVisible();
  await expect(page.locator(".pull-conversation")).toContainText(
    "Verified in the browser.",
  );
  await expect(page.locator(".review-event")).toContainText("Head unavailable");

  lifecycle.branchesAvailable = true;
  lifecycle.state = "open";
  await page.reload();
  await page
    .getByRole("button", { name: "Merge pull request", exact: true })
    .click();
  await expect(
    page.getByRole("textbox", { name: "Merge commit message" }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Confirm merge", exact: true }),
  ).toHaveCount(0);
  await page
    .getByRole("button", { name: "Merge pull request", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Confirm merge", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "The merge may have completed",
  );
  lifecycle.branchesAvailable = false;
  await page.reload();
  await expect(
    page.getByRole("button", { name: "Retry merge", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Retry merge", exact: true }).click();
  await expect(page.locator(".pull-state")).toHaveText("Merged");
  await expect(page.locator(".pull-merge-note")).toContainText(
    "Local operator created merge commit",
  );

  await page.setViewportSize({ width: 360, height: 800 });
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
});

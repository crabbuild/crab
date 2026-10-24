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
    page.getByRole("button", { name: "Create a merge commit", exact: true }),
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

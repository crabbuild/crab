import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const base = "a".repeat(40);
const head = "b".repeat(40);
const pathHex = "README.md"
  .split("")
  .map((character) => character.charCodeAt(0).toString(16).padStart(2, "0"))
  .join("");
const encodePath = (path: string) =>
  Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");

test("pull request creation, discussion, and files follow the GitHub review flow", async ({
  page,
}) => {
  let state: "open" | "closed" | "merged" = "open";
  let created = false;
  let branchesAvailable = true;
  let mergePending = false;
  let mergeRequest = "";
  let mergeMethod: "fast_forward" | "merge_commit" = "merge_commit";
  let mergeMessage = "";
  let checkState: "success" | null = null;
  const comments: Array<Record<string, unknown>> = [];
  const reviews: Array<Record<string, unknown>> = [];
  const threads: Array<Record<string, unknown>> = [
    {
      number: 1,
      pull: 2,
      path: "README.md",
      path_hex: pathHex,
      side: "new",
      start_line: 1,
      end_line: 1,
      author: "Bob",
      body: "Consider a clearer opening.",
      suggested_text: "Applied content",
      base_oid: base,
      head_oid: head,
      old_blob_oid: "c".repeat(40),
      new_blob_oid: "d".repeat(40),
      resolved: false,
      outdated: false,
      vanished: false,
      resolver: null,
      current: true,
      version: 1,
      created_at: 1_700_000_060_000,
      updated_at: 1_700_000_060_000,
      can_edit: true,
      can_reply: true,
      can_resolve: true,
    },
  ];
  const replies: Array<Record<string, unknown>> = [];
  const pull = () => {
    const approvals = branchesAvailable && reviews.length ? 1 : 0;
    return {
      number: created ? 2 : 1,
      title: created ? "Document the feature" : "Improve the README",
      body: created ? "This explains the **new behavior**." : "Please review.",
      state,
      author: "Alice",
      base_ref: "refs/heads/main",
      base_oid: base,
      head_ref: "refs/heads/feature/docs",
      head_oid: head,
      original_base_oid: base,
      original_head_oid: head,
      version: state === "open" ? 1 : 2,
      created_at: 1_700_000_000_000,
      updated_at: 1_700_000_000_000,
      can_edit: true,
      can_manage: !mergePending,
      can_decide: true,
      can_merge:
        state === "open" &&
        (mergePending ||
          (branchesAvailable && approvals >= 1 && checkState === "success")),
      branches_available: state === "merged" || branchesAvailable,
      merge_requirements: {
        protected: true,
        required_approvals: 1,
        approvals,
        changes_requested: 0,
        checks_satisfied: checkState === "success",
        checks: [
          {
            context: "ci/test",
            state: checkState,
            description:
              checkState === "success" ? "Tests passed in 42s" : null,
            target_url:
              checkState === "success" ? "https://ci.example.test/42" : null,
            author: checkState === "success" ? "CI service" : null,
            updated_at: checkState === "success" ? 1_700_000_040_000 : null,
            run_id: checkState === "success" ? 7 : null,
          },
        ],
        satisfied: approvals >= 1 && checkState === "success",
      },
      merge:
        state === "merged"
          ? {
              author: "Local operator",
              method: mergeMethod,
              commit_oid: head,
              message: mergeMessage,
              created_at: 1_700_000_200_000,
            }
          : null,
      merge_pending: mergePending
        ? {
            request_id: mergeRequest,
            author: "Local operator",
            method: mergeMethod,
            pull_version: 1,
            base_oid: base,
            head_oid: head,
            message: mergeMessage,
            created_at: 1_700_000_150_000,
          }
        : null,
    };
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A repository for our team.",
              access: "write",
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [
                {
                  branch: "main",
                  required_approvals: 1,
                  required_checks: ["ci/test"],
                },
              ],
            },
          ],
        },
      });
    if (path.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: base },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid: base },
            { name: "refs/heads/feature/docs", oid: head },
          ],
          generation: 2,
        },
      });
    if (path.endsWith("/changes"))
      return route.fulfill({
        json: {
          base,
          commit: head,
          changes: [
            {
              path: "README.md",
              path_hex: pathHex,
              kind: "Modified",
              old: {
                path: "README.md",
                path_hex: pathHex,
                kind: "Blob",
                oid: "c".repeat(40),
                mode: "100644",
              },
              new: {
                path: "README.md",
                path_hex: pathHex,
                kind: "Blob",
                oid: "d".repeat(40),
                mode: "100644",
              },
            },
          ],
        },
      });
    if (path.endsWith("/diff"))
      return route.fulfill({
        json: {
          base,
          commit: head,
          path: "README.md",
          old: {
            oid: "c".repeat(40),
            size: 11,
            mode: "100644",
            classification: "OrdinaryGit",
            text: "Old content\n",
          },
          new: {
            oid: "d".repeat(40),
            size: 11,
            mode: "100644",
            classification: "OrdinaryGit",
            text: "New content\n",
          },
        },
      });
    if (path === "/api/repos/team/project/file") {
      expect(url.searchParams.get("rev")).toBe(head);
      expect(url.searchParams.get("path_hex")).toBe(pathHex);
      return route.fulfill({
        json: {
          oid: "d".repeat(40),
          size: 12,
          mode: "100644",
          classification: "OrdinaryGit",
          text: "New content\n",
          text_truncated: false,
        },
      });
    }
    if (
      path === "/api/repos/team/project/contents" &&
      request.method() === "PATCH"
    ) {
      const input = request.postDataJSON();
      expect(input.branch).toBe("refs/heads/feature/docs");
      expect(input.expected_head).toBe(head);
      expect(input.expected_blob).toBe("d".repeat(40));
      expect(input.path_hex).toBe(pathHex);
      expect(input.content).toBe("Applied content\n");
      expect(input.message).toBe("Apply suggestion from review thread #1");
      threads[0].outdated = true;
      return route.fulfill({
        json: { commit: "e".repeat(40), path_hex: pathHex },
      });
    }
    if (path === "/api/repos/team/project/commits") {
      expect(url.searchParams.get("rev")).toBe(head);
      expect(url.searchParams.get("base")).toBe(base);
      expect(url.searchParams.get("limit")).toBe("50");
      return route.fulfill({
        json: {
          items: [
            {
              oid: head,
              tree: "e".repeat(40),
              parents: [base],
              author: "Alice",
              author_seconds: 1_700_000_040,
              message: "Document remote browsing\n\nExplain the workflow.",
            },
          ],
          next: null,
        },
      });
    }
    if (path === `/api/repos/team/project/commits/${head}/check-runs`) {
      expect(url.searchParams.get("limit")).toBe("50");
      return route.fulfill({
        json: {
          sha: head,
          items: [
            {
              id: 7,
              head_sha: head,
              name: "ci/test",
              status: "completed",
              conclusion: "success",
              details_url: "https://ci.example.test/runs/7",
              output_title: "All tests passed",
              author: "CI service",
              version: 3,
              started_at: 1_700_000_020_000,
              completed_at: 1_700_000_040_000,
              created_at: 1_700_000_010_000,
              updated_at: 1_700_000_040_000,
            },
          ],
          next: null,
        },
      });
    }
    if (path === `/api/repos/team/project/commits/${head}/check-runs/7`)
      return route.fulfill({
        json: {
          id: 7,
          head_sha: head,
          name: "ci/test",
          status: "completed",
          conclusion: "success",
          details_url: "https://ci.example.test/runs/7",
          output_title: "All tests passed",
          author: "CI service",
          version: 3,
          started_at: 1_700_000_020_000,
          completed_at: 1_700_000_040_000,
          created_at: 1_700_000_010_000,
          updated_at: 1_700_000_040_000,
          output: {
            title: "All tests passed",
            summary: "The **required test suite** passed.",
            text: "No failures were reported.",
            annotations: [
              {
                path: "src/lib.rs",
                start_line: 42,
                end_line: 44,
                level: "warning",
                title: "Slow assertion",
                message: "This assertion took longer than expected.",
              },
            ],
            steps: [
              {
                name: "Build and test",
                status: "completed",
                conclusion: "success",
                log: "44 passed; 0 failed\n<script>alert(1)</script>\n",
              },
            ],
          },
        },
      });
    if (path === "/api/repos/team/project/pulls") {
      if (request.method() === "POST") {
        created = true;
        return route.fulfill({ status: 201, json: pull() });
      }
      const query = url.searchParams.get("q")?.toLowerCase();
      const item = pull();
      const matches =
        !query ||
        [item.title, item.body, item.author].some((value) =>
          value.toLowerCase().includes(query),
        );
      return route.fulfill({
        json: { items: matches ? [item] : [], next: null },
      });
    }
    if (/\/pulls\/\d+$/.test(path)) {
      if (request.method() === "PATCH") {
        state = "closed";
        return route.fulfill({ json: pull() });
      }
      return route.fulfill({ json: pull() });
    }
    if (/\/pulls\/\d+\/comments$/.test(path)) {
      if (request.method() === "POST") {
        comments.push({
          number: 1,
          author: "Alice",
          body: "Verified in the browser.",
          version: 1,
          created_at: 1_700_000_100_000,
          updated_at: 1_700_000_100_000,
          can_edit: true,
        });
        return route.fulfill({ status: 201, json: comments[0] });
      }
      return route.fulfill({ json: { items: comments, next: null } });
    }
    if (/\/pulls\/\d+\/threads$/.test(path)) {
      if (request.method() === "POST") {
        const input = request.postDataJSON();
        const thread = {
          number: threads.length + 1,
          pull: 2,
          path: "README.md",
          path_hex: input.path_hex,
          side: input.side,
          start_line: input.start_line,
          end_line: input.end_line,
          author: "Alice",
          body: input.body,
          suggested_text: input.suggested_text,
          base_oid: input.base_oid,
          head_oid: input.head_oid,
          old_blob_oid: "c".repeat(40),
          new_blob_oid: "d".repeat(40),
          resolved: false,
          outdated: false,
          vanished: false,
          resolver: null,
          current: true,
          version: 1,
          created_at: 1_700_000_070_000,
          updated_at: 1_700_000_070_000,
          can_edit: true,
          can_reply: true,
          can_resolve: true,
        };
        threads.push(thread);
        return route.fulfill({ status: 201, json: thread });
      }
      const outdated = url.searchParams.get("outdated") === "true";
      return route.fulfill({
        json: {
          items: threads.filter(
            (thread) => Boolean(thread.outdated) === outdated,
          ),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/threads\/\d+\/replies$/.test(path)) {
      if (request.method() === "POST") {
        const input = request.postDataJSON();
        const reply = {
          number: replies.length + 1,
          pull: 2,
          thread: Number(path.match(/threads\/(\d+)\/replies$/)?.[1] ?? 1),
          author: "Alice",
          body: input.body,
          version: 1,
          created_at: 1_700_000_080_000,
          updated_at: 1_700_000_080_000,
          can_edit: true,
        };
        replies.push(reply);
        return route.fulfill({ status: 201, json: reply });
      }
      const thread = Number(path.match(/threads\/(\d+)\/replies$/)?.[1] ?? 1);
      return route.fulfill({
        json: {
          items: replies.filter((reply) => reply.thread === thread),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/threads\/\d+$/.test(path)) {
      const number = Number(path.match(/threads\/(\d+)$/)?.[1] ?? 1);
      const thread = threads.find((item) => item.number === number);
      if (!thread) return route.fulfill({ status: 404 });
      if (request.method() === "PATCH") {
        const input = request.postDataJSON();
        if (typeof input.resolved === "boolean")
          thread.resolved = input.resolved;
        thread.version = Number(thread.version) + 1;
        return route.fulfill({ json: thread });
      }
      return route.fulfill({ json: thread });
    }
    if (/\/pulls\/\d+\/reviews$/.test(path)) {
      if (request.method() === "POST") {
        checkState = "success";
        reviews.push({
          number: 1,
          author: "Bob",
          body: "Ready to merge.",
          state: "approved",
          commit_oid: head,
          current: branchesAvailable,
          version: 1,
          created_at: 1_700_000_050_000,
          updated_at: 1_700_000_050_000,
          can_edit: true,
        });
        return route.fulfill({ status: 201, json: reviews[0] });
      }
      return route.fulfill({
        json: {
          items: reviews.map((review) => ({
            ...review,
            current: branchesAvailable,
          })),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/merge$/.test(path)) {
      const input = request.postDataJSON();
      if (!mergePending) {
        mergePending = true;
        mergeRequest = input.request_id;
        mergeMethod = input.method;
        mergeMessage = input.message;
        expect(input.method).toBe("merge_commit");
        expect(input.message).toBe("Merge pull request #2 from feature/docs");
        return route.fulfill({
          status: 503,
          json: {
            error: {
              message:
                "The merge may have completed. Reload the pull request before retrying the same submission",
            },
          },
        });
      }
      expect(input.request_id).toBe(mergeRequest);
      mergePending = false;
      state = "merged";
      return route.fulfill({ json: pull() });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  await page.goto("/team/project?view=pulls");
  await expect(
    page.getByRole("link", { name: "Improve the README", exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  const search = page.getByRole("search", { name: "Search pulls" });
  await search.getByRole("textbox").fill("missing workflow");
  await search.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/q=missing\+workflow/);
  await expect(
    page.getByRole("heading", {
      name: "No pulls match “missing workflow”",
    }),
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
  const pullForm = page.locator(".pull-form");
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
  await expect(page).toHaveURL(/pull=2/);
  await expect(
    page.getByRole("heading", { name: /Document the feature #2/ }),
  ).toBeVisible();
  await expect(page.locator(".pull-summary")).toContainText(
    "feature/docs into main",
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
    page.getByRole("button", { name: "Open review thread #1", exact: true }),
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
  await expectNoAccessibilityViolations(page);

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
  await page
    .getByRole("button", { name: "Close pull request", exact: true })
    .click();
  await expect(page.locator(".pull-state")).toHaveText("Closed");

  branchesAvailable = false;
  await page.reload();
  await expect(
    page.getByText(/original commit IDs remain recorded/),
  ).toBeVisible();
  await expect(page.locator(".pull-conversation")).toContainText(
    "Verified in the browser.",
  );
  await expect(page.locator(".review-event")).toContainText("Outdated");

  branchesAvailable = true;
  state = "open";
  await page.reload();
  await page
    .getByRole("button", { name: "Merge pull request", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "The merge may have completed",
  );
  branchesAvailable = false;
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

test("large pull review workspace keeps multi-file navigation and lazy diffs stable", async ({
  page,
}) => {
  const base = "1".repeat(40);
  const head = "2".repeat(40);
  const changes = Array.from({ length: 64 }, (_, index) => {
    const path = `src/features/file-${String(index).padStart(2, "0")}.txt`;
    const path_hex = encodePath(path);
    const old_oid = `${(index % 8) + 3}`.repeat(40);
    const new_oid = `${(index % 8) + 9}`.repeat(40);
    return {
      path,
      path_hex,
      kind: "Modified",
      old: { path, path_hex, kind: "Blob", oid: old_oid, mode: "100644" },
      new: { path, path_hex, kind: "Blob", oid: new_oid, mode: "100644" },
      old_oid,
      new_oid,
    };
  });
  const thread = (
    number: number,
    change: (typeof changes)[number],
    outdated = false,
  ) => ({
    number,
    pull: 2,
    path: change.path,
    path_hex: change.path_hex,
    side: "new",
    start_line: 1,
    end_line: 1,
    author: `Reviewer ${number}`,
    body: `Review note ${number} for ${change.path}`,
    suggested_text: null,
    base_oid: outdated ? "3".repeat(40) : base,
    head_oid: outdated ? "4".repeat(40) : head,
    old_blob_oid: change.old_oid,
    new_blob_oid: change.new_oid,
    resolved: !outdated && number % 5 === 0,
    outdated,
    vanished: false,
    resolved_by: null,
    current: !outdated,
    version: 1,
    created_at: 1_700_000_060_000 + number,
    updated_at: 1_700_000_060_000 + number,
    can_edit: true,
    can_reply: true,
    can_resolve: true,
  });
  const activeThreads = Array.from({ length: 36 }, (_, index) =>
    thread(index + 1, changes[index % 12]),
  );
  const outdatedThreads = Array.from({ length: 4 }, (_, index) =>
    thread(100 + index, changes[index], true),
  );
  const diffRequests = new Set<string>();
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A large repository fixture.",
              access: "write",
              can_admin: false,
              archive_version: 0,
              archived: false,
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
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid: base },
            { name: "refs/heads/feature/review", oid: head },
          ],
          generation: 2,
        },
      });
    if (path.endsWith("/changes"))
      return route.fulfill({
        json: {
          base,
          commit: head,
          changes: changes.map(
            ({ old_oid: _old, new_oid: _new, ...change }) => change,
          ),
        },
      });
    if (path.endsWith("/diff")) {
      const path_hex = url.searchParams.get("path_hex") ?? "";
      const change = changes.find((item) => item.path_hex === path_hex);
      if (!change) return route.fulfill({ status: 404 });
      diffRequests.add(path_hex);
      return route.fulfill({
        json: {
          base,
          commit: head,
          path: change.path,
          old: {
            oid: change.old_oid,
            size: 20,
            mode: "100644",
            classification: "OrdinaryGit",
            text: `old ${change.path}\n`,
          },
          new: {
            oid: change.new_oid,
            size: 20,
            mode: "100644",
            classification: "OrdinaryGit",
            text: `new ${change.path}\n`,
          },
        },
      });
    }
    if (path === "/api/repos/team/project/pulls/2")
      return route.fulfill({
        json: {
          number: 2,
          title: "Review many files",
          body: "A large pull request fixture.",
          state: "open",
          author: "Alice",
          base_ref: "refs/heads/main",
          head_ref: "refs/heads/feature/review",
          base_oid: base,
          head_oid: head,
          original_base_oid: base,
          original_head_oid: head,
          version: 1,
          created_at: 1_700_000_000_000,
          updated_at: 1_700_000_000_000,
          labels: [],
          assignees: [],
          can_edit: true,
          can_label: false,
          can_assign: false,
          can_manage: true,
          can_decide: true,
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
    if (/\/pulls\/2\/threads$/.test(path)) {
      const isOutdated = url.searchParams.get("outdated") === "true";
      const source = isOutdated ? outdatedThreads : activeThreads;
      const before = Number(url.searchParams.get("before"));
      const maximum =
        Number.isSafeInteger(before) && before > 0 ? before - 1 : Infinity;
      const items = source
        .filter((item) => item.number <= maximum)
        .sort((left, right) => right.number - left.number)
        .slice(0, 30);
      return route.fulfill({
        json: {
          items,
          next: items.length === 30 ? items[items.length - 1].number : null,
        },
      });
    }
    if (/\/pulls\/2\/threads\/\d+\/replies$/.test(path))
      return route.fulfill({ json: { items: [], next: null } });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Large review fixture route unavailable" } },
    });
  });

  await page.goto("/team/project?view=pulls&pull=2&pull_tab=files");
  await selectDarkTheme(page);
  await expect(
    page.getByRole("heading", { name: "64 changed files" }),
  ).toBeVisible();
  await expect(page.locator(".change-tree-header")).toContainText("64 files");
  await expect(page.locator(".inline-review-metrics")).toHaveAttribute(
    "aria-label",
    "24 open, 6 resolved, 4 outdated",
  );

  const tree = page.locator('file-tree-container[aria-label="Changed files"]');
  const firstFile = tree.getByRole("treeitem", {
    name: "file-00.txt",
    exact: true,
  });
  await expect(firstFile).toContainText("3 threads");
  await expect.poll(() => diffRequests.size).toBeGreaterThan(0);
  expect(diffRequests.size).toBeLessThan(changes.length / 2);

  await page
    .getByRole("button", { name: "Load more threads", exact: true })
    .click();
  await expect(page.locator(".inline-review-metrics")).toHaveAttribute(
    "aria-label",
    "29 open, 7 resolved, 4 outdated",
  );
  await expect(firstFile).toContainText("4 threads");

  const treeScroll = tree.locator('[data-file-tree-virtualized-scroll="true"]');
  await treeScroll.evaluate((node) => {
    node.scrollTop = node.scrollHeight;
  });
  const lastFile = tree.getByRole("treeitem", {
    name: "file-63.txt",
    exact: true,
  });
  await expect(lastFile).toBeVisible();
  await lastFile.click();
  await expect(lastFile).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => diffRequests.has(changes[63].path_hex)).toBe(true);
  await expect(
    page.locator(`[data-change-path="${changes[63].path}"]`),
  ).toContainText(`new ${changes[63].path}`);

  await treeScroll.evaluate((node) => {
    node.scrollTop = 0;
  });
  const reviewedFile = tree.getByRole("treeitem", {
    name: "file-00.txt",
    exact: true,
  });
  await expect(reviewedFile).toBeVisible();
  await reviewedFile.click();
  await expect(
    page.getByRole("button", { name: "Open review thread #1", exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
});

import { expect, test } from "@playwright/test";

const base = "a".repeat(40);
const head = "b".repeat(40);
const pathHex = "README.md"
  .split("")
  .map((character) => character.charCodeAt(0).toString(16).padStart(2, "0"))
  .join("");

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
  const search = page.getByRole("search", { name: "Search pull requests" });
  await search.getByRole("textbox").fill("missing workflow");
  await search.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/q=missing\+workflow/);
  await expect(
    page.getByRole("heading", {
      name: "No pull requests match “missing workflow”",
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
  await expect(page.getByLabel("base:")).toHaveValue("refs/heads/main");
  await expect(page.getByLabel("compare:")).toHaveValue(
    "refs/heads/feature/docs",
  );
  await page.getByLabel("Title", { exact: true }).fill("Document the feature");
  await page
    .getByRole("textbox", { name: "Description", exact: true })
    .fill("This explains the **new behavior**.");
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

  await page.getByRole("link", { name: "Files changed", exact: true }).click();
  await expect(page.getByText("1 changed file", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: /Modified README.md/ }).click();
  await expect(page.locator(".diff-panel")).toContainText("New content");

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
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
});

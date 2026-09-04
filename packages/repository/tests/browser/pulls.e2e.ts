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
  const comments: Array<Record<string, unknown>> = [];
  const reviews: Array<Record<string, unknown>> = [];
  const pull = () => ({
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
    can_merge: state === "open" && (branchesAvailable || mergePending),
    branches_available: state === "merged" || branchesAvailable,
    merge:
      state === "merged"
        ? {
            author: "Local operator",
            method: "fast_forward",
            commit_oid: head,
            created_at: 1_700_000_200_000,
          }
        : null,
    merge_pending: mergePending
      ? {
          request_id: mergeRequest,
          author: "Local operator",
          method: "fast_forward",
          pull_version: 1,
          base_oid: base,
          head_oid: head,
          created_at: 1_700_000_150_000,
        }
      : null,
  });
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
              protected_branches: ["main"],
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
    if (path === "/api/repos/team/project/pulls") {
      if (request.method() === "POST") {
        created = true;
        return route.fulfill({ status: 201, json: pull() });
      }
      return route.fulfill({ json: { items: [pull()], next: null } });
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
    "Local operator fast-forwarded commit",
  );

  await page.setViewportSize({ width: 360, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
});

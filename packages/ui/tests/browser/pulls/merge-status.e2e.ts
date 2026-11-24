import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";
import type { PullRequest } from "../../../src/features/pull-requests/model";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

test("required checks show each same-name report and the commit they must pass on", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const waiting = {
    context: "ci/test",
    head_sha: pull.head_oid,
    state: null,
    description: null,
    target_url: null,
    author: null,
    updated_at: null,
    run_id: null,
    provider: "crab_status_or_check_run",
  } as const;
  let check: PullRequest["merge_requirements"]["checks"][number] = waiting;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => {
      const passed = check.state === "success";
      return route.fulfill({
        json: {
          ...pull,
          can_merge: passed,
          merge_requirements: {
            ...pull.merge_requirements,
            required_approvals: 0,
            checks: [check],
            checks_satisfied: passed,
            satisfied: passed,
          },
        },
      });
    },
  );

  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  const checks = panel.locator(".required-checks");
  const refresh = panel.getByRole("button", { name: "Refresh checks" });
  await expect(checks).toContainText("Crab commit status or check run");
  await expect(
    checks.getByRole("link", { name: pull.head_oid.slice(0, 7) }),
  ).toHaveAttribute("href", `/team/project/commit/${pull.head_oid}`);

  check = {
    ...waiting,
    state: "failure",
    provider: "crab_status",
    author: "CI gateway",
    description: "Integration environment failed",
    target_url: "https://ci.example.test/status/42",
    additional_report: {
      ...waiting,
      state: "success",
      provider: "crab_check_run",
      author: "Test runner",
      conclusion: "neutral",
      run_id: 7,
    },
  };
  await refresh.click();
  const status = checks
    .getByRole("listitem")
    .filter({ hasText: "Crab commit status" });
  const run = checks
    .getByRole("listitem")
    .filter({ hasText: "Crab check run" });
  await expect(status).toContainText("Failed");
  await expect(status.getByRole("link", { name: "Details" })).toHaveAttribute(
    "href",
    "https://ci.example.test/status/42",
  );
  await expect(run).toContainText("Passed · neutral");
  await expect(run.getByRole("link", { name: "Details" })).toHaveAttribute(
    "href",
    "/team/project/pull/1/checks?check=7",
  );
  await expect(
    panel.getByRole("button", { name: "Merge pull request" }),
  ).toHaveCount(0);

  check = {
    ...check,
    state: "success",
    description: "Integration environment passed",
  };
  await refresh.click();
  await expect(checks).toContainText("All required checks have passed");
  await expect(
    panel.getByRole("button", { name: "Merge pull request" }),
  ).toBeVisible();
});

test("merge guidance follows current eligibility and protection despite a stale repository catalog", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const state = { ready: true, pending: false, protected: false };
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...fixture.pull(),
          can_merge: false,
          can_manage: false,
          merge_pending: state.pending
            ? {
                request_id: "recorded-merge",
                author: { name: "Alice", profile: null },
                method: "merge_commit",
                pull_version: 1,
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
                message: "Original merge",
                created_at: 1700000000000,
              }
            : null,
          merge_methods: ["merge_commit", "squash", "rebase", "fast_forward"],
          merge_requirements: {
            protected: state.protected,
            rulesets: state.protected ? ["Default branch reviews"] : [],
            required_approvals: state.ready ? 0 : 1,
            approvals: 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: [],
            satisfied: state.ready,
          },
        },
      }),
  );
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  await expect(panel).toContainText("Merge requirements are satisfied");
  await expect(panel).toContainText("Write access is required to merge.");
  await expect(panel.locator(".protected-branch-note")).toHaveCount(0);
  await expect(
    panel.getByRole("button", {
      name: /Merge pull request|Confirm merge|Retry merge/,
    }),
  ).toHaveCount(0);
  await expectNoAccessibilityViolations(page);

  state.ready = false;
  state.protected = true;
  await page.reload();
  await expect(panel).toContainText("Merging is blocked");
  await expect(panel).toContainText("1 more approving review is required.");
  await expect(panel).toContainText(
    "The requirements above must be satisfied before this pull request can be merged.",
  );
  await expect(panel.locator(".protected-branch-note")).toContainText(
    "main is protected.",
  );

  await expect(panel).toContainText("Default branch reviews");
  await expect(
    panel.getByRole("link", { name: "Applied rulesets" }),
  ).toHaveAttribute("href", "/team/project/rulesets");

  state.pending = true;
  await page.reload();
  await expect(panel).toContainText(
    "Write access is required to recover the original merge.",
  );
  await expect(panel.getByRole("button", { name: "Retry merge" })).toHaveCount(
    0,
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

test("repository merge methods govern the picker and a pending rebase retains its original request", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  let enabled = ["merge_commit", "squash", "rebase", "fast_forward"];
  let policyVersion = 0;
  let queueMethod = "merge_commit";
  let queueBuilds = 1;
  let branchQueues: unknown[] = [];
  let request:
    | {
        request_id: string;
        version: number;
        base_oid: string;
        head_oid: string;
        message: string;
        method: string;
      }
    | undefined;
  let merged = false;
  const root = "/api/repos/team/project";
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: fixture.lifecycle.repositoryId,
              owner: "team",
              name: "project",
              description: "",
              access: "write",
              can_admin: true,
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/settings/merge-methods`,
    (route) => {
      if (route.request().method() === "PUT") {
        expect(route.request().headers()["x-crab-repository-id"]).toBe(
          fixture.lifecycle.repositoryId,
        );
        const body = route.request().postDataJSON();
        expect(body).toEqual({
          expected_version: 0,
          enabled: ["squash", "rebase"],
          merge_queue_method: "rebase",
          merge_queue_builds: 3,
          merge_queue_branches: [
            { branch: "release/stable", method: "squash", timeout_minutes: 30 },
          ],
        });
        enabled = body.enabled;
        queueMethod = body.merge_queue_method;
        queueBuilds = body.merge_queue_builds;
        branchQueues = body.merge_queue_branches;
        policyVersion += 1;
      }
      return route.fulfill({
        json: {
          version: policyVersion,
          enabled,
          merge_queue_method: queueMethod,
          merge_queue_builds: queueBuilds,
          merge_queue_branches: branchQueues,
        },
      });
    },
  );
  function pull() {
    const current = fixture.pull();
    const author = { name: "Alice", profile: null };
    return {
      ...current,
      state: merged ? "merged" : "open",
      can_merge: !merged,
      merge_methods: enabled,
      merged_at: merged ? 1_700_000_200_000 : null,
      merge_requirements: {
        protected: false,
        required_approvals: 0,
        approvals: 0,
        changes_requested: 0,
        checks_satisfied: true,
        checks: [{ ...current.merge_requirements.checks[0], state: "success" }],
        satisfied: true,
      },
      merge_pending:
        request && !merged
          ? {
              request_id: request.request_id,
              author,
              method: request.method,
              pull_version: request.version,
              base_oid: request.base_oid,
              head_oid: request.head_oid,
              message: request.message,
              created_at: 1_700_000_190_000,
            }
          : null,
      merge: merged
        ? {
            author,
            method: "rebase",
            commit_oid: "c".repeat(40),
            message: "",
            created_at: 1_700_000_190_000,
          }
        : null,
    };
  }
  await page.route(
    (url) => url.pathname === `${root}/pulls/1`,
    (route) => route.fulfill({ json: pull() }),
  );
  await page.route(
    (url) => url.pathname === `${root}/pulls/1/merge`,
    (route) => {
      const body = route.request().postDataJSON();
      if (!request) {
        expect(body).toMatchObject({
          method: "rebase",
          message: "",
          base_oid: "a".repeat(40),
          head_oid: "b".repeat(40),
        });
        request = body;
        enabled = ["squash"];
        return route.fulfill({
          status: 503,
          json: { error: { message: "Result unavailable" } },
        });
      }
      expect(body).toEqual(request);
      merged = true;
      return route.fulfill({ json: pull() });
    },
  );

  await page.goto("/team/project/settings");
  const settings = page.getByRole("region", {
    name: "Pull request merge methods",
  });
  await settings
    .getByRole("checkbox", { name: /^Create a merge commit/ })
    .uncheck();
  await settings
    .getByRole("checkbox", { name: /^Fast-forward only/ })
    .uncheck();
  await settings.getByLabel("Queue merge method").selectOption("rebase");
  await settings.getByLabel("Concurrent queue builds").fill("3");
  const overrides = settings.getByRole("group", {
    name: "Branch queue overrides",
  });
  await overrides.getByRole("button", { name: "Add branch override" }).click();
  await overrides.getByLabel("Branch name").fill("release/stable");
  await overrides.getByLabel("Branch queue method").selectOption("squash");
  await overrides.getByLabel("Branch check timeout (minutes)").fill("30");
  await settings.getByRole("button", { name: "Save merge methods" }).click();
  await expect(
    settings.getByRole("button", { name: "Save merge methods" }),
  ).toBeDisabled();
  await expect(
    settings.getByRole("checkbox", { name: /^Squash and merge/ }),
  ).toBeChecked();
  await expect(settings.getByLabel("Queue merge method")).toHaveValue("rebase");
  await expect(overrides.getByLabel("Branch name")).toHaveValue(
    "release/stable",
  );
  await expect(overrides.getByLabel("Branch queue method")).toHaveValue(
    "squash",
  );

  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  await panel
    .getByRole("button", { name: "Squash and merge", exact: true })
    .click();
  await expect(
    panel.getByRole("textbox", { name: "Squash commit message" }),
  ).toHaveValue("Improve the README (#1)");
  enabled = ["rebase"];
  await panel.getByRole("button", { name: "Refresh checks" }).click();
  await expect(
    panel.getByRole("button", { name: "Confirm merge" }),
  ).toBeDisabled();
  await expect(
    panel.getByText(/merge options or branch tips changed/),
  ).toBeVisible();
  await panel.getByRole("button", { name: "Cancel", exact: true }).click();
  enabled = ["squash", "rebase"];
  await panel.getByRole("button", { name: "Refresh checks" }).click();
  await panel.getByRole("button", { name: "Select merge method" }).click();
  await expect(
    page.getByRole("menuitemradio", { name: /^Create a merge commit/ }),
  ).toHaveCount(0);
  await page.getByRole("menuitemradio", { name: /^Rebase and merge/ }).click();
  await panel
    .getByRole("button", { name: "Rebase and merge", exact: true })
    .click();
  await expect(
    panel.getByRole("textbox", { name: /commit message/ }),
  ).toHaveCount(0);
  await panel.getByRole("button", { name: "Confirm merge" }).click();
  await expect(panel.getByRole("alert")).toBeVisible();
  await page.reload();
  await panel.getByRole("button", { name: "Retry merge" }).click();
  await expect(panel).toContainText("rebased changes through");
});

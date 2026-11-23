import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "../accessibility";
import { mockPullRequest } from "./fixture";
import type { BranchProtection } from "@/lib/contracts";

test("branch protection displays the code owner evidence that gates a pull request", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const root = "/api/repos/team/project";
  let rules: BranchProtection[] = [];
  let approved = false;
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
              protection_version: rules.length,
              protected_branches: rules,
            },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/settings/branch-protections`,
    (route) => {
      if (route.request().method() === "PUT") {
        expect(route.request().headers()["x-crab-repository-id"]).toBe(
          fixture.lifecycle.repositoryId,
        );
        const body = route.request().postDataJSON();
        expect(body).toEqual({
          expected_version: 0,
          rules: [
            {
              branch: "main",
              required_approvals: 0,
              required_checks: [],
              required_code_owner_reviews: true,
            },
          ],
        });
        rules = body.rules;
      }
      return route.fulfill({ json: { version: rules.length, rules } });
    },
  );
  const report = () => ({
    path: ".github/CODEOWNERS",
    base_oid: fixture.pull().base_oid,
    head_oid: fixture.pull().head_oid,
    invalid_lines: 0,
    problem: null,
    affected_paths: 1,
    missing_paths: approved ? 0 : 1,
    groups: [
      {
        line: 4,
        owners: ["@release-owner"],
        paths: ["README.md"],
        affected: 1,
        approved,
        has_eligible_owner: true,
      },
    ],
  });
  await page.route(
    (url) => url.pathname === `${root}/pulls/1`,
    (route) => {
      const pull = fixture.pull();
      return route.fulfill({
        json: {
          ...pull,
          can_merge: approved,
          merge_requirements: {
            protected: true,
            required_approvals: 0,
            approvals: approved ? 1 : 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: [],
            satisfied: approved,
            code_owners: report(),
          },
        },
      });
    },
  );

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/settings/branches");
  await page
    .getByRole("button", { name: "Add branch protection rule" })
    .click();
  await page.getByLabel("Branch name").fill("main");
  await page
    .getByRole("checkbox", { name: "Require review from code owners" })
    .check();
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(
    page.getByText("Code owner approval required", { exact: true }),
  ).toBeVisible();
  await page.reload();
  await expect(
    page.getByText("Code owner approval required", { exact: true }),
  ).toBeVisible();

  await page.goto("/team/project/pull/1");
  const merge = page.getByRole("region", { name: "Merge status" });
  await expect(merge).toContainText("Code owner approval required for 1 path");
  await expect(merge).toContainText("@release-owner");
  await expect(merge).toContainText("README.md");
  await expect(
    merge.getByRole("link", { name: "View base branch CODEOWNERS" }),
  ).toHaveAttribute(
    "href",
    `/team/project/blob/${fixture.pull().base_oid}/.github/CODEOWNERS`,
  );
  await expectNoAccessibilityViolations(page);
  approved = true;
  await page.reload();
  await expect(merge).toContainText("Code owner approvals complete");
  await expect(
    merge.getByRole("button", { name: "Merge pull request" }),
  ).toBeEnabled();
});

test("Files changed uses ownership from the same inspected base and head", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  let stale = true;
  const path = "/api/repos/team/project/pulls/1/codeowners";
  await page.route(
    (url) => url.pathname === path,
    (route) => {
      const requested = new URL(route.request().url());
      expect(requested.searchParams.get("base")).toBe(pull.base_oid);
      expect(requested.searchParams.get("head")).toBe(pull.head_oid);
      return route.fulfill({
        json: {
          path: "docs/CODEOWNERS",
          base_oid: stale ? "f".repeat(40) : pull.base_oid,
          head_oid: pull.head_oid,
          invalid_lines: 0,
          problem: null,
          affected_paths: 1,
          missing_paths: 1,
          groups: [
            {
              line: 3,
              owners: ["@release-owner"],
              paths: ["README.md"],
              affected: 1,
              approved: false,
              has_eligible_owner: true,
            },
          ],
        },
      });
    },
  );
  const result = page.waitForResponse(
    (response) => new URL(response.url()).pathname === path,
  );
  await page.goto("/team/project/pull/1/files");
  await result;
  const source = page.getByRole("link", {
    name: "View base branch CODEOWNERS",
  });
  await expect(source).toHaveCount(0);
  stale = false;
  await page.reload();
  await expect(source).toHaveAttribute(
    "href",
    `/team/project/blob/${pull.base_oid}/docs/CODEOWNERS`,
  );
  await page.getByText("Code owner requirements", { exact: true }).click();
  await expect(page.getByText("@release-owner", { exact: true })).toBeVisible();
});

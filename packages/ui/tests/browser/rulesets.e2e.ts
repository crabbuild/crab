import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("repository admins manage rulesets and readers see active rules", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  let admin = true;
  let version = 0;
  let rulesets: {
    id: string;
    name: string;
    enforcement: string;
    bypass_actors?: unknown;
    review_dismissal?: unknown;
  }[] = [];
  const releaseTeam = "00000000-0000-4000-8000-0000000000aa";
  const releaseReviewer = "00000000-0000-4000-8000-0000000000bb";
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/assignees",
    (route) =>
      route.fulfill({
        json: {
          items: [
            { id: releaseReviewer, name: "Release reviewer", profile: null },
          ],
          can_manage: true,
        },
      }),
  );
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "local",
        user: null,
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: lifecycle.repositoryId,
              owner: "team",
              name: "project",
              description: "",
              can_admin: admin,
              access: admin ? "write" : "read",
              archived: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname.endsWith("/settings/integrations"),
    (route) => route.fulfill({ json: { available: true, items: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/rulesets",
    async (route) => {
      const request = route.request();
      if (request.method() === "PUT") {
        expect(admin).toBe(true);
        expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
        expect(request.headers()["x-crab-repository-id"]).toBe(
          lifecycle.repositoryId,
        );
        const payload = request.postDataJSON();
        expect(payload.expected_version).toBe(version);
        rulesets = payload.rulesets;
        version += 1;
      }
      return route.fulfill({
        json: {
          version,
          can_manage: admin,
          bypass_options: admin
            ? {
                organization_owner: true,
                teams: [{ id: releaseTeam, name: "Release Engineering" }],
              }
            : undefined,
          rulesets: admin
            ? rulesets
            : rulesets
                .filter((rule) => rule.enforcement === "active")
                .map((rule) => ({
                  ...rule,
                  bypass_actors: undefined,
                  review_dismissal: rule.review_dismissal ? [] : undefined,
                })),
        },
      });
    },
  );

  await page.goto("/team/project/settings/rulesets");
  await page.getByRole("button", { name: "New ruleset" }).click();
  await page.getByLabel("Name", { exact: true }).fill("Release branches");
  await page.getByLabel("Include patterns").fill("release/**/*");
  await page.getByLabel("Exclude patterns").fill("release/sandbox");
  await page.getByLabel("Required approving reviews").fill("2");
  await page
    .getByLabel("Restrict who can dismiss pull request reviews")
    .check();
  await page
    .getByLabel("Allow review dismissal for")
    .selectOption(`user:${releaseReviewer}`);
  await page
    .getByLabel("Allow review dismissal for")
    .selectOption(`team:${releaseTeam}`);
  await page.getByLabel("Restrict updates", { exact: true }).check();
  await page.getByLabel("Require linear history", { exact: true }).check();
  await page.getByLabel("Restrict deletions").check();
  await page.getByLabel("Require conversation resolution").check();
  const strict = page.getByLabel(
    "Require branches to be up to date before merging",
  );
  await expect(strict).toBeDisabled();
  const queue = page.getByLabel("Require merge queue", { exact: true });
  await expect(queue).toBeDisabled();
  await page.getByLabel("Required check names").fill("ci/release");
  await strict.check();
  await queue.check();
  await page.getByLabel("Add bypass actor").selectOption(releaseTeam);
  await page.getByLabel("Add bypass actor").selectOption("repository_admin");
  await page
    .getByLabel("Bypass mode for Repository administrators")
    .selectOption("always");
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save ruleset" }).click();
  await expect(
    page.getByRole("heading", { name: "Release branches" }),
  ).toBeVisible();
  expect(rulesets[0]).toMatchObject({
    name: "Release branches",
    target: "branch",
    enforcement: "active",
    include: ["release/**/*"],
    exclude: ["release/sandbox"],
    restrict_deletions: true,
    restrict_updates: true,
    require_linear_history: true,
    require_conversation_resolution: true,
    require_up_to_date: true,
    require_merge_queue: true,
    required_checks: ["ci/release"],
    required_approvals: 2,
    review_dismissal: [
      { kind: "user", id: releaseReviewer },
      { kind: "team", id: releaseTeam },
    ],
    bypass_actors: [
      { actor: { kind: "team", id: releaseTeam }, mode: "pull_request" },
      { actor: { kind: "repository_admin" }, mode: "always" },
    ],
  });

  admin = false;
  await page.goto("/team/project/branches");
  await page.getByRole("link", { name: "Rulesets" }).click();
  await expect(page).toHaveURL(/\/team\/project\/rulesets$/);
  await expect(
    page.getByRole("heading", { name: "Release branches" }),
  ).toBeVisible();
  await expect(page.getByText("Require 2 approving reviews")).toBeVisible();
  await expect(
    page.getByText("Restrict review dismissal", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Require linear history", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Restrict updates", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Require merge queue", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("2 bypass grants")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "New ruleset" })).toHaveCount(
    0,
  );
  await expect(
    page.getByRole("button", { name: "Edit Release branches" }),
  ).toHaveCount(0);

  admin = true;
  await page.goto("/team/project/settings/rulesets");
  await page.getByRole("button", { name: "Edit Release branches" }).click();
  await page.getByLabel("Enforcement").selectOption("disabled");
  await page.getByRole("button", { name: "Save ruleset" }).click();
  await expect(page.getByText("Disabled", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Delete Release branches" }).click();
  await page
    .getByRole("button", { name: "Delete ruleset", exact: true })
    .click();
  await expect(page.getByText("No rulesets to display.")).toBeVisible();
});
